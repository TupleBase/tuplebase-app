//! Everything that talks to AWS: credentials kept in Secrets Manager and SSM Parameter
//! Store, RDS IAM auth tokens, temporary Redshift credentials, the profiles on this
//! machine, and the databases in RDS and Redshift.
//!
//! A connection saves only where the secret is (profile, region, name). The value is
//! read on every connect and never written to disk or the OS credential store, so a
//! rotated password needs nothing from the user.

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use aws_config::profile::ProfileFileCredentialsProvider;
use aws_config::provider_config::ProviderConfig;
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_sdk_rds::types::{DbCluster, DbInstance};
use aws_sdk_redshiftserverless::types::{Namespace, Workgroup};
use aws_sdk_ssm::config::{ProvideCredentials, SharedCredentialsProvider};
use aws_sdk_ssm::error::{ProvideErrorMetadata, SdkError};
use aws_smithy_http_client::tls::{Provider as TlsProvider, rustls_provider::CryptoMode};
use aws_types::os_shim_internal::{Env, Fs};
use serde_json::Value;
use tokio::time::timeout;

use super::{DiscoverError, DiscoverErrorKind, Discovered, Discovery};
use crate::config::{
    Adapter, Auth, AwsIamAuth, AwsSecretRef, AwsStore, Connection, Provider, Source,
};

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

/// An RDS IAM auth token, made by [`iam_token`].
pub struct IamToken {
    /// The password.
    pub token: String,
    /// When the credentials that signed it expire, if they do; the token is refused from
    /// then on even within its own lifetime.
    pub credentials_expire: Option<SystemTime>,
}

/// An RDS IAM auth token: the password for `user` at `host:port`, valid for
/// [`IAM_TOKEN_LIFETIME`] or until the credentials that signed it expire, whichever is
/// sooner. Made from the profile's credentials, read afresh on every call.
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
) -> Result<IamToken> {
    let region = match region {
        "" => region_of_host(host).unwrap_or_default(),
        region => region.to_owned(),
    };
    let made = async {
        let config = sdk_config(profile, &region).await?;
        let credentials_expire = match config.credentials_provider() {
            Some(provider) => provider.provide_credentials().await?.expiry(),
            None => None,
        };
        let token = sign_iam_token(&config, host, port, user).await?;
        anyhow::Ok(IamToken {
            token,
            credentials_expire,
        })
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

/// The service, the labels before the region, and the region of an AWS database
/// endpoint: `orders.abc123.eu-west-1.rds.amazonaws.com` gives `rds`, `orders.abc123`
/// and `eu-west-1`. RDS, Redshift and Redshift Serverless endpoints, also in China.
fn endpoint_parts(host: &str) -> Option<(String, String, String)> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let rest = host
        .strip_suffix(".amazonaws.com")
        .or_else(|| host.strip_suffix(".amazonaws.com.cn"))?;
    let (rest, service) = rest.rsplit_once('.')?;
    if !["rds", "redshift", "redshift-serverless"].contains(&service) {
        return None;
    }
    let (labels, region) = rest.rsplit_once('.')?;
    let looks_like_one = region.split('-').count() >= 3
        && region
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    (looks_like_one && !labels.is_empty())
        .then(|| (service.to_owned(), labels.to_owned(), region.to_owned()))
}

/// The region in an RDS or Redshift endpoint such as
/// `orders.abc123.eu-west-1.rds.amazonaws.com`.
pub fn region_of_host(host: &str) -> Option<String> {
    endpoint_parts(host).map(|(_, _, region)| region)
}

/// What a Redshift endpoint names, for asking AWS for its temporary credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedshiftTarget {
    /// A provisioned cluster, by its identifier.
    Cluster(String),
    /// A Redshift Serverless workgroup, by its name.
    Workgroup(String),
}

/// The cluster or workgroup at a Redshift endpoint, and its region. A cluster's endpoint
/// is `<cluster>.<id>.<region>.redshift.amazonaws.com`, a workgroup's
/// `<workgroup>.<account>.<region>.redshift-serverless.amazonaws.com`. `None` for any other
/// host, such as a custom domain name.
pub fn redshift_target(host: &str) -> Option<(RedshiftTarget, String)> {
    let (service, labels, region) = endpoint_parts(host)?;
    let (name, _) = labels.split_once('.')?;
    let target = match service.as_str() {
        "redshift" => RedshiftTarget::Cluster(name.to_owned()),
        "redshift-serverless" => RedshiftTarget::Workgroup(name.to_owned()),
        _ => return None,
    };
    Some((target, region))
}

/// How long a temporary Redshift password is asked for: the least both
/// `GetClusterCredentials` and Redshift Serverless's `GetCredentials` allow, the same as
/// an RDS IAM token, so it is renewed on the same schedule.
pub const REDSHIFT_PASSWORD_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// A temporary Redshift sign-in, from [`redshift_credentials`].
pub struct RedshiftLogin {
    /// The database user to sign in as, which AWS names: `IAM:<user>` for a cluster,
    /// `IAM:<user>` or `IAMR:<role>` for a workgroup.
    pub user: String,
    pub password: String,
    /// When new connections can no longer sign in with it.
    pub expires: Option<SystemTime>,
}

/// Temporary credentials for the Redshift at `host`: `GetClusterCredentials` for a
/// provisioned cluster, for the database user `user`, never creating it; Redshift
/// Serverless's `GetCredentials` for a workgroup, whose user AWS takes from the IAM
/// identity. `database` limits them to that database when given. An empty `region` means
/// the one in the host name. Made from the profile's credentials, read afresh on every
/// call.
///
/// `host` must be the cluster's or workgroup's own endpoint, even when the connection goes
/// through an SSH tunnel: it names the cluster or workgroup.
pub async fn redshift_credentials(
    profile: &str,
    region: &str,
    host: &str,
    user: &str,
    database: &str,
) -> Result<RedshiftLogin> {
    let Some((target, host_region)) = redshift_target(host) else {
        bail!(
            "signing in to Redshift with IAM needs the cluster's or workgroup's own endpoint \
             (*.redshift.amazonaws.com or *.redshift-serverless.amazonaws.com) as the host, \
             to know which one to ask for credentials; \"{host}\" isn't one"
        );
    };
    let region = match region {
        "" => host_region,
        region => region.to_owned(),
    };
    let made = async {
        let config = sdk_config(profile, &region).await?;
        match &target {
            RedshiftTarget::Cluster(cluster) => {
                let client = aws_sdk_redshift::Client::new(&config);
                let output = cluster_credentials(&client, cluster, user, database)
                    .send()
                    .await
                    .map_err(api_error)?;
                redshift_login(output.db_user(), output.db_password(), output.expiration())
            }
            RedshiftTarget::Workgroup(workgroup) => {
                let client = aws_sdk_redshiftserverless::Client::new(&config);
                let output = workgroup_credentials(&client, workgroup, database)
                    .send()
                    .await
                    .map_err(api_error)?;
                redshift_login(output.db_user(), output.db_password(), output.expiration())
            }
        }
    };
    timeout(TIMEOUT, made)
        .await
        .map_err(|_| anyhow!("timed out"))?
        .context("getting temporary Redshift credentials")
}

/// The `GetClusterCredentials` request: the user, never created, and the database when
/// given, for [`REDSHIFT_PASSWORD_LIFETIME`].
fn cluster_credentials(
    client: &aws_sdk_redshift::Client,
    cluster: &str,
    user: &str,
    database: &str,
) -> aws_sdk_redshift::operation::get_cluster_credentials::builders::GetClusterCredentialsFluentBuilder
{
    client
        .get_cluster_credentials()
        .cluster_identifier(cluster)
        .db_user(user)
        .set_db_name((!database.is_empty()).then(|| database.to_owned()))
        .auto_create(false)
        .duration_seconds(REDSHIFT_PASSWORD_LIFETIME.as_secs() as i32)
}

/// Redshift Serverless's `GetCredentials` request: the workgroup, and the database when
/// given, for [`REDSHIFT_PASSWORD_LIFETIME`]. It takes no user: AWS maps the IAM identity
/// to one.
fn workgroup_credentials(
    client: &aws_sdk_redshiftserverless::Client,
    workgroup: &str,
    database: &str,
) -> aws_sdk_redshiftserverless::operation::get_credentials::builders::GetCredentialsFluentBuilder {
    client
        .get_credentials()
        .workgroup_name(workgroup)
        .set_db_name((!database.is_empty()).then(|| database.to_owned()))
        .duration_seconds(REDSHIFT_PASSWORD_LIFETIME.as_secs() as i32)
}

/// The sign-in from what either credentials call answered.
fn redshift_login(
    user: Option<&str>,
    password: Option<&str>,
    expiration: Option<&aws_sdk_redshift::primitives::DateTime>,
) -> Result<RedshiftLogin> {
    let user = user.filter(|u| !u.is_empty());
    let password = password.filter(|p| !p.is_empty());
    Ok(RedshiftLogin {
        user: user.context("AWS returned no database user")?.to_owned(),
        password: password.context("AWS returned no password")?.to_owned(),
        expires: expiration.and_then(|at| SystemTime::try_from(*at).ok()),
    })
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

/// Finds the RDS databases, Aurora clusters, Redshift clusters and Redshift Serverless
/// workgroups `profile` can see in `regions`, searching the regions and the services at
/// once; no regions means the profile's own. Each service needs its own permissions (see
/// [`Service::permissions`]); one that is refused is an error beside what the others
/// found. See [`super::discover`].
pub async fn discover(profile: &str, regions: &[String]) -> Discovery {
    let regions: Vec<&str> = regions
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    let mut discovery = Discovery::default();
    // Signing in once serves every region, and a failure is reported once.
    let first = regions.first().copied().unwrap_or_default();
    let base = match timeout(TIMEOUT, sdk_config(profile, first)).await {
        Ok(Ok(config)) => config,
        Ok(Err(e)) => {
            discovery
                .errors
                .push(sign_in_error(profile, &format!("{e:#}")));
            return discovery;
        }
        Err(_) => {
            discovery.errors.push(DiscoverError {
                region: None,
                kind: DiscoverErrorKind::TimedOut,
                message: "Timed out signing in to AWS.".into(),
                detail: None,
            });
            return discovery;
        }
    };
    let regions: Vec<String> = if regions.is_empty() {
        // `sdk_config` refuses a config with no region, so there is one.
        base.region().map(|r| r.to_string()).into_iter().collect()
    } else {
        regions.into_iter().map(str::to_owned).collect()
    };
    let searches = regions.iter().map(|region| {
        let config = base
            .clone()
            .into_builder()
            .region(Region::new(region.clone()))
            .build();
        async move { (region, discover_region(&config, profile, region).await) }
    });
    for (region, (found, errors)) in futures_util::future::join_all(searches).await {
        // Empty if the services that answered hold nothing; those that didn't are errors.
        if found.is_empty() && errors.len() < SERVICES.len() {
            discovery.empty_regions.push(region.clone());
        }
        discovery.found.extend(found);
        discovery.errors.extend(errors);
    }
    discovery
        .found
        .sort_by(|a, b| (a.source.region.as_str(), &a.label).cmp(&(&b.source.region, &b.label)));
    discovery
}

/// A failure from AWS itself, with its code when it gave one.
struct ApiFailure {
    code: Option<String>,
    message: String,
}

fn api_failure<E>(err: SdkError<E>) -> ApiFailure
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    match err.as_service_error() {
        Some(refused) => ApiFailure {
            code: refused.code().map(str::to_owned),
            message: refused.message().unwrap_or("no details given").to_owned(),
        },
        None => ApiFailure {
            code: None,
            message: causes(&err),
        },
    }
}

/// The services discovery searches in each region.
const SERVICES: [Service; 3] = [Service::Rds, Service::Redshift, Service::RedshiftServerless];

/// An AWS service that discovery lists databases in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    Rds,
    Redshift,
    RedshiftServerless,
}

impl Service {
    fn name(self) -> &'static str {
        match self {
            Service::Rds => "RDS",
            Service::Redshift => "Redshift",
            Service::RedshiftServerless => "Redshift Serverless",
        }
    }

    /// What it lists.
    fn listing(self) -> &'static str {
        match self {
            Service::Rds => "databases",
            Service::Redshift => "Redshift clusters",
            Service::RedshiftServerless => "Redshift Serverless workgroups",
        }
    }

    /// What the AWS identity needs to list them.
    fn permissions(self) -> &'static str {
        match self {
            Service::Rds => "rds:DescribeDBInstances and rds:DescribeDBClusters",
            Service::Redshift => "redshift:DescribeClusters",
            Service::RedshiftServerless => {
                "redshift-serverless:ListWorkgroups and redshift-serverless:ListNamespaces"
            }
        }
    }

    /// How a public database's certificate is checked.
    fn verified_note(self) -> &'static str {
        match self {
            Service::Rds => "Encryption: Verified, with Amazon's RDS certificates.",
            Service::Redshift | Service::RedshiftServerless => {
                "Encryption: Verified, with Amazon's public certificate authorities."
            }
        }
    }
}

/// What every service holds in `region`, and the services that couldn't be listed.
async fn discover_region(
    config: &SdkConfig,
    profile: &str,
    region: &str,
) -> (Vec<Discovered>, Vec<DiscoverError>) {
    let (rds, redshift, serverless) = tokio::join!(
        timeout(TIMEOUT, rds_in_region(config, profile, region)),
        timeout(TIMEOUT, redshift_in_region(config, profile, region)),
        timeout(TIMEOUT, serverless_in_region(config, profile, region)),
    );
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let [rds_service, redshift_service, serverless_service] = SERVICES;
    for (service, result) in [
        (rds_service, rds),
        (redshift_service, redshift),
        (serverless_service, serverless),
    ] {
        match result {
            Ok(Ok(more)) => found.extend(more),
            Ok(Err(e)) => errors.push(region_error(service, region, profile, e)),
            Err(_) => errors.push(DiscoverError {
                region: Some(region.to_owned()),
                kind: DiscoverErrorKind::TimedOut,
                message: format!("Timed out listing the {} in {region}.", service.listing()),
                detail: None,
            }),
        }
    }
    (found, errors)
}

async fn rds_in_region(
    config: &SdkConfig,
    profile: &str,
    region: &str,
) -> Result<Vec<Discovered>, ApiFailure> {
    let client = aws_sdk_rds::Client::new(config);
    let instances = client
        .describe_db_instances()
        .into_paginator()
        .items()
        .send()
        .try_collect()
        .await
        .map_err(api_failure)?;
    let clusters = client
        .describe_db_clusters()
        .into_paginator()
        .items()
        .send()
        .try_collect()
        .await
        .map_err(api_failure)?;
    Ok(found_in_region(&instances, &clusters, profile, region))
}

async fn redshift_in_region(
    config: &SdkConfig,
    profile: &str,
    region: &str,
) -> Result<Vec<Discovered>, ApiFailure> {
    let clusters: Vec<aws_sdk_redshift::types::Cluster> = aws_sdk_redshift::Client::new(config)
        .describe_clusters()
        .into_paginator()
        .items()
        .send()
        .try_collect()
        .await
        .map_err(api_failure)?;
    Ok(clusters
        .iter()
        .map(|cluster| from_redshift_cluster(cluster, profile, region))
        .collect())
}

async fn serverless_in_region(
    config: &SdkConfig,
    profile: &str,
    region: &str,
) -> Result<Vec<Discovered>, ApiFailure> {
    let client = aws_sdk_redshiftserverless::Client::new(config);
    let workgroups: Vec<Workgroup> = client
        .list_workgroups()
        .into_paginator()
        .items()
        .send()
        .try_collect()
        .await
        .map_err(api_failure)?;
    if workgroups.is_empty() {
        return Ok(Vec::new());
    }
    // A workgroup's database and admin user are its namespace's.
    let namespaces: Vec<Namespace> = client
        .list_namespaces()
        .into_paginator()
        .items()
        .send()
        .try_collect()
        .await
        .map_err(api_failure)?;
    Ok(workgroups
        .iter()
        .map(|workgroup| {
            let namespace = namespaces.iter().find(|n| {
                n.namespace_name().is_some() && n.namespace_name() == workgroup.namespace_name()
            });
            from_workgroup(workgroup, namespace, profile, region)
        })
        .collect())
}

/// Clusters once each, at their writer endpoint, and the instances outside any cluster.
fn found_in_region(
    instances: &[DbInstance],
    clusters: &[DbCluster],
    profile: &str,
    region: &str,
) -> Vec<Discovered> {
    let clusters = clusters.iter().map(|cluster| {
        // Aurora says whether a cluster is public on its instances.
        let members: Vec<&str> = cluster
            .db_cluster_members()
            .iter()
            .filter_map(|m| m.db_instance_identifier())
            .collect();
        let public = cluster.publicly_accessible().unwrap_or(false)
            || instances.iter().any(|i| {
                i.publicly_accessible() == Some(true)
                    && i.db_instance_identifier()
                        .is_some_and(|id| members.contains(&id))
            });
        from_cluster(cluster, public, profile, region)
    });
    let instances = instances
        .iter()
        .filter(|i| i.db_cluster_identifier().is_none())
        .map(|instance| from_instance(instance, profile, region));
    clusters.chain(instances).collect()
}

/// What a database's fields say, in the provider-neutral form.
struct Found<'a> {
    service: Service,
    label: &'a str,
    engine: &'a str,
    /// The engine's version, such as `16.3`, when the provider names one.
    version: Option<&'a str>,
    host: &'a str,
    port: Option<i32>,
    database: Option<&'a str>,
    user: Option<&'a str>,
    status: Option<&'a str>,
    public: bool,
    iam_enabled: bool,
    managed_secret: Option<&'a str>,
    arn: Option<&'a str>,
    /// For a cluster: its reader endpoint.
    readers: Option<&'a str>,
}

fn from_instance(instance: &DbInstance, profile: &str, region: &str) -> Discovered {
    let endpoint = instance.endpoint();
    discovered(
        Found {
            service: Service::Rds,
            label: instance.db_instance_identifier().unwrap_or_default(),
            engine: instance.engine().unwrap_or_default(),
            version: instance.engine_version(),
            host: endpoint.and_then(|e| e.address()).unwrap_or_default(),
            port: endpoint.and_then(|e| e.port()),
            database: instance.db_name(),
            user: instance.master_username(),
            status: instance.db_instance_status(),
            public: instance.publicly_accessible().unwrap_or(false),
            iam_enabled: instance.iam_database_authentication_enabled() == Some(true),
            managed_secret: instance.master_user_secret().and_then(|s| s.secret_arn()),
            arn: instance.db_instance_arn(),
            readers: None,
        },
        profile,
        region,
    )
}

fn from_cluster(cluster: &DbCluster, public: bool, profile: &str, region: &str) -> Discovered {
    discovered(
        Found {
            service: Service::Rds,
            label: cluster.db_cluster_identifier().unwrap_or_default(),
            engine: cluster.engine().unwrap_or_default(),
            version: cluster.engine_version(),
            host: cluster.endpoint().unwrap_or_default(),
            port: cluster.port(),
            database: cluster.database_name(),
            user: cluster.master_username(),
            status: cluster.status(),
            public,
            iam_enabled: cluster.iam_database_authentication_enabled() == Some(true),
            managed_secret: cluster.master_user_secret().and_then(|s| s.secret_arn()),
            arn: cluster.db_cluster_arn(),
            readers: cluster.reader_endpoint(),
        },
        profile,
        region,
    )
}

/// A provisioned Redshift cluster. Any cluster takes temporary IAM credentials
/// (`GetClusterCredentials`), so `iam_enabled` is always set.
fn from_redshift_cluster(
    cluster: &aws_sdk_redshift::types::Cluster,
    profile: &str,
    region: &str,
) -> Discovered {
    let endpoint = cluster.endpoint();
    discovered(
        Found {
            service: Service::Redshift,
            label: cluster.cluster_identifier().unwrap_or_default(),
            engine: REDSHIFT_ENGINE,
            version: cluster.cluster_version(),
            host: endpoint.and_then(|e| e.address()).unwrap_or_default(),
            port: endpoint.and_then(|e| e.port()),
            database: cluster.db_name(),
            user: cluster.master_username(),
            status: cluster.cluster_status(),
            public: cluster.publicly_accessible().unwrap_or(false),
            iam_enabled: true,
            managed_secret: cluster.master_password_secret_arn(),
            arn: cluster.cluster_namespace_arn(),
            readers: None,
        },
        profile,
        region,
    )
}

/// A Redshift Serverless workgroup, with its namespace's database and admin user when
/// known. Any workgroup takes temporary IAM credentials (`GetCredentials`).
fn from_workgroup(
    workgroup: &Workgroup,
    namespace: Option<&Namespace>,
    profile: &str,
    region: &str,
) -> Discovered {
    let endpoint = workgroup.endpoint();
    discovered(
        Found {
            service: Service::RedshiftServerless,
            label: workgroup.workgroup_name().unwrap_or_default(),
            engine: REDSHIFT_SERVERLESS_ENGINE,
            // Serverless has no version to choose.
            version: None,
            host: endpoint.and_then(|e| e.address()).unwrap_or_default(),
            port: endpoint.and_then(|e| e.port()).or(workgroup.port()),
            database: namespace.and_then(|n| n.db_name()),
            user: namespace.and_then(|n| n.admin_username()),
            status: workgroup.status().map(|s| s.as_str()),
            public: workgroup.publicly_accessible().unwrap_or(false),
            iam_enabled: true,
            managed_secret: namespace.and_then(|n| n.admin_password_secret_arn()),
            arn: workgroup.workgroup_arn(),
            readers: None,
        },
        profile,
        region,
    )
}

/// The engine names discovery gives Redshift, which RDS doesn't run.
const REDSHIFT_ENGINE: &str = "redshift";
const REDSHIFT_SERVERLESS_ENGINE: &str = "redshift-serverless";

/// The app's database for an RDS engine name, or Redshift's; `None` for the ones it
/// can't open yet.
pub fn adapter_for_engine(engine: &str) -> Option<Adapter> {
    match engine {
        "postgres" | "aurora-postgresql" => Some(Adapter::Postgres),
        // `aurora` is Aurora MySQL 5.6, from before the engine had its own name.
        "mysql" | "aurora-mysql" | "aurora" => Some(Adapter::Mysql),
        "mariadb" => Some(Adapter::Mariadb),
        REDSHIFT_ENGINE | REDSHIFT_SERVERLESS_ENGINE => Some(Adapter::Redshift),
        _ => None,
    }
}

/// How a found database should sign in: the password RDS keeps in Secrets Manager if it
/// keeps one, else an IAM token if the database takes them, else a password typed in.
fn suggested_auth(found: &Found, profile: &str, region: &str) -> Auth {
    if let Some(arn) = found.managed_secret {
        Auth::AwsSecret(AwsSecretRef {
            profile: profile.to_owned(),
            region: region.to_owned(),
            name: arn.to_owned(),
            use_secret_host: false,
        })
    } else if found.iam_enabled {
        Auth::AwsIam(AwsIamAuth {
            profile: profile.to_owned(),
            region: region.to_owned(),
        })
    } else {
        Auth::Password
    }
}

fn discovered(found: Found, profile: &str, region: &str) -> Discovered {
    let adapter = adapter_for_engine(found.engine);
    let status = found.status.unwrap_or("unknown").to_owned();
    let mut notes = Vec::new();
    if adapter.is_none() {
        notes.push(format!("{} is not supported yet.", found.engine));
    }
    if found.host.is_empty() {
        notes.push(format!("It has no endpoint yet ({status})."));
    }
    if let Some(readers) = found.readers {
        notes.push(format!(
            "Aurora cluster: connects to the writer. Readers are at {readers}."
        ));
    }
    if found.public {
        notes.push(found.service.verified_note().into());
    } else {
        notes.push(
            "Not reachable from the internet: connect through an SSH tunnel to a host in \
             its VPC. Encryption is Required rather than Verified, since through a tunnel \
             the certificate names another host."
                .into(),
        );
    }
    // Without a database, PostgreSQL and Redshift would look for one named after the
    // user. Redshift's first database is `dev` unless named otherwise.
    let database = match (found.database, adapter) {
        (Some(name), _) if !name.is_empty() => name.to_owned(),
        (_, Some(Adapter::Postgres)) => "postgres".to_owned(),
        (_, Some(Adapter::Redshift)) => "dev".to_owned(),
        _ => String::new(),
    };
    Discovered {
        label: found.label.to_owned(),
        adapter,
        engine: found.engine.to_owned(),
        version: found.version.unwrap_or_default().to_owned(),
        host: found.host.to_owned(),
        port: found
            .port
            .and_then(|p| u16::try_from(p).ok())
            .or(adapter.map(Adapter::default_port))
            .unwrap_or_default(),
        database,
        user: found.user.unwrap_or_default().to_owned(),
        status,
        public: found.public,
        iam_enabled: found.iam_enabled,
        managed_secret: found.managed_secret.map(str::to_owned),
        auth: suggested_auth(&found, profile, region),
        source: Source {
            provider: Provider::Aws,
            resource: found.arn.unwrap_or(found.label).to_owned(),
            region: region.to_owned(),
        },
        note: (!notes.is_empty()).then(|| notes.join(" ")),
    }
}

/// The `aws sso login` command for `profile`.
fn login_command(profile: &str) -> String {
    match profile {
        "" => "aws sso login".to_owned(),
        profile => format!("aws sso login --profile {profile}"),
    }
}

/// Why signing in failed, from [`sdk_config`]'s message.
fn sign_in_error(profile: &str, message: &str) -> DiscoverError {
    let who = match profile {
        "" => "your default AWS credentials".to_owned(),
        profile => format!("\"{profile}\""),
    };
    // What came after the summary, in parentheses: the technical cause.
    let cause = message
        .split_once(" (")
        .map(|(_, rest)| rest.strip_suffix(')').unwrap_or(rest).to_owned());
    let (kind, message, detail) = if message.contains("no AWS region") {
        (
            DiscoverErrorKind::NoRegion,
            "No region to search: add one, or set one in the AWS profile.".to_owned(),
            None,
        )
    } else if message.contains("is not in your AWS config") {
        (
            DiscoverErrorKind::Other,
            format!("The AWS profile {who} isn't in your AWS config."),
            cause,
        )
    } else if message.starts_with(SSO_SIGN_IN_NEEDED) {
        (
            DiscoverErrorKind::SignIn,
            format!(
                "Your AWS sign-in for {who} has expired or never happened. Run `{}`, then \
                 Retry.",
                login_command(profile)
            ),
            cause,
        )
    } else {
        (
            DiscoverErrorKind::SignIn,
            format!("Couldn't get AWS credentials for {who}."),
            cause.or_else(|| Some(message.to_owned())),
        )
    };
    DiscoverError {
        region: None,
        kind,
        message,
        detail,
    }
}

fn region_error(
    service: Service,
    region: &str,
    profile: &str,
    failure: ApiFailure,
) -> DiscoverError {
    let code = failure.code.as_deref().unwrap_or_default();
    let (kind, message) = match code {
        "AccessDenied" | "AccessDeniedException" | "UnauthorizedOperation" => (
            DiscoverErrorKind::AccessDenied,
            format!(
                "Not allowed to list {} in {region}: the AWS identity needs {} ({}).",
                service.listing(),
                service.permissions(),
                failure.message
            ),
        ),
        "ExpiredToken"
        | "ExpiredTokenException"
        | "RequestExpired"
        | "InvalidClientTokenId"
        | "UnrecognizedClientException" => (
            DiscoverErrorKind::SignIn,
            format!(
                "AWS refused the sign-in in {region} ({code}). If your session expired, run \
                 `{}` and try again.",
                login_command(profile)
            ),
        ),
        "" => (
            DiscoverErrorKind::Other,
            format!(
                "Couldn't reach {} in {region}: {}",
                service.name(),
                failure.message
            ),
        ),
        code => (
            DiscoverErrorKind::Other,
            format!(
                "{}: {code} in {region}: {}",
                service.name(),
                failure.message
            ),
        ),
    };
    DiscoverError {
        region: Some(region.to_owned()),
        kind,
        message,
        detail: None,
    }
}

/// What AWS clients are built from. Loads the profile and signs in once, so a missing or
/// expired session is reported as such instead of as a failed request. An empty `profile`
/// means the default AWS credentials; an empty `region` means the profile's.
pub async fn sdk_config(profile: &str, region: &str) -> Result<SdkConfig> {
    // `ring` keeps the build free of C libraries (the SDK's default TLS is aws-lc).
    let http = aws_smithy_http_client::Builder::new()
        .tls_provider(TlsProvider::Rustls(CryptoMode::Ring))
        .build_https();
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).http_client(http.clone());
    if !profile.is_empty() {
        loader = loader.profile_name(profile);
    }
    if !region.is_empty() {
        loader = loader.region(Region::new(region.to_owned()));
    }
    // The region: the one given, else the profile's, else the default chain's.
    let config = loader.load().await;
    // Before signing in: without a region, a role profile would fail at STS instead.
    if config.region().is_none() {
        if !profile_exists(profile).await {
            bail!("the AWS profile \"{profile}\" is not in your AWS config");
        }
        bail!("no AWS region: set one on the connection or in the AWS profile");
    }
    let provider = if profile.is_empty() {
        config
            .credentials_provider()
            .context("no AWS credentials are set up on this machine")?
    } else {
        // The default chain reads AWS_ACCESS_KEY_ID and friends before any profile, so
        // keys in the environment would win over a profile the user chose. A named
        // profile reads only that profile. A profile that assumes a role asks STS, which
        // needs the region.
        SharedCredentialsProvider::new(profile_credentials(profile, &config, http))
    };
    let credentials = match provider.provide_credentials().await {
        Ok(credentials) => credentials,
        Err(e) if !profile_exists(profile).await => {
            bail!(
                "the AWS profile \"{profile}\" is not in your AWS config ({})",
                causes(&e)
            )
        }
        Err(e) => {
            let cause = causes(&e);
            if is_sso_sign_in(&cause) {
                bail!(
                    "{SSO_SIGN_IN_NEEDED}: run `{}` ({cause})",
                    login_command(profile)
                );
            }
            bail!("no usable AWS credentials ({cause})")
        }
    };
    Ok(config
        .into_builder()
        .credentials_provider(SharedCredentialsProvider::new(credentials))
        .build())
}

/// Credentials from the profile named `profile` only, with `config`'s region and HTTP
/// client for whatever it has to ask, such as STS for a role.
fn profile_credentials(
    profile: &str,
    config: &SdkConfig,
    http: aws_sdk_ssm::config::SharedHttpClient,
) -> ProfileFileCredentialsProvider {
    let provider_config = provider_config(config, http);
    ProfileFileCredentialsProvider::builder()
        .configure(&provider_config)
        .profile_name(profile)
        .build()
}

fn provider_config(
    config: &SdkConfig,
    http: aws_sdk_ssm::config::SharedHttpClient,
) -> ProviderConfig {
    ProviderConfig::default()
        .with_http_client(http)
        .with_region(config.region().cloned())
}

/// The start of the error when an AWS SSO session has expired or was never started.
const SSO_SIGN_IN_NEEDED: &str = "the AWS sign-in has expired or never happened";

/// Whether a credentials failure is an SSO session that needs `aws sso login`. The SDK's
/// SSO error types are private, so this matches what they say: the SSO token (missing,
/// unreadable or expired), its cache file, refreshing it through SSO OIDC, or the SSO
/// service refusing it. Not a profile's name, and not other expired credentials.
fn is_sso_sign_in(cause: &str) -> bool {
    let cause = cause.to_ascii_lowercase();
    [
        "sso token",
        "sso oidc",
        "/.aws/sso/cache/",
        "\\.aws\\sso\\cache\\",
        "unauthorizedexception: session token not found or invalid",
    ]
    .iter()
    .any(|phrase| cause.contains(phrase))
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
    use crate::config::TlsMode;
    use aws_sdk_rds::config::Credentials;
    use aws_sdk_rds::types::{DbClusterMember, Endpoint, MasterUserSecret};

    fn instance(id: &str, engine: &str) -> aws_sdk_rds::types::builders::DbInstanceBuilder {
        DbInstance::builder()
            .db_instance_identifier(id)
            .engine(engine)
            .db_instance_status("available")
            .master_username("admin")
            .db_instance_arn(format!("arn:aws:rds:eu-west-1:123456789012:db:{id}"))
            .endpoint(
                Endpoint::builder()
                    .address(format!("{id}.abc.eu-west-1.rds.amazonaws.com"))
                    .port(5432)
                    .build(),
            )
    }

    #[test]
    fn engines_map_to_the_apps_databases() {
        assert_eq!(adapter_for_engine("postgres"), Some(Adapter::Postgres));
        assert_eq!(
            adapter_for_engine("aurora-postgresql"),
            Some(Adapter::Postgres)
        );
        assert_eq!(adapter_for_engine("mysql"), Some(Adapter::Mysql));
        assert_eq!(adapter_for_engine("aurora-mysql"), Some(Adapter::Mysql));
        assert_eq!(adapter_for_engine("mariadb"), Some(Adapter::Mariadb));
        assert_eq!(adapter_for_engine("redshift"), Some(Adapter::Redshift));
        assert_eq!(
            adapter_for_engine("redshift-serverless"),
            Some(Adapter::Redshift)
        );
        for other in ["oracle-ee", "sqlserver-se", "db2-se", "docdb", "neptune"] {
            assert_eq!(adapter_for_engine(other), None, "{other}");
        }
    }

    #[test]
    fn an_aurora_cluster_is_listed_once_at_its_writer() {
        let cluster = DbCluster::builder()
            .db_cluster_identifier("shop")
            .engine("aurora-postgresql")
            .endpoint("shop.cluster-abc.eu-west-1.rds.amazonaws.com")
            .reader_endpoint("shop.cluster-ro-abc.eu-west-1.rds.amazonaws.com")
            .port(5432)
            .status("available")
            .master_username("admin")
            .db_cluster_members(
                DbClusterMember::builder()
                    .db_instance_identifier("shop-1")
                    .build(),
            )
            .db_cluster_members(
                DbClusterMember::builder()
                    .db_instance_identifier("shop-2")
                    .build(),
            )
            .build();
        let instances = [
            instance("shop-1", "aurora-postgresql")
                .db_cluster_identifier("shop")
                .publicly_accessible(true)
                .build(),
            instance("shop-2", "aurora-postgresql")
                .db_cluster_identifier("shop")
                .build(),
            instance("orders", "postgres").build(),
        ];
        let found = found_in_region(&instances, &[cluster], "prod", "eu-west-1");
        let labels: Vec<_> = found.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, ["shop", "orders"]);
        let shop = &found[0];
        assert_eq!(shop.host, "shop.cluster-abc.eu-west-1.rds.amazonaws.com");
        assert!(shop.public, "a public member makes the cluster reachable");
        assert!(shop.note.as_deref().unwrap().contains("cluster-ro-abc"));
        assert_eq!(shop.database, "postgres");
        assert!(!found[1].public);
        assert!(found[1].note.as_deref().unwrap().contains("SSH tunnel"));
    }

    #[test]
    fn the_managed_secret_wins_then_iam_then_a_password() {
        let secret = "arn:aws:secretsmanager:eu-west-1:123456789012:secret:rds!db-1";
        let managed = instance("a", "postgres")
            .iam_database_authentication_enabled(true)
            .master_user_secret(MasterUserSecret::builder().secret_arn(secret).build())
            .build();
        let iam = instance("b", "mysql")
            .iam_database_authentication_enabled(true)
            .build();
        let plain = instance("c", "mariadb").publicly_accessible(true).build();
        let found = found_in_region(&[managed, iam, plain], &[], "prod", "eu-west-1");
        assert_eq!(
            found[0].auth,
            Auth::AwsSecret(AwsSecretRef {
                profile: "prod".into(),
                region: "eu-west-1".into(),
                name: secret.into(),
                use_secret_host: false,
            })
        );
        assert_eq!(found[0].managed_secret.as_deref(), Some(secret));
        assert_eq!(
            found[1].auth,
            Auth::AwsIam(AwsIamAuth {
                profile: "prod".into(),
                region: "eu-west-1".into(),
            })
        );
        assert_eq!(found[2].auth, Auth::Password);
        assert!(found[2].note.as_deref().unwrap().contains("Verified"));
        assert_eq!(found[2].connection().unwrap().tls, TlsMode::Verify);
        let conn = found[1].connection().unwrap();
        assert_eq!(
            (conn.adapter, conn.user.as_str()),
            (Adapter::Mysql, "admin")
        );
        assert_eq!(conn.tls, TlsMode::Require);
        assert_eq!(
            conn.source.unwrap().resource,
            "arn:aws:rds:eu-west-1:123456789012:db:b"
        );
    }

    #[test]
    fn unsupported_and_unfinished_databases_are_listed_but_not_usable() {
        let oracle = instance("legacy", "oracle-ee").build();
        let creating = DbInstance::builder()
            .db_instance_identifier("new")
            .engine("postgres")
            .db_instance_status("creating")
            .build();
        let found = found_in_region(&[oracle, creating], &[], "", "eu-west-1");
        assert!(!found[0].usable());
        assert!(
            found[0]
                .note
                .as_deref()
                .unwrap()
                .contains("not supported yet")
        );
        assert!(!found[1].usable());
        assert!(found[1].note.as_deref().unwrap().contains("creating"));
    }

    #[test]
    fn discovery_errors_say_what_to_do() {
        // As `sdk_config` words it.
        let cause = "failed to read `/root/.aws/sso/cache/abc.json`: No such file";
        let expired = sign_in_error(
            "prod-sso",
            &format!("{SSO_SIGN_IN_NEEDED}: run `aws sso login --profile prod-sso` ({cause})"),
        );
        assert_eq!(expired.kind, DiscoverErrorKind::SignIn);
        assert_eq!(
            expired.message,
            "Your AWS sign-in for \"prod-sso\" has expired or never happened. Run \
             `aws sso login --profile prod-sso`, then Retry."
        );
        assert_eq!(expired.detail.as_deref(), Some(cause));
        assert_eq!(expired.region, None);
        assert!(is_sso_sign_in(cause));

        let role = sign_in_error("admin", "no usable AWS credentials (AccessDenied from STS)");
        assert_eq!(role.message, "Couldn't get AWS credentials for \"admin\".");
        assert!(
            !role.message.contains("sso"),
            "no SSO hint for another failure"
        );
        assert_eq!(role.detail.as_deref(), Some("AccessDenied from STS"));
        assert!(!is_sso_sign_in("AccessDenied from STS"));
        // What the SDK says when the SSO sign-in is the problem.
        for cause in [
            "the SSO token has expired and cannot be refreshed",
            "failed to load the cached SSO token: invalid JSON in cached SSO token file",
            "call to SSO OIDC CreateToken for SSO token refresh failed: dispatch failure",
            "service error: UnauthorizedException: Session token not found or invalid",
            r"failed to read `C:\Users\me\.aws\sso\cache\abc.json`: not found",
        ] {
            assert!(is_sso_sign_in(cause), "{cause}");
        }
        // Not for a profile that merely has "sso" in its name, nor other expired keys.
        for cause in [
            "profile `prod-sso` assumes a role: AccessDenied: not authorized",
            "ExpiredToken: The security token included in the request is expired",
            "the token has expired",
        ] {
            assert!(!is_sso_sign_in(cause), "{cause}");
        }

        let no_region = sign_in_error("", "no AWS region: set one on the connection");
        assert_eq!(no_region.kind, DiscoverErrorKind::NoRegion);

        let denied = region_error(
            Service::Rds,
            "eu-west-1",
            "prod",
            ApiFailure {
                code: Some("AccessDenied".into()),
                message: "User is not authorized to perform: rds:DescribeDBInstances".into(),
            },
        );
        assert_eq!(denied.kind, DiscoverErrorKind::AccessDenied);
        assert!(denied.message.contains("rds:DescribeDBClusters"));
        assert_eq!(denied.region.as_deref(), Some("eu-west-1"));

        let expired = region_error(
            Service::Rds,
            "us-east-1",
            "",
            ApiFailure {
                code: Some("ExpiredTokenException".into()),
                message: "expired".into(),
            },
        );
        assert_eq!(expired.kind, DiscoverErrorKind::SignIn);
        assert!(expired.message.contains("`aws sso login`"));

        let offline = region_error(
            Service::Rds,
            "eu-west-1",
            "",
            ApiFailure {
                code: None,
                message: "dispatch failure".into(),
            },
        );
        assert_eq!(offline.kind, DiscoverErrorKind::Other);
        assert!(offline.message.contains("dispatch failure"));
    }

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
        // What botocore's `generate_db_auth_token` (as in `aws rds generate-db-auth-token`)
        // makes from the same credentials, endpoint and clock.
        assert_eq!(
            token,
            "orders.abc123.eu-west-1.rds.amazonaws.com:5432/?Action=connect&DBUser=app_user\
             &X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20240826%2F\
             eu-west-1%2Frds-db%2Faws4_request&X-Amz-Date=20240826T220000Z&X-Amz-Expires=900\
             &X-Amz-SignedHeaders=host&X-Amz-Signature=\
             d979106687899672efbbf170f18a2890b79e9467e94cd3abe996861da08e6264"
        );
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
    fn a_redshift_endpoint_names_its_cluster_or_workgroup_and_region() {
        assert_eq!(
            redshift_target("examplecluster.abc123xyz789.us-west-2.redshift.amazonaws.com"),
            Some((
                RedshiftTarget::Cluster("examplecluster".into()),
                "us-west-2".into()
            ))
        );
        assert_eq!(
            redshift_target("Default-WG.123456789012.EU-WEST-1.redshift-serverless.amazonaws.com."),
            Some((
                RedshiftTarget::Workgroup("default-wg".into()),
                "eu-west-1".into()
            ))
        );
        assert_eq!(
            redshift_target("dw.abc.cn-north-1.redshift.amazonaws.com.cn"),
            Some((RedshiftTarget::Cluster("dw".into()), "cn-north-1".into()))
        );
        for other in [
            "orders.abc.eu-west-1.rds.amazonaws.com",
            "dw.example.com",
            "redshift.amazonaws.com",
            "eu-west-1.redshift.amazonaws.com",
            "dw.eu-west-1.redshift.amazonaws.com.example.com",
        ] {
            assert_eq!(redshift_target(other), None, "{other}");
        }
        assert_eq!(
            region_of_host("dw.abc.ap-south-1.redshift.amazonaws.com").as_deref(),
            Some("ap-south-1")
        );
        // Amazon's RDS authorities are for RDS hosts only.
        assert!(!is_rds_host("dw.abc.ap-south-1.redshift.amazonaws.com"));
    }

    #[test]
    fn a_cluster_is_asked_for_an_existing_user_for_fifteen_minutes() {
        let client = aws_sdk_redshift::Client::new(&fixed_config("eu-west-1"));
        let request = cluster_credentials(&client, "dw", "app", "sales");
        let input = request.as_input();
        assert_eq!(input.get_cluster_identifier().as_deref(), Some("dw"));
        assert_eq!(input.get_db_user().as_deref(), Some("app"));
        assert_eq!(input.get_db_name().as_deref(), Some("sales"));
        // Never creates a user that doesn't exist.
        assert_eq!(*input.get_auto_create(), Some(false));
        assert_eq!(*input.get_duration_seconds(), Some(900));
        assert!(input.get_db_groups().is_none());
        let any_database = cluster_credentials(&client, "dw", "app", "");
        assert_eq!(*any_database.as_input().get_db_name(), None);
    }

    #[test]
    fn a_workgroup_is_asked_without_a_user() {
        let client = aws_sdk_redshiftserverless::Client::new(&fixed_config("eu-west-1"));
        let request = workgroup_credentials(&client, "analytics", "dev");
        let input = request.as_input();
        assert_eq!(input.get_workgroup_name().as_deref(), Some("analytics"));
        assert_eq!(input.get_db_name().as_deref(), Some("dev"));
        assert_eq!(*input.get_duration_seconds(), Some(900));
        assert_eq!(*input.get_custom_domain_name(), None);
    }

    #[test]
    fn a_redshift_login_takes_the_user_password_and_expiry_aws_returns() {
        use aws_sdk_redshift::primitives::DateTime;
        let at = DateTime::from_secs(1_724_710_500);
        let login = redshift_login(Some("IAMR:analyst"), Some("p4ss"), Some(&at)).unwrap();
        assert_eq!(login.user, "IAMR:analyst");
        assert_eq!(login.password, "p4ss");
        assert_eq!(
            login.expires,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_710_500))
        );
        assert!(redshift_login(None, Some("p"), None).is_err());
        assert!(redshift_login(Some("IAM:a"), Some(""), None).is_err());
        assert!(
            redshift_login(Some("IAM:a"), Some("p"), None)
                .unwrap()
                .expires
                .is_none()
        );
    }

    #[tokio::test]
    async fn redshift_credentials_need_a_redshift_endpoint() {
        // Refused before asking AWS for anything.
        let error = redshift_credentials("", "eu-west-1", "dw.example.com", "app", "dev")
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("*.redshift.amazonaws.com"), "{error}");
    }

    #[test]
    fn redshift_clusters_and_workgroups_are_found_with_their_sign_in() {
        use aws_sdk_redshift::types::{Cluster, Endpoint as ClusterEndpoint};
        use aws_sdk_redshiftserverless::types::{Endpoint as WorkgroupEndpoint, WorkgroupStatus};
        let cluster = Cluster::builder()
            .cluster_identifier("dw")
            .cluster_status("available")
            .master_username("awsuser")
            .publicly_accessible(true)
            .cluster_namespace_arn("arn:aws:redshift:eu-west-1:123456789012:namespace:abc")
            .endpoint(
                ClusterEndpoint::builder()
                    .address("dw.abc123.eu-west-1.redshift.amazonaws.com")
                    .port(5439)
                    .build(),
            )
            .build();
        let found = from_redshift_cluster(&cluster, "prod", "eu-west-1");
        assert_eq!(found.adapter, Some(Adapter::Redshift));
        assert_eq!(found.engine, "redshift");
        assert_eq!(found.database, "dev", "Redshift's first database");
        assert_eq!((found.port, found.user.as_str()), (5439, "awsuser"));
        assert_eq!(
            found.auth,
            Auth::AwsIam(AwsIamAuth {
                profile: "prod".into(),
                region: "eu-west-1".into(),
            })
        );
        assert!(
            found
                .note
                .as_deref()
                .unwrap()
                .contains("public certificate")
        );
        let conn = found.connection().unwrap();
        assert_eq!(conn.tls, TlsMode::Verify);
        assert_eq!(conn.adapter, Adapter::Redshift);

        let managed = Cluster::builder()
            .cluster_identifier("dw2")
            .db_name("sales")
            .master_password_secret_arn("arn:aws:secretsmanager:eu-west-1:1:secret:redshift!dw2")
            .build();
        let found = from_redshift_cluster(&managed, "prod", "eu-west-1");
        assert!(matches!(&found.auth, Auth::AwsSecret(s) if s.name.ends_with("redshift!dw2")));
        assert_eq!(found.database, "sales");
        assert!(!found.usable(), "no endpoint yet");

        let workgroup = Workgroup::builder()
            .workgroup_name("analytics")
            .namespace_name("ns")
            .status(WorkgroupStatus::Available)
            .workgroup_arn("arn:aws:redshift-serverless:eu-west-1:1:workgroup/x")
            .endpoint(
                WorkgroupEndpoint::builder()
                    .address("analytics.123456789012.eu-west-1.redshift-serverless.amazonaws.com")
                    .port(5439)
                    .build(),
            )
            .build();
        let namespace = Namespace::builder()
            .namespace_name("ns")
            .db_name("warehouse")
            .admin_username("admin")
            .build();
        let found = from_workgroup(&workgroup, Some(&namespace), "", "eu-west-1");
        assert_eq!(found.engine, "redshift-serverless");
        assert_eq!(found.status, "AVAILABLE");
        assert_eq!(
            (found.database.as_str(), found.user.as_str()),
            ("warehouse", "admin")
        );
        assert!(found.note.as_deref().unwrap().contains("SSH tunnel"));
        assert_eq!(found.connection().unwrap().tls, TlsMode::Require);
        let alone = from_workgroup(&workgroup, None, "", "eu-west-1");
        assert_eq!(alone.database, "dev");
        assert!(matches!(alone.auth, Auth::AwsIam(_)));
    }

    #[test]
    fn a_refused_redshift_listing_names_its_own_permissions() {
        let failure = || ApiFailure {
            code: Some("AccessDeniedException".into()),
            message: "not authorized".into(),
        };
        let cluster = region_error(Service::Redshift, "eu-west-1", "prod", failure());
        assert_eq!(cluster.kind, DiscoverErrorKind::AccessDenied);
        assert!(
            cluster.message.contains("redshift:DescribeClusters"),
            "{}",
            cluster.message
        );
        assert!(!cluster.message.contains("rds:"), "{}", cluster.message);
        let serverless = region_error(Service::RedshiftServerless, "eu-west-1", "prod", failure());
        assert!(
            serverless
                .message
                .contains("redshift-serverless:ListWorkgroups")
        );
        let unknown = region_error(
            Service::RedshiftServerless,
            "eu-west-1",
            "",
            ApiFailure {
                code: Some("UnknownOperationException".into()),
                message: "x".into(),
            },
        );
        assert!(
            unknown.message.starts_with("Redshift Serverless: "),
            "{}",
            unknown.message
        );
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
        // Named outright: `~` would expand with `\` on Windows.
        let env = Env::from_slice(&[
            ("AWS_CONFIG_FILE", "/home/me/.aws/config"),
            ("AWS_SHARED_CREDENTIALS_FILE", "/home/me/.aws/credentials"),
        ]);
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
