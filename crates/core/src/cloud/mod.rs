//! Cloud providers: one module per provider, each the only code that talks to it (L6).
//!
//! [`aws`] covers Amazon Web Services: credentials kept in Secrets Manager and Parameter
//! Store, RDS IAM auth tokens, the profiles on this machine, and finding databases in
//! RDS. This module holds what every provider shares: what discovery finds, how it can
//! fail, and [`discover`], which hands the work to the provider. A new provider is a new
//! file here and an arm in [`discover`].

pub mod aws;

use crate::config::{Adapter, Auth, Connection, Provider, Source, TlsMode};

/// A database found in a cloud account, ready to fill in the connection form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Its name in the provider, also the suggested connection name.
    pub label: String,
    /// `None` for an engine the app doesn't support yet; it is listed but can't be used.
    pub adapter: Option<Adapter>,
    /// The provider's name for the engine, such as `aurora-postgresql`.
    pub engine: String,
    /// The engine's version, such as `16.3`; empty when the provider names none.
    pub version: String,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    /// The provider's status, such as `available`.
    pub status: String,
    /// Whether it can be reached from the internet; otherwise it needs an SSH tunnel.
    pub public: bool,
    /// Whether it accepts IAM auth tokens.
    pub iam_enabled: bool,
    /// The secret the provider keeps the master user's password in, if it does.
    pub managed_secret: Option<String>,
    /// How the connection should sign in, chosen from the above.
    pub auth: Auth,
    pub source: Source,
    /// Something to know before using it, such as how to reach a private database.
    pub note: Option<String>,
}

impl Discovered {
    /// Whether the app can connect to it.
    pub fn usable(&self) -> bool {
        self.adapter.is_some() && !self.host.is_empty()
    }

    /// The engine as people name it, with its version: `Aurora PostgreSQL 15.4`,
    /// `Redshift Serverless`, or the provider's name for one the app doesn't know.
    pub fn engine_label(&self) -> String {
        let name = match self.engine.as_str() {
            "aurora-postgresql" => "Aurora PostgreSQL".to_owned(),
            "aurora-mysql" | "aurora" => "Aurora MySQL".to_owned(),
            "redshift-serverless" => "Redshift Serverless".to_owned(),
            _ => match self.adapter {
                Some(adapter) => adapter.label().to_owned(),
                None => self.engine.clone(),
            },
        };
        match self.version.as_str() {
            "" => name,
            version => format!("{name} {version}"),
        }
    }

    /// Whether a search for `query` finds it: each word of the query, ignoring case, is
    /// in its name, engine, version, region, endpoint or status. An empty query finds all.
    pub fn matches(&self, query: &str) -> bool {
        let fields = [
            &self.label,
            &self.engine,
            &self.engine_label(),
            &self.source.region,
            &self.host,
            &self.status,
        ]
        .map(|f| f.to_lowercase());
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| fields.iter().any(|field| field.contains(word)))
    }

    /// The connection it suggests: its name, endpoint, master user and database, with its
    /// certificate verified if it is public, else encryption required. `None` if it
    /// can't be used.
    pub fn connection(&self) -> Option<Connection> {
        let adapter = self.adapter.filter(|_| self.usable())?;
        Some(Connection {
            name: self.label.clone(),
            folder: String::new(),
            adapter,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            user: self.user.clone(),
            readonly: false,
            color: String::new(),
            // A public database's certificate can be checked (Amazon's RDS authorities come
            // with the app). Through an SSH tunnel the app reaches 127.0.0.1, which the
            // certificate doesn't name, so a private one only requires encryption.
            tls: if self.public {
                TlsMode::Verify
            } else {
                TlsMode::Require
            },
            ssh: None,
            auth: self.auth.clone(),
            source: Some(self.source.clone()),
        })
    }
}

/// Why looking in a place failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverErrorKind {
    /// The sign-in expired or is missing; the user signs in outside the app and retries.
    SignIn,
    /// The identity may not list databases.
    AccessDenied,
    /// No region was given and the profile has none.
    NoRegion,
    TimedOut,
    Other,
}

/// A place that could not be searched. Others may still have answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoverError {
    /// The region, or `None` when nothing could be searched.
    pub region: Option<String>,
    pub kind: DiscoverErrorKind,
    /// What to tell the user, including what to do about it: one short line.
    pub message: String,
    /// The technical cause, if there is more to it, to show less prominently.
    pub detail: Option<String>,
}

/// What a search found, region by region.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovery {
    pub found: Vec<Discovered>,
    /// Regions searched that hold no databases.
    pub empty_regions: Vec<String>,
    pub errors: Vec<DiscoverError>,
}

/// Lists the databases `profile` can see in `regions`; no regions means the profile's
/// own. Never fails as a whole: each region that can't be searched is an error in the
/// result, beside what the others found.
pub async fn discover(provider: Provider, profile: &str, regions: &[String]) -> Discovery {
    match provider {
        Provider::Aws => aws::discover(profile, regions).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AwsIamAuth;

    fn found() -> Discovered {
        Discovered {
            label: "orders".into(),
            adapter: Some(Adapter::Postgres),
            engine: "postgres".into(),
            version: "16.3".into(),
            host: "orders.abc.eu-west-1.rds.amazonaws.com".into(),
            port: 5432,
            database: "orders".into(),
            user: "admin".into(),
            status: "available".into(),
            public: false,
            iam_enabled: true,
            managed_secret: None,
            auth: Auth::AwsIam(AwsIamAuth {
                profile: "prod".into(),
                region: "eu-west-1".into(),
            }),
            source: Source {
                provider: Provider::Aws,
                resource: "arn:aws:rds:eu-west-1:123456789012:db:orders".into(),
                region: "eu-west-1".into(),
            },
            note: None,
        }
    }

    #[test]
    fn a_found_database_fills_in_a_valid_connection() {
        let conn = found().connection().unwrap();
        assert_eq!(conn.name, "orders");
        // Private: reached through a tunnel, where the certificate can't match.
        assert_eq!(conn.tls, TlsMode::Require);
        let mut public = found();
        public.public = true;
        assert_eq!(public.connection().unwrap().tls, TlsMode::Verify);
        assert_eq!(conn.source, Some(found().source));
        let mut config = crate::config::Config::default();
        config.upsert(conn, None).unwrap();
    }

    #[test]
    fn a_search_matches_every_word_in_any_field_ignoring_case() {
        let mut aurora = found();
        aurora.engine = "aurora-postgresql".into();
        assert_eq!(aurora.engine_label(), "Aurora PostgreSQL 16.3");
        assert!(aurora.matches(""));
        assert!(aurora.matches("ORDERS"));
        assert!(aurora.matches("aurora eu-west"));
        assert!(aurora.matches("postgresql 16"));
        assert!(aurora.matches("abc.eu-west-1.rds"));
        assert!(aurora.matches("  available  "));
        assert!(!aurora.matches("orders mysql"));
        assert!(!aurora.matches("us-east-1"));
        let mut legacy = found();
        legacy.adapter = None;
        legacy.engine = "oracle-ee".into();
        legacy.version.clear();
        assert_eq!(legacy.engine_label(), "oracle-ee");
    }

    #[test]
    fn an_unsupported_engine_or_one_without_an_endpoint_gives_nothing() {
        let mut oracle = found();
        oracle.adapter = None;
        assert!(!oracle.usable() && oracle.connection().is_none());
        let mut creating = found();
        creating.host.clear();
        assert!(creating.connection().is_none());
    }
}
