//! Everything that talks to AWS: credentials kept in Secrets Manager and SSM Parameter
//! Store, RDS IAM auth tokens and the profiles on this machine.
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

use crate::config::{AwsSecretRef, AwsStore, Connection};

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

/// The connection to open and its password: `conn` with the user (and the host, if
/// `use_secret_host`) taken from a secret's `value`.
pub fn apply_secret(
    conn: &Connection,
    value: &str,
    use_secret_host: bool,
) -> Result<(Connection, String)> {
    Secret::parse(value)?.apply(conn, use_secret_host)
}

/// Reads the text of a secret or parameter from AWS.
pub async fn secret_value(store: AwsStore, secret: &AwsSecretRef) -> Result<String> {
    timeout(TIMEOUT, read(store, secret))
        .await
        .map_err(|_| anyhow!("timed out"))?
}

async fn read(store: AwsStore, secret: &AwsSecretRef) -> Result<String> {
    let config = sdk_config(&secret.profile, &secret.region).await?;
    match store {
        AwsStore::SecretsManager => {
            let output = aws_sdk_secretsmanager::Client::new(&config)
                .get_secret_value()
                .secret_id(&secret.name)
                .send()
                .await
                .map_err(api_error)?;
            let value = output.secret_string().map(str::to_owned);
            value.context("the secret is binary, not text")
        }
        AwsStore::ParameterStore => {
            let output = aws_sdk_ssm::Client::new(&config)
                .get_parameter()
                .name(&secret.name)
                .with_decryption(true)
                .send()
                .await
                .map_err(api_error)?;
            let value = output.parameter().and_then(|p| p.value());
            value.map(str::to_owned).context("the parameter is empty")
        }
    }
}

/// How long RDS accepts an IAM auth token for new connections, which is also the most it
/// allows.
pub const IAM_TOKEN_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// An RDS IAM auth token: the password for `user` at `host:port`, valid for
/// [`IAM_TOKEN_LIFETIME`]. Made from the profile's credentials, read afresh on every call.
///
/// `host` and `port` must be the database's real endpoint, even when the connection goes
/// through an SSH tunnel: the token is signed for that name. An empty `region` means the
/// one in an `*.rds.amazonaws.com` host name, or else the profile's.
pub async fn iam_token(
    profile: &str,
    region: &str,
    host: &str,
    port: u16,
    user: &str,
) -> Result<String> {
    let region = match region {
        "" => region_of_host(host).unwrap_or_default(),
        region => region.to_owned(),
    };
    let made = async {
        let config = sdk_config(profile, &region).await?;
        sign_iam_token(&config, host, port, user).await
    };
    timeout(TIMEOUT, made)
        .await
        .map_err(|_| anyhow!("timed out"))?
        .context("making an RDS IAM auth token")
}

/// Signs the token with the credentials, region and clock in `config`.
async fn sign_iam_token(config: &SdkConfig, host: &str, port: u16, user: &str) -> Result<String> {
    let region = config.region().context("no AWS region")?.clone();
    let generator = aws_sdk_rds::auth_token::AuthTokenGenerator::new(
        aws_sdk_rds::auth_token::Config::builder()
            .hostname(host)
            .port(port.into())
            .username(user)
            .region(region)
            .expires_in(IAM_TOKEN_LIFETIME.as_secs())
            .build()
            .map_err(|e| anyhow!("{e}"))?,
    );
    let token = generator
        .auth_token(config)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(token.as_str().to_owned())
}

/// The region in an RDS endpoint such as `orders.abc123.eu-west-1.rds.amazonaws.com`.
pub fn region_of_host(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let rest = host
        .strip_suffix(".rds.amazonaws.com")
        .or_else(|| host.strip_suffix(".rds.amazonaws.com.cn"))?;
    let region = rest.rsplit('.').next()?;
    let looks_like_one = region.split('-').count() >= 3
        && region
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    looks_like_one.then(|| region.to_owned())
}

/// Whether `host` is an RDS endpoint, whose certificates come from Amazon's RDS
/// certificate authorities.
pub fn is_rds_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host.ends_with(".rds.amazonaws.com") || host.ends_with(".rds.amazonaws.com.cn")
}

/// Amazon's RDS certificate authorities, for verifying an RDS database's certificate.
///
/// Source: <https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem>, the
/// bundle for every commercial region, linked from "Using SSL/TLS to encrypt a connection
/// to a DB instance" in the RDS user guide. It is public. To update it, download that URL
/// over the file and check that `the_rds_bundle_holds_certificates` still passes.
pub const RDS_CA_BUNDLE: &[u8] = include_bytes!("../../certs/rds-global-bundle.pem");

/// An AWS profile in `~/.aws/config` or `~/.aws/credentials`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// The profile's own region, if it sets one.
    pub region: Option<String>,
}

/// The profiles set up on this machine, sorted by name. Honours `AWS_CONFIG_FILE` and
/// `AWS_SHARED_CREDENTIALS_FILE`.
pub async fn profiles() -> Result<Vec<Profile>> {
    profiles_in(&Fs::real(), &Env::real()).await
}

async fn profiles_in(fs: &Fs, env: &Env) -> Result<Vec<Profile>> {
    let files = Default::default();
    let set = aws_config::profile::load(fs, env, &files, None)
        .await
        .map_err(|e| anyhow!("reading the AWS config files: {}", causes(&e)))?;
    let mut profiles: Vec<Profile> = set
        .profiles()
        .map(|name| Profile {
            name: name.to_owned(),
            region: set
                .get_profile(name)
                .and_then(|p| p.get("region"))
                .map(str::to_owned),
        })
        .collect();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
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
    use crate::config::{Adapter, Auth, TlsMode};
    use aws_sdk_rds::config::Credentials;

    /// Fixed credentials, region and clock, so a token comes out the same every time.
    fn fixed_config(region: &str) -> SdkConfig {
        SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(Credentials::new(
                "AKIDEXAMPLE",
                "secret",
                None,
                None,
                "test",
            )))
            .region(Region::new(region.to_owned()))
            // 2024-08-26T22:00:00Z
            .time_source(aws_smithy_async::time::StaticTimeSource::from_secs(
                1_724_709_600,
            ))
            .build()
    }

    #[tokio::test]
    async fn an_iam_token_is_a_presigned_connect_request_for_the_endpoint() {
        let host = "orders.abc123.eu-west-1.rds.amazonaws.com";
        let token = sign_iam_token(&fixed_config("eu-west-1"), host, 5432, "app_user")
            .await
            .unwrap();
        let (endpoint, query) = token.split_once("/?").expect("host, then a query");
        assert_eq!(endpoint, format!("{host}:5432"));
        let params: Vec<(&str, &str)> = query
            .split('&')
            .map(|pair| pair.split_once('=').unwrap())
            .collect();
        let param = |name: &str| params.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
        assert_eq!(param("Action"), Some("connect"));
        assert_eq!(param("DBUser"), Some("app_user"));
        assert_eq!(param("X-Amz-Expires"), Some("900"));
        assert_eq!(param("X-Amz-Algorithm"), Some("AWS4-HMAC-SHA256"));
        assert_eq!(param("X-Amz-Date"), Some("20240826T220000Z"));
        assert_eq!(
            param("X-Amz-Credential"),
            Some("AKIDEXAMPLE%2F20240826%2Feu-west-1%2Frds-db%2Faws4_request")
        );
        assert!(param("X-Amz-Signature").is_some_and(|s| s.len() == 64));
        assert!(!token.starts_with("https://"));
    }

    #[test]
    fn the_region_is_read_from_an_rds_host_name() {
        let region = |host| region_of_host(host);
        assert_eq!(
            region("orders.abc123.eu-west-1.rds.amazonaws.com").as_deref(),
            Some("eu-west-1")
        );
        assert_eq!(
            region("Orders.cluster-ro-abc.US-EAST-2.rds.amazonaws.com.").as_deref(),
            Some("us-east-2")
        );
        assert_eq!(region("db.example.com"), None);
        assert_eq!(region("localhost"), None);
        assert!(is_rds_host("orders.abc123.eu-west-1.rds.amazonaws.com"));
        assert!(!is_rds_host("rds.amazonaws.com.example.com"));
    }

    #[test]
    fn the_rds_bundle_holds_certificates() {
        use rustls_pki_types::CertificateDer;
        use rustls_pki_types::pem::PemObject;
        let certificates: Vec<_> = CertificateDer::pem_slice_iter(RDS_CA_BUNDLE)
            .collect::<Result<_, _>>()
            .expect("every certificate parses");
        assert!(certificates.len() > 50, "{}", certificates.len());
    }

    #[tokio::test]
    async fn profiles_come_from_both_files_with_their_regions() {
        let fs = Fs::from_slice(&[
            (
                "/home/me/.aws/config",
                "[default]\nregion = eu-west-1\n\n[profile prod-sso]\nsso_session = corp\n\
                 region = us-east-1\n\n[sso-session corp]\nsso_region = us-east-1\n",
            ),
            (
                "/home/me/.aws/credentials",
                "[ci]\naws_access_key_id = AKID\naws_secret_access_key = s\n",
            ),
        ]);
        let env = Env::from_slice(&[("HOME", "/home/me")]);
        let profiles = profiles_in(&fs, &env).await.unwrap();
        let names: Vec<_> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["ci", "default", "prod-sso"]);
        assert_eq!(profiles[1].region.as_deref(), Some("eu-west-1"));
        assert_eq!(profiles[2].region.as_deref(), Some("us-east-1"));
        assert_eq!(profiles[0].region, None);
    }

    #[tokio::test]
    async fn no_aws_files_means_no_profiles() {
        let env = Env::from_slice(&[("HOME", "/home/me")]);
        assert!(
            profiles_in(&Fs::from_slice(&[]), &env)
                .await
                .unwrap()
                .is_empty()
        );
    }

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
            auth: Auth::Password,
            source: None,
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
