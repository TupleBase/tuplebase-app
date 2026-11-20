//! Opening a connection's pool: its credentials, its SSH tunnel, and each engine's
//! connect options, including what an RDS database needs (Amazon's certificate
//! authorities, and on MySQL and MariaDB the clear-text plugin for IAM tokens).
//!
//! The rest of `db` asks for a pool and gets an [`Open`]; nothing here runs queries.

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use super::{Open, Pool};
use crate::cloud::aws;
use crate::config::{Adapter, Auth, Connection, Provider, TlsMode};
use crate::{credentials, secrets, ssh};

/// Opens the SSH tunnel, if any, and then the connection pool.
pub(super) async fn connect(conn: &Connection) -> Result<Open> {
    // What to connect to: a secret in AWS can name the user and host as well.
    let credentials::Resolved {
        target,
        password,
        refresh,
    } = credentials::resolve(conn).await?;
    let tunnel = match &conn.ssh {
        None => None,
        Some(config) => {
            let name = conn.name.clone();
            let secret =
                tokio::task::spawn_blocking(move || secrets::get_ssh_password(&name)).await??;
            let tunnel = ssh::open(&target, config, secret)
                .await
                .with_context(|| format!("opening the SSH tunnel for \"{}\"", conn.name))?;
            Some(tunnel)
        }
    };
    let (host, port) = match &tunnel {
        Some(tunnel) => ("127.0.0.1".to_owned(), tunnel.local_port()),
        None => (target.host.clone(), target.port),
    };
    let options = connect_options(&target, &host, port, password.as_deref());
    let pool = open_pool(options)
        .await
        .with_context(|| format!("connecting to \"{}\"", conn.name))?;
    let refresher = refresh.map(|every| Refresher::start(conn, &pool, host, port, every));
    Ok(Open {
        config: conn.clone(),
        pool,
        tunnel,
        _refresher: refresher,
    })
}

/// How often a [`Refresher`] looks at the clock. Short, because a computer that slept
/// doesn't count the time asleep in Tokio's timers, but does on the clock.
const REFRESH_CHECK: Duration = Duration::from_secs(30);

/// Gives a pool a new password before the old one expires, for the connections it opens
/// from then on; those already open stay signed in. Stops when dropped.
pub(super) struct Refresher(tokio::task::JoinHandle<()>);

impl Refresher {
    /// Resolves `conn` again every `every`, reading fresh AWS credentials each time.
    /// `host` and `port` are where the pool connects: the tunnel's end if there is one.
    fn start(conn: &Connection, pool: &Pool, host: String, port: u16, every: Duration) -> Self {
        let (conn, pool) = (conn.clone(), pool.clone());
        Self(tokio::spawn(async move {
            let mut made = SystemTime::now();
            loop {
                tokio::time::sleep(REFRESH_CHECK).await;
                if made.elapsed().is_ok_and(|age| age < every) {
                    continue;
                }
                // A failure keeps the old password, which may still be valid, and the
                // next check tries again.
                if let Ok(resolved) = credentials::resolve(&conn).await {
                    let password = resolved.password.as_deref();
                    pool.set_options(connect_options(&resolved.target, &host, port, password));
                    made = SystemTime::now();
                }
            }
        }))
    }
}

impl Drop for Refresher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What a pool opens each of its connections with.
enum Options {
    Pg(PgConnectOptions),
    My(MySqlConnectOptions),
    Sqlite(SqliteConnectOptions),
}

/// Certificate authorities to trust, besides the usual ones, when verifying `conn`'s
/// certificate: Amazon's, for a database in RDS.
fn extra_roots(conn: &Connection) -> Option<&'static [u8]> {
    let found_in_aws = conn
        .source
        .as_ref()
        .is_some_and(|s| s.provider == Provider::Aws);
    let rds = found_in_aws || aws::is_rds_host(&conn.host);
    (conn.tls == TlsMode::Verify && rds).then_some(aws::RDS_CA_BUNDLE)
}

/// Options to reach `conn` at `host:port`, which is the local end of the tunnel when
/// there is one.
fn connect_options(conn: &Connection, host: &str, port: u16, password: Option<&str>) -> Options {
    match conn.adapter {
        Adapter::Postgres => Options::Pg(postgres_options(conn, host, port, password)),
        Adapter::Mariadb | Adapter::Mysql => Options::My(mysql_options(conn, host, port, password)),
        Adapter::Sqlite => Options::Sqlite(sqlite_options(conn)),
    }
}

fn postgres_options(
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<&str>,
) -> PgConnectOptions {
    let mut opts = PgConnectOptions::new()
        .host(host)
        .port(port)
        .username(&conn.user)
        .ssl_mode(match conn.tls {
            TlsMode::Prefer => PgSslMode::Prefer,
            TlsMode::Require => PgSslMode::Require,
            TlsMode::Verify => PgSslMode::VerifyFull,
        });
    if !conn.database.is_empty() {
        opts = opts.database(&conn.database);
    }
    if let Some(pw) = password {
        opts = opts.password(pw);
    }
    if let Some(pem) = extra_roots(conn) {
        opts = opts.ssl_root_cert_from_pem(pem.to_vec());
    }
    opts
}

/// MySQL and MariaDB.
fn mysql_options(
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<&str>,
) -> MySqlConnectOptions {
    let mut opts = MySqlConnectOptions::new()
        .host(host)
        .port(port)
        .username(&conn.user)
        .ssl_mode(match conn.tls {
            TlsMode::Prefer => MySqlSslMode::Preferred,
            TlsMode::Require => MySqlSslMode::Required,
            TlsMode::Verify => MySqlSslMode::VerifyIdentity,
        });
    if !conn.database.is_empty() {
        opts = opts.database(&conn.database);
    }
    if let Some(pw) = password {
        opts = opts.password(pw);
    }
    if let Some(pem) = extra_roots(conn) {
        opts = opts.ssl_ca_from_pem(pem.to_vec());
    }
    // RDS asks for an IAM token in clear text, inside the encryption that IAM requires.
    // No other password may be sent that way.
    if matches!(conn.auth, Auth::AwsIam(_)) {
        opts = opts.enable_cleartext_plugin(true);
    }
    opts
}

fn sqlite_options(conn: &Connection) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(ssh::expand_home(&conn.database))
        .read_only(conn.readonly)
}

impl Pool {
    /// What new connections open with from now on. Options for another engine are
    /// ignored; they can't come from the same connection.
    fn set_options(&self, options: Options) {
        match (self, options) {
            (Pool::Pg(p), Options::Pg(o)) => p.set_connect_options(o),
            (Pool::My(p), Options::My(o)) => p.set_connect_options(o),
            (Pool::Sqlite(p), Options::Sqlite(o)) => p.set_connect_options(o),
            _ => {}
        }
    }
}

async fn open_pool(options: Options) -> Result<Pool> {
    let timeout = Duration::from_secs(10);
    Ok(match options {
        Options::Pg(opts) => Pool::Pg(
            PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(timeout)
                .connect_with(opts)
                .await?,
        ),
        Options::Sqlite(opts) => Pool::Sqlite(
            SqlitePoolOptions::new()
                .max_connections(4)
                .acquire_timeout(timeout)
                .connect_with(opts)
                .await?,
        ),
        Options::My(opts) => Pool::My(
            MySqlPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(timeout)
                .connect_with(opts)
                .await?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rds(tls: TlsMode) -> Connection {
        Connection {
            name: "orders".into(),
            folder: String::new(),
            adapter: Adapter::Postgres,
            host: "orders.abc.eu-west-1.rds.amazonaws.com".into(),
            port: 5432,
            database: "orders".into(),
            user: "app".into(),
            readonly: false,
            color: String::new(),
            tls,
            ssh: None,
            auth: Auth::Password,
            source: None,
        }
    }

    #[test]
    fn amazons_authorities_are_trusted_only_to_verify_an_rds_database() {
        assert!(extra_roots(&rds(TlsMode::Verify)).is_some());
        assert!(extra_roots(&rds(TlsMode::Require)).is_none());
        let mut elsewhere = rds(TlsMode::Verify);
        elsewhere.host = "db.example.com".into();
        assert!(extra_roots(&elsewhere).is_none());
        // A custom DNS name for a database that was found in AWS.
        elsewhere.source = Some(crate::config::Source {
            provider: Provider::Aws,
            resource: "arn:aws:rds:eu-west-1:123456789012:db:orders".into(),
            region: "eu-west-1".into(),
        });
        assert!(extra_roots(&elsewhere).is_some());
    }

    #[tokio::test]
    async fn new_options_reach_the_pool_and_a_tunnel_keeps_its_address() {
        let Options::Pg(first) = connect_options(&rds(TlsMode::Require), "127.0.0.1", 40000, None)
        else {
            panic!("PostgreSQL options expected");
        };
        assert_eq!((first.get_host(), first.get_port()), ("127.0.0.1", 40000));
        let pool = Pool::Pg(PgPoolOptions::new().connect_lazy_with(first));
        let mut renamed = rds(TlsMode::Require);
        renamed.user = "reporting".into();
        pool.set_options(connect_options(&renamed, "127.0.0.1", 40000, Some("token")));
        let Pool::Pg(pg) = &pool else { unreachable!() };
        assert_eq!(pg.connect_options().get_username(), "reporting");
        assert_eq!(pg.connect_options().get_host(), "127.0.0.1");
        // Options for another database are ignored rather than mixed in.
        pool.set_options(Options::Sqlite(SqliteConnectOptions::new()));
        assert_eq!(pg.connect_options().get_username(), "reporting");
    }
}
