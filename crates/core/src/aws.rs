//! Credentials kept in AWS: Secrets Manager secrets and SSM Parameter Store parameters.
//!
//! A connection saves only where the secret is (profile, region, name). The value is
//! read on every connect and never written to disk or the OS credential store, so a
//! rotated password needs nothing from the user.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_config::{BehaviorVersion, SdkConfig};
use aws_sdk_ssm::config::{ProvideCredentials, Region, SharedCredentialsProvider};
use aws_sdk_ssm::error::{ProvideErrorMetadata, SdkError};
use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
use aws_types::os_shim_internal::{Env, Fs};
use serde_json::Value;
use tokio::time::timeout;

use crate::config::{AwsAuth, AwsStore, Connection};

const TIMEOUT: Duration = Duration::from_secs(20);

/// What a secret says about a connection.
struct Secret {
    username: Option<String>,
    password: String,
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
}

impl Secret {
    /// Reads a JSON object in the shape RDS writes: `username`, `password`, `host`,
    /// `port`, `dbname`. Any other text is the password itself.
    fn parse(value: &str) -> Result<Self> {
        let Ok(Value::Object(fields)) = serde_json::from_str(value) else {
            if value.is_empty() {
                bail!("the secret is empty");
            }
            return Ok(Self {
                username: None,
                password: value.to_owned(),
                host: None,
                port: None,
                database: None,
            });
        };
        let text = |key: &str| match fields.get(key) {
            Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
            Some(Value::Number(number)) => Some(number.to_string()),
            _ => None,
        };
        let port = text("port")
            .map(|port| port.parse())
            .transpose()
            .map_err(|_| anyhow!("the secret's \"port\" is not a port number"))?;
        Ok(Self {
            username: text("username"),
            password: text("password").context("the secret has no \"password\" field")?,
            host: text("host"),
            port,
            database: text("dbname"),
        })
    }

    /// The connection to open and its password: `conn` with what the secret provides.
    /// The connection's own database wins, so one secret can serve several databases.
    fn apply(self, conn: &Connection, use_secret_host: bool) -> Result<(Connection, String)> {
        let mut target = conn.clone();
        if let Some(username) = self.username {
            target.user = username;
        }
        if target.user.is_empty() {
            bail!("the secret has no \"username\" field, so the connection needs a user");
        }
        if use_secret_host {
            target.host = self.host.context("the secret has no \"host\" field")?;
            target.port = self.port.unwrap_or(conn.adapter.default_port());
            if let Some(database) = self.database.filter(|_| target.database.is_empty()) {
                target.database = database;
            }
        }
        Ok((target, self.password))
    }
}

/// Reads the secret `auth` points at. Returns the connection to open, with the user
/// (and host, if asked) filled in from the secret, and its password.
pub async fn resolve(conn: &Connection, auth: &AwsAuth) -> Result<(Connection, String)> {
    let source = format!("reading \"{}\" from {}", auth.name, auth.store.label());
    let value = timeout(TIMEOUT, read(auth))
        .await
        .map_err(|_| anyhow!("timed out"))
        .flatten()
        .with_context(|| source.clone())?;
    Secret::parse(&value)
        .and_then(|secret| secret.apply(conn, auth.use_secret_host))
        .context(source)
}

async fn read(auth: &AwsAuth) -> Result<String> {
    let config = sdk_config(auth).await?;
    match auth.store {
        AwsStore::SecretsManager => {
            let output = aws_sdk_secretsmanager::Client::new(&config)
                .get_secret_value()
                .secret_id(&auth.name)
                .send()
                .await
                .map_err(api_error)?;
            let value = output.secret_string().map(str::to_owned);
            value.context("the secret is binary, not text")
        }
        AwsStore::ParameterStore => {
            let output = aws_sdk_ssm::Client::new(&config)
                .get_parameter()
                .name(&auth.name)
                .with_decryption(true)
                .send()
                .await
                .map_err(api_error)?;
            let value = output.parameter().and_then(|p| p.value());
            value.map(str::to_owned).context("the parameter is empty")
        }
    }
}

/// Loads the profile and signs in once, so a missing or expired session is reported as
/// such instead of as a failed request.
async fn sdk_config(auth: &AwsAuth) -> Result<SdkConfig> {
    // `ring` keeps the build free of C libraries (the SDK's default TLS is aws-lc).
    let http = aws_smithy_http_client::Builder::new()
        .tls_provider(Provider::Rustls(CryptoMode::Ring))
        .build_https();
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).http_client(http);
    if !auth.profile.is_empty() {
        loader = loader.profile_name(&auth.profile);
    }
    if !auth.region.is_empty() {
        loader = loader.region(Region::new(auth.region.clone()));
    }
    let config = loader.load().await;
    let provider = config
        .credentials_provider()
        .context("no AWS credentials are set up on this machine")?;
    let credentials = match provider.provide_credentials().await {
        Ok(credentials) => credentials,
        Err(e) if !profile_exists(&auth.profile).await => {
            bail!(
                "the AWS profile \"{}\" is not in your AWS config ({})",
                auth.profile,
                causes(&e)
            )
        }
        Err(e) => {
            let profile = Some(&auth.profile).filter(|p| !p.is_empty());
            let login = profile.map_or_else(
                || "aws sso login".to_owned(),
                |profile| format!("aws sso login --profile {profile}"),
            );
            bail!(
                "no usable AWS credentials ({}). If your session expired, run `{login}`",
                causes(&e)
            )
        }
    };
    if config.region().is_none() {
        bail!("no AWS region: set one on the connection or in the AWS profile");
    }
    Ok(config
        .into_builder()
        .credentials_provider(SharedCredentialsProvider::new(credentials))
        .build())
}

/// Whether the AWS config files define `profile`. True for the default one, and when the
/// files can't be read, so only a profile that is surely missing is reported as such.
async fn profile_exists(profile: &str) -> bool {
    if profile.is_empty() {
        return true;
    }
    let files = Default::default();
    match aws_config::profile::load(&Fs::real(), &Env::real(), &files, None).await {
        Ok(profiles) => profiles.get_profile(profile).is_some(),
        Err(_) => true,
    }
}

/// An error's causes on one line, without the repeats and the lists the SDK adds.
fn causes(err: &(dyn std::error::Error + 'static)) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut next = Some(err);
    while let Some(err) = next {
        let text = err.to_string();
        let line = text.lines().next().unwrap_or_default();
        let line = line.trim_end_matches(" Attempted:").to_owned();
        if lines.last() != Some(&line) {
            lines.push(line);
        }
        next = err.source();
    }
    lines.join(": ")
}

/// AWS's own code and message for a refused request; the cause chain for anything else.
fn api_error<E>(err: SdkError<E>) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match err.as_service_error() {
        Some(refused) => anyhow!(
            "{}: {}",
            refused.code().unwrap_or("AWS error"),
            refused.message().unwrap_or("no details given")
        ),
        None => anyhow::Error::new(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Adapter, TlsMode};

    fn conn() -> Connection {
        Connection {
            name: "orders".into(),
            folder: String::new(),
            adapter: Adapter::Postgres,
            host: "localhost".into(),
            port: 6000,
            database: String::new(),
            user: "me".into(),
            readonly: false,
            color: String::new(),
            tls: TlsMode::default(),
            ssh: None,
            auth: None,
        }
    }

    const RDS: &str = r#"{"username":"app","password":"s3cret","engine":"postgres",
        "host":"orders.rds.amazonaws.com","port":5433,"dbname":"orders"}"#;

    #[test]
    fn the_secret_supplies_user_and_password_and_leaves_the_host_alone() {
        let (target, password) = Secret::parse(RDS).unwrap().apply(&conn(), false).unwrap();
        assert_eq!((target.user.as_str(), password.as_str()), ("app", "s3cret"));
        assert_eq!((target.host.as_str(), target.port), ("localhost", 6000));
        assert_eq!(target.database, "");
    }

    #[test]
    fn the_secret_can_supply_host_port_and_database() {
        let (target, _) = Secret::parse(RDS).unwrap().apply(&conn(), true).unwrap();
        assert_eq!(target.host, "orders.rds.amazonaws.com");
        assert_eq!((target.port, target.database.as_str()), (5433, "orders"));

        let mut named = conn();
        named.database = "reports".into();
        let (target, _) = Secret::parse(RDS).unwrap().apply(&named, true).unwrap();
        assert_eq!(target.database, "reports");
    }

    #[test]
    fn a_port_can_be_text_and_defaults_to_the_database_one() {
        let text = r#"{"password":"p","host":"h","port":"5433"}"#;
        let (target, _) = Secret::parse(text).unwrap().apply(&conn(), true).unwrap();
        assert_eq!((target.port, target.user.as_str()), (5433, "me"));

        let none = r#"{"password":"p","host":"h"}"#;
        let (target, _) = Secret::parse(none).unwrap().apply(&conn(), true).unwrap();
        assert_eq!(target.port, 5432);

        assert!(Secret::parse(r#"{"password":"p","port":"abc"}"#).is_err());
    }

    #[test]
    fn text_that_is_not_a_json_object_is_the_password() {
        for plain in ["hunter2", "12345", "{not json"] {
            let (target, password) = Secret::parse(plain).unwrap().apply(&conn(), false).unwrap();
            assert_eq!((target.user.as_str(), password.as_str()), ("me", plain));
        }
        assert!(Secret::parse("").is_err());
    }

    #[test]
    fn missing_fields_are_named() {
        let error = |value: &str, conn: &Connection, use_host| {
            let result = Secret::parse(value).and_then(|s| s.apply(conn, use_host));
            result.unwrap_err().to_string()
        };
        assert!(error(r#"{"username":"app"}"#, &conn(), false).contains("\"password\""));
        assert!(error(r#"{"password":"p"}"#, &conn(), true).contains("\"host\""));
        let mut no_user = conn();
        no_user.user.clear();
        assert!(error("hunter2", &no_user, false).contains("\"username\""));
    }
}
