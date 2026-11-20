//! Opening a connection's pool: its credentials, its SSH tunnel, and each engine's
//! connect options, including what an RDS database needs (Amazon's certificate
//! authorities, and on MySQL and MariaDB the clear-text plugin for IAM tokens).
//!
//! The rest of `db` asks for a pool and gets an [`Open`]; nothing here runs queries.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlSslMode};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use super::{Open, Pool, SqlitePools};
use crate::cloud::aws;
use crate::config::{Adapter, Auth, Connection, TlsMode};
use crate::credentials::{self, Expiry};
use crate::{secrets, ssh};

/// Opens the SSH tunnel, if any, and then the connection pool.
pub(super) async fn connect(conn: &Connection) -> Result<Open> {
    // A token's life starts when it is signed, before the tunnel and pool take their time.
    let signed = SystemTime::now();
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
    let refresher =
        refresh.map(|expiry| Refresher::start(conn, &pool, (host, port), expiry, signed));
    Ok(Open {
        config: conn.clone(),
        pool,
        tunnel,
        refresher,
    })
}

/// How often a [`Refresher`] looks at the clock. Short, because a computer that slept
/// doesn't count the time asleep in Tokio's timers, but does on the clock.
const REFRESH_CHECK: Duration = Duration::from_secs(30);

/// Gives a pool a new password before the old one expires, for the connections it opens
/// from then on; those already open stay signed in. Stops when dropped.
///
/// Once the password has run out without a new one, [`Refresher::expired`] says so, and
/// `db` connects again instead of using the pool: that either signs in afresh or fails
/// with why, such as the `aws sso login` to run, rather than with the database refusing
/// an expired token.
pub(super) struct Refresher {
    task: tokio::task::JoinHandle<()>,
    renewal: Arc<Mutex<Renewal>>,
}

impl Refresher {
    /// Resolves `conn` again `expiry.renew_after` after `made`, when the password in use
    /// was made, reading fresh AWS credentials each time; each new password brings its
    /// own [`Expiry`], shorter when the credentials expire sooner. `host` and `port` are
    /// where the pool connects: the tunnel's end if there is one.
    fn start(
        conn: &Connection,
        pool: &Pool,
        (host, port): (String, u16),
        expiry: Expiry,
        made: SystemTime,
    ) -> Self {
        let (conn, pool) = (conn.clone(), pool.clone());
        let renewal = Arc::new(Mutex::new(Renewal::new(made, expiry)));
        let shared = renewal.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(REFRESH_CHECK).await;
                let now = SystemTime::now();
                if !shared.lock().unwrap().due(now) {
                    continue;
                }
                // A failure keeps the old password, which may still be valid, and tries
                // again later and later; once it has run out, `expired` reports it and
                // connecting again says why.
                match credentials::resolve(&conn).await {
                    Ok(resolved) => {
                        let password = resolved.password.as_deref();
                        let options = connect_options(&resolved.target, &host, port, password);
                        pool.set_options(options);
                        let expiry = resolved.refresh.unwrap_or(expiry);
                        *shared.lock().unwrap() = Renewal::new(now, expiry);
                    }
                    Err(_) => shared.lock().unwrap().failed(now),
                }
            }
        });
        Self { task, renewal }
    }

    /// Whether the password has run out without a new one: renewing failed, or the
    /// computer slept past it.
    pub(super) fn expired(&self) -> bool {
        let Renewal { made, expiry, .. } = *self.renewal.lock().unwrap();
        made.elapsed().is_ok_and(|age| age >= expiry.lasts)
    }
}

/// The longest a [`Refresher`] waits between failed attempts.
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);

/// How renewing a password goes: when the one in use was made and how long it is good
/// for, and the attempts that failed since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Renewal {
    made: SystemTime,
    expiry: Expiry,
    failures: u32,
    last_try: Option<SystemTime>,
}

impl Renewal {
    fn new(made: SystemTime, expiry: Expiry) -> Self {
        Self {
            made,
            expiry,
            failures: 0,
            last_try: None,
        }
    }

    /// Whether to try making a new password at `now`: once it is `renew_after` old, and
    /// after a failure only once the wait, doubling from [`REFRESH_CHECK`] up to
    /// [`RETRY_MAX`], has passed.
    fn due(&self, now: SystemTime) -> bool {
        if now
            .duration_since(self.made)
            .is_ok_and(|age| age < self.expiry.renew_after)
        {
            return false;
        }
        let Some(last) = self.last_try else {
            return true;
        };
        let wait = REFRESH_CHECK
            .saturating_mul(1 << self.failures.saturating_sub(1).min(10))
            .min(RETRY_MAX);
        now.duration_since(last).is_ok_and(|since| since >= wait)
    }

    fn failed(&mut self, at: SystemTime) {
        self.failures += 1;
        self.last_try = Some(at);
    }
}

impl Drop for Refresher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What a pool opens each of its connections with.
enum Options {
    Pg(PgConnectOptions),
    My(MySqlConnectOptions),
    Sqlite(SqliteConnectOptions),
}

/// Certificate authorities to trust, besides the usual ones, when verifying `conn`'s
/// certificate: Amazon's, for a database at an RDS host name. Decided by the host alone,
/// not by where the connection was found: RDS certificates name only RDS hosts, and the
/// host may come from a secret.
fn extra_roots(conn: &Connection) -> Option<&'static [u8]> {
    (conn.tls == TlsMode::Verify && aws::is_rds_host(&conn.host)).then_some(aws::RDS_CA_BUNDLE)
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
    // No other password may be sent that way, and never without encryption.
    if matches!(conn.auth, Auth::AwsIam(_)) && conn.tls != TlsMode::Prefer {
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
            (Pool::Sqlite(p), Options::Sqlite(o)) => {
                p.read_only.set_connect_options(o.clone().read_only(true));
                p.main.set_connect_options(o);
            }
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
        Options::Sqlite(opts) => {
            let pool = || {
                SqlitePoolOptions::new()
                    .max_connections(4)
                    .acquire_timeout(timeout)
            };
            Pool::Sqlite(SqlitePools {
                read_only: pool().connect_lazy_with(opts.clone().read_only(true)),
                main: pool().connect_with(opts).await?,
            })
        }
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
    fn the_clear_text_plugin_is_only_for_iam_over_encryption() {
        let cleartext = |auth: Auth, tls: TlsMode| {
            let mut conn = rds(tls);
            conn.adapter = Adapter::Mysql;
            conn.auth = auth;
            let options = mysql_options(&conn, "127.0.0.1", 3306, Some("x"));
            format!("{options:?}").contains("enable_cleartext_plugin: true")
        };
        let iam = || {
            Auth::AwsIam(crate::config::AwsIamAuth {
                profile: String::new(),
                region: "eu-west-1".into(),
            })
        };
        assert!(cleartext(iam(), TlsMode::Require));
        assert!(cleartext(iam(), TlsMode::Verify));
        assert!(!cleartext(iam(), TlsMode::Prefer));
        assert!(!cleartext(Auth::Password, TlsMode::Require));
    }

    #[test]
    fn amazons_authorities_are_trusted_only_to_verify_an_rds_database() {
        assert!(extra_roots(&rds(TlsMode::Verify)).is_some());
        assert!(extra_roots(&rds(TlsMode::Require)).is_none());
        let mut elsewhere = rds(TlsMode::Verify);
        elsewhere.host = "db.example.com".into();
        assert!(extra_roots(&elsewhere).is_none());
        // Not even for a connection found in AWS, whose secret may name another host.
        elsewhere.source = Some(crate::config::Source {
            provider: crate::config::Provider::Aws,
            resource: "arn:aws:rds:eu-west-1:123456789012:db:orders".into(),
            region: "eu-west-1".into(),
        });
        assert!(extra_roots(&elsewhere).is_none());
        let mut china = rds(TlsMode::Verify);
        china.host = "orders.abc.cn-north-1.rds.amazonaws.com.cn".into();
        assert!(extra_roots(&china).is_some());
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

    #[tokio::test]
    async fn an_expired_password_makes_the_pool_unusable_so_connecting_says_why() {
        let conn = rds(TlsMode::Require);
        let Options::Pg(options) = connect_options(&conn, "127.0.0.1", 40000, None) else {
            panic!("PostgreSQL options expected");
        };
        let pool = Pool::Pg(PgPoolOptions::new().connect_lazy_with(options));
        let refresher = Refresher::start(
            &conn,
            &pool,
            ("127.0.0.1".into(), 40000),
            credentials::IAM_EXPIRY,
            SystemTime::now(),
        );
        let open = Open {
            config: conn.clone(),
            pool,
            tunnel: None,
            refresher: Some(refresher),
        };
        assert!(open.is_usable_for(&conn));
        // Renewing failed for a while, or the computer slept: the token has run out.
        let refresher = open.refresher.as_ref().unwrap();
        let minutes_ago = |m: u64| SystemTime::now() - Duration::from_secs(m * 60);
        refresher.renewal.lock().unwrap().made = minutes_ago(16);
        assert!(refresher.expired());
        assert!(!open.is_usable_for(&conn));
        // Late, but not yet run out: still usable while the next renewal is tried.
        refresher.renewal.lock().unwrap().made = minutes_ago(12);
        assert!(open.is_usable_for(&conn));
        // Unless the credentials that signed it expired sooner: then it ran out with them.
        let signed = minutes_ago(12);
        let short = credentials::iam_expiry(signed, Some(signed + Duration::from_secs(6 * 60)));
        refresher.renewal.lock().unwrap().expiry = short;
        assert!(refresher.expired());
    }

    #[test]
    fn renewing_waits_for_its_time_then_backs_off_after_failures() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let at = |secs| start + Duration::from_secs(secs);
        let mut renewal = Renewal::new(start, credentials::IAM_EXPIRY);
        assert!(!renewal.due(at(9 * 60)));
        assert!(renewal.due(at(10 * 60)));
        // Each failure doubles the wait: 30 s, 1 min, 2 min, 4 min, then 5 min at most.
        let mut now = 10 * 60;
        for wait in [30, 60, 120, 240, 300, 300] {
            renewal.failed(at(now));
            assert!(!renewal.due(at(now + wait - 1)), "{wait}");
            assert!(renewal.due(at(now + wait)), "{wait}");
            now += wait;
        }
        assert_eq!(
            renewal.made, start,
            "a failure keeps the old password's age"
        );
        // A clock set back doesn't make it try at once.
        assert!(!renewal.due(at(now - 600)));
        // Credentials that expire in 6 minutes: renewed after 4.
        let short = credentials::iam_expiry(start, Some(at(6 * 60)));
        let renewal = Renewal::new(start, short);
        assert!(!renewal.due(at(4 * 60 - 1)));
        assert!(renewal.due(at(4 * 60)));
    }
}
