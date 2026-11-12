//! Credentials kept in AWS: Secrets Manager secrets and SSM Parameter Store parameters.
//!
//! A connection saves only where the secret is (profile, region, name). The value is
//! read on every connect and never written to disk or the OS credential store, so a
//! rotated password needs nothing from the user.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_sdk_ssm::config::{ProvideCredentials, SharedCredentialsProvider};
use aws_sdk_ssm::error::{ProvideErrorMetadata, SdkError};
use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
use aws_types::os_shim_internal::{Env, Fs};
use serde_json::Value;
use tokio::time::timeout;

use crate::config::{AwsAuth, AwsStore, Connection};

const TIMEOUT: Duration = Duration::from_secs(20);

/// What a secret says about a connection.
#[derive(Default)]
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
        let fields = match serde_json::from_str(value) {
            Ok(Value::Object(fields)) => fields,
            // Sent as a password, broken JSON would fail far from its cause.
            Err(e) if value.trim().starts_with('{') && value.trim().ends_with('}') => {
                bail!("the secret looks like JSON but isn't valid ({e})")
            }
            _ => {
                // A line break after the password comes from how it was pasted or piped in.
                let password = value.trim_end_matches(['\r', '\n']);
                if password.is_empty() {
                    bail!("the secret is empty");
                }
                return Ok(Self {
                    password: password.to_owned(),
                    ..Self::default()
                });
            }
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
    let resolved = async {
        let value = timeout(TIMEOUT, read(auth))
            .await
            .map_err(|_| anyhow!("timed out"))??;
        Secret::parse(&value)?.apply(conn, auth.use_secret_host)
    };
    resolved
        .await
        .with_context(|| format!("reading \"{}\" from {}", auth.name, auth.store.label()))
}

async fn read(auth: &AwsAuth) -> Result<String> {
    let config = sdk_config(&auth.profile, &auth.region).await?;
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

/// What AWS clients are built from. Loads the profile and signs in once, so a missing or
/// expired session is reported as such instead of as a failed request. An empty `profile`
/// means the default AWS credentials; an empty `region` means the profile's.
pub async fn sdk_config(profile: &str, region: &str) -> Result<SdkConfig> {
    // `ring` keeps the build free of C libraries (the SDK's default TLS is aws-lc).
    let http = aws_smithy_http_client::Builder::new()
        .tls_provider(Provider::Rustls(CryptoMode::Ring))
        .build_https();
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).http_client(http);
    if !profile.is_empty() {
        loader = loader.profile_name(profile);
    }
    if !region.is_empty() {
        loader = loader.region(Region::new(region.to_owned()));
    }
    let config = loader.load().await;
    let provider = config
        .credentials_provider()
        .context("no AWS credentials are set up on this machine")?;
    let credentials = match provider.provide_credentials().await {
        Ok(credentials) => credentials,
        Err(e) if !profile_exists(profile).await => {
            bail!(
                "the AWS profile \"{profile}\" is not in your AWS config ({})",
                causes(&e)
            )
        }
        Err(e) => {
            let mut login = "aws sso login".to_owned();
            if !profile.is_empty() {
                login = format!("{login} --profile {profile}");
            }
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
        assert_eq!(Secret::parse("hunter2\r\n").unwrap().password, "hunter2");
        assert!(Secret::parse("").is_err());
    }

    #[test]
    fn broken_json_is_reported_instead_of_used_as_a_password() {
        let Err(error) = Secret::parse(r#"{"username":"app","password":"s3cret",}"#) else {
            panic!("broken JSON was accepted");
        };
        let error = error.to_string();
        assert!(error.contains("JSON"), "{error}");
        assert!(!error.contains("s3cret"), "{error}");
    }

    #[test]
    fn causes_are_one_line_without_repeats_or_lists() {
        let root = anyhow!("no credentials found in chain. Attempted:\n  Environment: not set");
        let error = root
            .context("loading credentials")
            .context("loading credentials");
        assert_eq!(
            causes(error.as_ref()),
            "loading credentials: no credentials found in chain."
        );
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
