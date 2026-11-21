//! Opening a connection's pool: what every engine shares. Its credentials, its SSH
//! tunnel, renewing an IAM token before it expires, Amazon's certificate authorities for
//! an RDS host, and the pool's size and timeout.
//!
//! Each engine's own connect options and pools are in its module (`postgres::connect`,
//! ...), reached through [`Pool::open`] and [`Pool::renew`], so a new engine changes
//! nothing here. The rest of `db` asks for a pool and gets an [`Open`]; nothing here runs
//! queries.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sqlx::pool::PoolOptions;

use super::{Open, Pool};
use crate::cloud::aws;
use crate::config::{Connection, TlsMode};
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
    let pool = Pool::open(&target, &host, port, password.as_deref())
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
                        pool.renew(&resolved.target, &host, port, password);
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

/// The most connections a pool opens to one database.
pub(super) const POOL_SIZE: u32 = 4;

/// How long to wait for a connection from the pool, or for a server to answer.
pub(super) const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// What every SQL engine's pool is opened with: at most [`POOL_SIZE`] connections, and
/// [`ACQUIRE_TIMEOUT`] to wait for one. The MongoDB driver takes the same two settings.
pub(super) fn pool_options<DB: sqlx::Database>() -> PoolOptions<DB> {
    PoolOptions::new()
        .max_connections(POOL_SIZE)
        .acquire_timeout(ACQUIRE_TIMEOUT)
}

/// Certificate authorities to trust, besides the usual ones, when verifying `conn`'s
/// certificate: Amazon's, for a database at an RDS host name. Decided by the host alone,
/// not by where the connection was found: RDS certificates name only RDS hosts, and the
/// host may come from a secret.
pub(super) fn extra_roots(conn: &Connection) -> Option<&'static [u8]> {
    (conn.tls == TlsMode::Verify && aws::is_rds_host(&conn.host)).then_some(aws::RDS_CA_BUNDLE)
}

#[cfg(test)]
pub(super) mod tests {
    use sqlx::postgres::PgPoolOptions;

    use super::super::postgres;
    use super::*;
    use crate::config::{Adapter, Auth};

    /// A PostgreSQL database at an RDS host, signing in with a password.
    pub(in crate::db) fn rds(tls: TlsMode) -> Connection {
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
            mongo: None,
        }
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
        let first = postgres::options(&rds(TlsMode::Require), "127.0.0.1", 40000, None);
        assert_eq!((first.get_host(), first.get_port()), ("127.0.0.1", 40000));
        let pool = Pool::Pg(PgPoolOptions::new().connect_lazy_with(first));
        let mut renamed = rds(TlsMode::Require);
        renamed.user = "reporting".into();
        pool.renew(&renamed, "127.0.0.1", 40000, Some("token"));
        let Pool::Pg(pg) = &pool else { unreachable!() };
        assert_eq!(pg.connect_options().get_username(), "reporting");
        assert_eq!(pg.connect_options().get_host(), "127.0.0.1");
    }

    #[tokio::test]
    async fn an_expired_password_makes_the_pool_unusable_so_connecting_says_why() {
        let conn = rds(TlsMode::Require);
        let options = postgres::options(&conn, "127.0.0.1", 40000, None);
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
