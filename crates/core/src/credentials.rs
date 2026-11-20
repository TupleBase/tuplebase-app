//! What a connection signs in with: its [`Auth`] turned into a password, and the server
//! to give it to.
//!
//! Each way of getting a password lives in the module that owns its system: the OS
//! credential store in [`crate::secrets`], cloud providers in [`crate::cloud`]. This module only picks
//! one, so `db` connects the same way whatever the connection uses.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::cloud::aws;
use crate::config::{Auth, AwsIamAuth, AwsSecretRef, AwsStore, Connection};
use crate::secrets;

/// How often a password that expires is made again, well before it does. An IAM token
/// lasts [`aws::IAM_TOKEN_LIFETIME`]; new connections need a valid one.
pub const IAM_REFRESH: Duration = Duration::from_secs(10 * 60);

/// What to sign in to and with. Not `Debug`, so the password can't end up in a log.
pub struct Resolved {
    /// The connection to open: host, port, database and user, some of which a secret
    /// in AWS can supply.
    pub target: Connection,
    /// `None` when there is none: a database file, or no password saved.
    pub password: Option<String>,
    /// When to resolve again because the password expires; `None` if it doesn't.
    pub refresh: Option<Expiry>,
}

/// How long a password that expires is good for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expiry {
    /// Make a new one this long after the last, well before it runs out.
    pub renew_after: Duration,
    /// After this long, new connections can't sign in with it.
    pub lasts: Duration,
}

/// An IAM token's.
pub const IAM_EXPIRY: Expiry = Expiry {
    renew_after: IAM_REFRESH,
    lasts: aws::IAM_TOKEN_LIFETIME,
};

/// Reads or makes the password for `conn`. Called on every connect, so a rotated secret
/// needs nothing from the user.
pub async fn resolve(conn: &Connection) -> Result<Resolved> {
    resolve_with(&System, conn).await
}

/// Where passwords come from; the tests replace it.
trait Store {
    /// The password saved in the OS credential store, if any.
    async fn keychain_password(&self, connection: &str) -> Result<Option<String>>;
    /// The text of a secret or parameter in AWS.
    async fn aws_secret(&self, store: AwsStore, secret: &AwsSecretRef) -> Result<String>;
    /// A new RDS IAM auth token.
    async fn iam_token(
        &self,
        iam: &AwsIamAuth,
        host: &str,
        port: u16,
        user: &str,
    ) -> Result<String>;
}

/// The real ones.
struct System;

impl Store for System {
    async fn keychain_password(&self, connection: &str) -> Result<Option<String>> {
        // Keychain access can block (or show an OS prompt), so keep it off the runtime.
        let name = connection.to_owned();
        tokio::task::spawn_blocking(move || secrets::get_password(&name)).await?
    }

    async fn aws_secret(&self, store: AwsStore, secret: &AwsSecretRef) -> Result<String> {
        aws::secret_value(store, secret).await
    }

    async fn iam_token(
        &self,
        iam: &AwsIamAuth,
        host: &str,
        port: u16,
        user: &str,
    ) -> Result<String> {
        aws::iam_token(&iam.profile, &iam.region, host, port, user).await
    }
}

async fn resolve_with(store: &impl Store, conn: &Connection) -> Result<Resolved> {
    let unchanged = |password, refresh| Resolved {
        target: conn.clone(),
        password,
        refresh,
    };
    if conn.adapter.is_file() {
        return Ok(unchanged(None, None));
    }
    match &conn.auth {
        Auth::Password => {
            let password = store.keychain_password(&conn.name).await?;
            Ok(unchanged(password, None))
        }
        Auth::AwsSecret(secret) | Auth::AwsParameter(secret) => {
            let kind = if matches!(conn.auth, Auth::AwsSecret(_)) {
                AwsStore::SecretsManager
            } else {
                AwsStore::ParameterStore
            };
            let read = async {
                let value = store.aws_secret(kind, secret).await?;
                aws::apply_secret(conn, &value, secret.use_secret_host)
            };
            let (target, password) = read
                .await
                .with_context(|| format!("reading \"{}\" from {}", secret.name, kind.label()))?;
            Ok(Resolved {
                target,
                password: Some(password),
                refresh: None,
            })
        }
        Auth::AwsIam(iam) => {
            // Before making a token, so none is ever sent without encryption.
            conn.check_iam_encryption()?;
            // Signed for the database's own name, even when a tunnel carries the traffic.
            let token = store
                .iam_token(iam, &conn.host, conn.port, &conn.user)
                .await?;
            Ok(unchanged(Some(token), Some(IAM_EXPIRY)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::bail;

    use super::*;
    use crate::config::{Adapter, TlsMode};

    /// Answers from fixed values and records what it was asked.
    #[derive(Default)]
    struct Fake {
        asked: Mutex<Vec<String>>,
        secret: Option<&'static str>,
    }

    impl Store for Fake {
        async fn keychain_password(&self, connection: &str) -> Result<Option<String>> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("keychain {connection}"));
            Ok(Some("from-keychain".into()))
        }

        async fn aws_secret(&self, store: AwsStore, secret: &AwsSecretRef) -> Result<String> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("{} {}", store.label(), secret.name));
            match self.secret {
                Some(value) => Ok(value.into()),
                None => bail!("ResourceNotFoundException: no such secret"),
            }
        }

        async fn iam_token(
            &self,
            iam: &AwsIamAuth,
            host: &str,
            port: u16,
            user: &str,
        ) -> Result<String> {
            self.asked
                .lock()
                .unwrap()
                .push(format!("iam {} {host}:{port} {user}", iam.profile));
            Ok(format!("{host}:{port}/?Action=connect&DBUser={user}"))
        }
    }

    fn conn(auth: Auth) -> Connection {
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
            tls: TlsMode::Require,
            ssh: None,
            auth,
            source: None,
        }
    }

    fn secret(name: &str, use_secret_host: bool) -> AwsSecretRef {
        AwsSecretRef {
            profile: "prod".into(),
            region: String::new(),
            name: name.into(),
            use_secret_host,
        }
    }

    #[tokio::test]
    async fn a_password_comes_from_the_keychain_and_never_expires() {
        let fake = Fake::default();
        let resolved = resolve_with(&fake, &conn(Auth::Password)).await.unwrap();
        assert_eq!(resolved.password.as_deref(), Some("from-keychain"));
        assert_eq!(resolved.target, conn(Auth::Password));
        assert_eq!(resolved.refresh, None);
        assert_eq!(*fake.asked.lock().unwrap(), ["keychain orders"]);
    }

    #[tokio::test]
    async fn a_database_file_needs_nothing() {
        let fake = Fake::default();
        let mut file = conn(Auth::Password);
        file.adapter = Adapter::Sqlite;
        let resolved = resolve_with(&fake, &file).await.unwrap();
        assert!(resolved.password.is_none());
        assert!(fake.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_secret_supplies_the_user_password_and_host() {
        let fake = Fake {
            secret: Some(r#"{"username":"admin","password":"s3cret","host":"other","port":6000}"#),
            ..Fake::default()
        };
        let resolved = resolve_with(&fake, &conn(Auth::AwsSecret(secret("prod/db", true))))
            .await
            .unwrap();
        assert_eq!(resolved.password.as_deref(), Some("s3cret"));
        assert_eq!(resolved.target.user, "admin");
        assert_eq!(
            (resolved.target.host.as_str(), resolved.target.port),
            ("other", 6000)
        );
        assert_eq!(resolved.refresh, None);
        assert_eq!(*fake.asked.lock().unwrap(), ["AWS Secrets Manager prod/db"]);
    }

    #[tokio::test]
    async fn a_parameter_can_be_the_password_alone() {
        let fake = Fake {
            secret: Some("hunter2"),
            ..Fake::default()
        };
        let auth = Auth::AwsParameter(secret("/prod/pw", false));
        let resolved = resolve_with(&fake, &conn(auth.clone())).await.unwrap();
        assert_eq!(resolved.password.as_deref(), Some("hunter2"));
        assert_eq!(resolved.target, conn(auth));
        assert_eq!(
            *fake.asked.lock().unwrap(),
            ["AWS Parameter Store /prod/pw"]
        );
    }

    #[tokio::test]
    async fn a_failed_read_names_the_secret_and_the_store() {
        let fake = Fake::default();
        let Err(error) =
            resolve_with(&fake, &conn(Auth::AwsSecret(secret("prod/db", false)))).await
        else {
            panic!("a missing secret resolved");
        };
        let error = format!("{error:#}");
        assert!(
            error.contains("\"prod/db\" from AWS Secrets Manager"),
            "{error}"
        );
        assert!(error.contains("ResourceNotFoundException"), "{error}");
    }

    #[tokio::test]
    async fn no_iam_token_is_made_for_a_connection_that_may_be_unencrypted() {
        // A file edited by hand skips the check on save.
        let fake = Fake::default();
        let mut plain = conn(Auth::AwsIam(AwsIamAuth {
            profile: "prod".into(),
            region: "eu-west-1".into(),
        }));
        plain.tls = TlsMode::Prefer;
        let error = resolve_with(&fake, &plain).await.err().unwrap().to_string();
        assert!(error.contains("encrypted connection"), "{error}");
        assert!(fake.asked.lock().unwrap().is_empty(), "no token was made");
    }

    #[tokio::test]
    async fn an_iam_token_is_made_for_the_real_endpoint_and_refreshed() {
        let fake = Fake::default();
        let mut tunneled = conn(Auth::AwsIam(AwsIamAuth {
            profile: "prod".into(),
            region: String::new(),
        }));
        tunneled.ssh = Some(crate::config::SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "me".into(),
            key_path: String::new(),
            fingerprint: "SHA256:abc".into(),
        });
        let resolved = resolve_with(&fake, &tunneled).await.unwrap();
        assert_eq!(
            resolved.password.as_deref(),
            Some("orders.abc.eu-west-1.rds.amazonaws.com:5432/?Action=connect&DBUser=app")
        );
        assert_eq!(resolved.target, tunneled);
        assert_eq!(resolved.refresh, Some(IAM_EXPIRY));
        assert!(IAM_REFRESH < aws::IAM_TOKEN_LIFETIME);
        assert_eq!(
            *fake.asked.lock().unwrap(),
            ["iam prod orders.abc.eu-west-1.rds.amazonaws.com:5432 app"]
        );
    }
}
