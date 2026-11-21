//! Connection settings, saved as JSON in the user's config directory.
//!
//! The file never holds secrets; passwords live in the OS keychain (see
//! [`crate::secrets`]).

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, anyhow, bail};
use serde::ser::SerializeSeq as _;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

/// Separates folder levels in a stored path, e.g. `RGG/storefront/bear`.
const FOLDER_SEPARATOR: char = '/';

/// The folder a folder path sits in; `""` at the top level.
/// Whether `host` is a Redshift Serverless workgroup's own endpoint,
/// `<workgroup>.<account>.<region>.redshift-serverless.amazonaws.com` (`.com.cn` in China).
pub fn is_redshift_serverless_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let rest = host
        .strip_suffix(".redshift-serverless.amazonaws.com")
        .or_else(|| host.strip_suffix(".redshift-serverless.amazonaws.com.cn"));
    // At least the workgroup, the account and the region.
    rest.is_some_and(|rest| rest.split('.').filter(|label| !label.is_empty()).count() >= 3)
}

pub fn parent_of(path: &str) -> &str {
    path.rsplit_once(FOLDER_SEPARATOR)
        .map_or("", |(parent, _)| parent)
}

/// The last level of a folder path.
pub fn folder_name(path: &str) -> &str {
    path.rsplit(FOLDER_SEPARATOR).next().unwrap_or(path)
}

fn join_folder(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}{FOLDER_SEPARATOR}{name}")
    }
}

/// `path` moved from under `old` to under `new`, or `None` if it isn't inside `old`.
pub fn rebase_folder(path: &str, old: &str, new: &str) -> Option<String> {
    if path == old {
        return Some(new.to_owned());
    }
    let rest = path.strip_prefix(old)?.strip_prefix(FOLDER_SEPARATOR)?;
    Some(join_folder(new, rest))
}

/// File name inside [`config_dir`].
pub const CONFIG_FILE: &str = "connections.json";

/// Overrides [`config_dir`]; used by tests and to point the MCP server elsewhere.
pub const CONFIG_DIR_ENV: &str = "TUPLEBASE_CONFIG_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Adapter {
    Postgres,
    Mariadb,
    Mysql,
    Sqlite,
    Redshift,
    Mongodb,
    Documentdb,
}

/// How a database quotes an identifier such as a table name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentQuote {
    /// `"name"`, with `"` doubled inside: standard SQL.
    Double,
    /// `` `name` ``, with `` ` `` doubled inside: MySQL and MariaDB.
    Backtick,
}

/// Which rules `safety` follows to read SQL: to cut a script into statements, and to
/// find the code between the comments and strings for the read-only guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptFlavor {
    /// Dollar quotes, nested block comments, `--` comments that also end at a carriage
    /// return, and `E'...'` escape strings; backticks are not quotes.
    Postgres,
    /// PostgreSQL's rules, except that a backslash escapes the next character in every
    /// `'...'` string, not only in `E'...'`: Redshift reads `'a\'b'` as one string.
    Redshift,
    /// `#` comments, `--` comments only before a space, backslash escapes, executable
    /// `/*! ... */` comments, and routine bodies kept whole.
    Mysql,
    /// Plain SQL quotes and comments, and `[name]` and `` `name` `` quoted names.
    Sqlite,
    /// Mongo shell commands: `'`, `"` and backtick strings with backslash escapes, `//`
    /// and `/* */` comments, and `/regex/` where a value may start. A statement ends at
    /// `;` or at a line break outside brackets, unless the next line starts with `.`.
    /// Read by the same scanner as the parser, `mongo_shell::split`.
    MongoShell,
}

/// The kind of query language and data model an engine has. Decides which runner,
/// editor language and result shape apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Tables of rows, queried with SQL.
    Sql,
    /// Collections of documents, such as MongoDB.
    Document,
    /// Tables of items reached by key, such as DynamoDB.
    KeyValue,
}

/// Where an engine's data lives, which decides the fields a connection needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// A server reached by host and port, with a user and password.
    Server,
    /// A local file, with no server, user or password.
    File,
    /// A service in an AWS region, signed in with an AWS profile: no host, port, user
    /// or password.
    AwsRegion,
}

/// What every part of TupleBase may need to know about an engine without talking to it.
/// One constant per engine, chosen by [`Adapter::info`]. The SQL that `db` runs on an
/// engine lives in that engine's module under `db/` (ARCHITECTURE.md, L7).
#[derive(Debug, PartialEq, Eq)]
pub struct EngineInfo {
    /// The name shown to people, such as `PostgreSQL`.
    pub label: &'static str,
    /// The port a server listens on unless told otherwise; 0 for a file database.
    pub default_port: u16,
    pub family: Family,
    pub place: Place,
    /// The user a new server usually has, shown as an example in an empty User field;
    /// empty for a file database.
    pub user_hint: &'static str,
    pub quote: IdentQuote,
    pub script: ScriptFlavor,
}

const POSTGRES: EngineInfo = EngineInfo {
    label: "PostgreSQL",
    default_port: 5432,
    family: Family::Sql,
    place: Place::Server,
    user_hint: "postgres",
    quote: IdentQuote::Double,
    script: ScriptFlavor::Postgres,
};

const MARIADB: EngineInfo = EngineInfo {
    label: "MariaDB",
    default_port: 3306,
    family: Family::Sql,
    place: Place::Server,
    user_hint: "root",
    quote: IdentQuote::Backtick,
    script: ScriptFlavor::Mysql,
};

const MYSQL: EngineInfo = EngineInfo {
    label: "MySQL",
    default_port: 3306,
    family: Family::Sql,
    place: Place::Server,
    user_hint: "root",
    quote: IdentQuote::Backtick,
    script: ScriptFlavor::Mysql,
};

const SQLITE: EngineInfo = EngineInfo {
    label: "SQLite",
    default_port: 0,
    family: Family::Sql,
    place: Place::File,
    user_hint: "",
    quote: IdentQuote::Double,
    script: ScriptFlavor::Sqlite,
};

const REDSHIFT: EngineInfo = EngineInfo {
    label: "Redshift",
    default_port: 5439,
    family: Family::Sql,
    place: Place::Server,
    user_hint: "awsuser",
    quote: IdentQuote::Double,
    script: ScriptFlavor::Redshift,
};

const MONGODB: EngineInfo = EngineInfo {
    label: "MongoDB",
    default_port: 27017,
    family: Family::Document,
    place: Place::Server,
    user_hint: "admin",
    // No Mongo path quotes a name; this only keeps the field filled in.
    quote: IdentQuote::Double,
    script: ScriptFlavor::MongoShell,
};

const DOCUMENTDB: EngineInfo = EngineInfo {
    label: "Amazon DocumentDB",
    default_port: 27017,
    family: Family::Document,
    place: Place::Server,
    user_hint: "docdbadmin",
    quote: IdentQuote::Double,
    script: ScriptFlavor::MongoShell,
};

impl Adapter {
    pub const ALL: [Adapter; 7] = [
        Adapter::Postgres,
        Adapter::Mariadb,
        Adapter::Mysql,
        Adapter::Sqlite,
        Adapter::Redshift,
        Adapter::Mongodb,
        Adapter::Documentdb,
    ];

    /// This engine's static facts. A new engine adds its constant here.
    pub fn info(self) -> &'static EngineInfo {
        match self {
            Adapter::Postgres => &POSTGRES,
            Adapter::Mariadb => &MARIADB,
            Adapter::Mysql => &MYSQL,
            Adapter::Sqlite => &SQLITE,
            Adapter::Redshift => &REDSHIFT,
            Adapter::Mongodb => &MONGODB,
            Adapter::Documentdb => &DOCUMENTDB,
        }
    }

    /// Whether the database is a local file, with no server, user or password.
    pub fn is_file(self) -> bool {
        self.info().place == Place::File
    }

    pub fn family(self) -> Family {
        self.info().family
    }

    pub fn label(self) -> &'static str {
        self.info().label
    }

    /// Quotes an identifier for this database.
    pub fn quote_ident(self, ident: &str) -> String {
        match self.info().quote {
            IdentQuote::Double => format!("\"{}\"", ident.replace('"', "\"\"")),
            IdentQuote::Backtick => format!("`{}`", ident.replace('`', "``")),
        }
    }

    /// The query showing the first rows of a table, in the engine's own language.
    pub fn preview_query(self, schema: &str, table: &str) -> String {
        preview_query(
            self.info().family,
            |ident| self.quote_ident(ident),
            schema,
            table,
        )
    }

    pub fn default_port(self) -> u16 {
        self.info().default_port
    }

    pub fn user_hint(self) -> &'static str {
        self.info().user_hint
    }
}

/// [`Adapter::preview_query`] for an engine of `family` that quotes names with `quote`.
/// `schema` is the database or region for a non-SQL engine.
fn preview_query(
    family: Family,
    quote: impl Fn(&str) -> String,
    schema: &str,
    table: &str,
) -> String {
    match family {
        Family::Sql => format!("SELECT * FROM {}.{} LIMIT 100", quote(schema), quote(table)),
        Family::Document => {
            let string = |s: &str| Value::from(s).to_string();
            format!(
                "db.getSiblingDB({}).getCollection({}).find({{}}).limit(100)",
                string(schema),
                string(table)
            )
        }
        // PartiQL has no LIMIT: the row cap is applied while paging.
        Family::KeyValue => format!("SELECT * FROM {}", quote(table)),
    }
}

/// How strictly to require an encrypted connection to the database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Encrypt if the server supports it.
    #[default]
    Prefer,
    /// Refuse to connect without encryption.
    Require,
    /// Also verify the server's certificate and hostname.
    Verify,
}

impl TlsMode {
    pub const ALL: [TlsMode; 3] = [TlsMode::Prefer, TlsMode::Require, TlsMode::Verify];

    pub fn label(self) -> &'static str {
        match self {
            TlsMode::Prefer => "If available",
            TlsMode::Require => "Required",
            TlsMode::Verify => "Verified",
        }
    }

    fn is_default(&self) -> bool {
        *self == TlsMode::default()
    }
}

/// Reach the database through this SSH server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    /// Private key file. Empty means password authentication.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_path: String,
    /// The server's key as `SHA256:…`; connections are refused if it doesn't match.
    pub fingerprint: String,
}

fn default_ssh_port() -> u16 {
    22
}

/// The AWS service that holds a connection's credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwsStore {
    SecretsManager,
    ParameterStore,
}

impl AwsStore {
    pub fn label(self) -> &'static str {
        match self {
            AwsStore::SecretsManager => "AWS Secrets Manager",
            AwsStore::ParameterStore => "AWS Parameter Store",
        }
    }
}

/// Where in AWS a secret or parameter is. Only this pointer is saved; the value is read
/// on every connect (see [`crate::cloud::aws`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsSecretRef {
    /// Profile in `~/.aws/config`. Empty means the default AWS credentials.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    /// Empty means the profile's region.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    /// Secret name or ARN, or parameter name.
    pub name: String,
    /// Also take the host, port and database from the secret.
    #[serde(default)]
    pub use_secret_host: bool,
}

/// Sign in to the database with a short-lived RDS IAM token made from an AWS profile, or on
/// Redshift with temporary credentials AWS hands out for it (`GetClusterCredentials`, or
/// Redshift Serverless's `GetCredentials`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsIamAuth {
    /// Profile in `~/.aws/config`. Empty means the default AWS credentials.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    /// The database's region. Empty means the one in its `*.rds.amazonaws.com` (or
    /// Redshift) host name, or else the profile's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
}

/// Where a connection's password comes from. Saved as `auth`, tagged by `type`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    /// The OS credential store (see [`crate::secrets`]). Not written to the file.
    #[default]
    Password,
    /// A secret in AWS Secrets Manager.
    AwsSecret(AwsSecretRef),
    /// A parameter in AWS Systems Manager Parameter Store.
    AwsParameter(AwsSecretRef),
    /// An RDS IAM auth token for the connection's user, made on every connect; on Redshift,
    /// temporary credentials for it.
    AwsIam(AwsIamAuth),
}

impl Auth {
    pub fn is_password(&self) -> bool {
        *self == Auth::Password
    }

    pub fn kind(&self) -> AuthKind {
        match self {
            Auth::Password => AuthKind::Password,
            Auth::AwsSecret(_) => AuthKind::AwsSecret,
            Auth::AwsParameter(_) => AuthKind::AwsParameter,
            Auth::AwsIam(_) => AuthKind::AwsIam,
        }
    }

    /// The secret or parameter it reads, and the store that holds it.
    pub fn secret(&self) -> Option<(AwsStore, &AwsSecretRef)> {
        match self {
            Auth::AwsSecret(secret) => Some((AwsStore::SecretsManager, secret)),
            Auth::AwsParameter(parameter) => Some((AwsStore::ParameterStore, parameter)),
            Auth::Password | Auth::AwsIam(_) => None,
        }
    }

    /// The AWS profile and region it uses, if it uses AWS.
    pub fn aws_profile(&self) -> Option<(&str, &str)> {
        match self {
            Auth::AwsSecret(r) | Auth::AwsParameter(r) => Some((&r.profile, &r.region)),
            Auth::AwsIam(iam) => Some((&iam.profile, &iam.region)),
            Auth::Password => None,
        }
    }
}

/// The choices the connection form offers, one per [`Auth`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Password,
    AwsSecret,
    AwsParameter,
    AwsIam,
}

impl AuthKind {
    pub const ALL: [AuthKind; 4] = [
        AuthKind::Password,
        AuthKind::AwsSecret,
        AuthKind::AwsParameter,
        AuthKind::AwsIam,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AuthKind::Password => "Password",
            AuthKind::AwsSecret => "Secrets Manager",
            AuthKind::AwsParameter => "Parameter Store",
            AuthKind::AwsIam => "IAM token",
        }
    }
}

/// The cloud a connection was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Aws,
}

/// Where a connection was discovered, so it can be found there again. Never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub provider: Provider,
    /// The resource's identifier there, such as an RDS ARN.
    pub resource: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
}

/// What a MongoDB or DocumentDB connection needs besides host, port, user and database.
/// Never a secret: the password stays in the OS credential store (S1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MongoSettings {
    /// Find the servers through DNS (`mongodb+srv://`); the port is then not used.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub srv: bool,
    /// Connection string options, such as `authSource=admin&replicaSet=rs0`. Never a
    /// password or a session token: [`Connection::validate`] refuses them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub options: String,
}

/// Whether an option would put a secret into the saved, plain options string (S1): any
/// key naming a password. `key` is lower-cased and percent-decoded.
fn is_secret_option(key: &str) -> bool {
    key.contains("password") || key == "pwd"
}

impl MongoSettings {
    /// Each option as a lower-cased, percent-decoded key and its decoded value.
    pub fn pairs(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.options
            .trim_start_matches('?')
            .split('&')
            .filter(|pair| !pair.trim().is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (
                    percent_decode(key).trim().to_ascii_lowercase(),
                    percent_decode(value),
                )
            })
    }

    /// Refuses options that would put a secret in the saved settings.
    pub fn check(&self) -> Result<()> {
        for (key, value) in self.pairs() {
            if is_secret_option(&key) {
                bail!(
                    "the password can't go in the connection options: they are saved as \
                     plain text. Leave it to the password field"
                );
            }
            if key == "authmechanismproperties"
                && value.to_ascii_uppercase().contains("AWS_SESSION_TOKEN")
            {
                bail!("an AWS session token can't go in the connection options");
            }
        }
        Ok(())
    }
}

/// What a pasted `mongodb://` or `mongodb+srv://` connection string holds, split into the
/// form's fields. The string itself is never kept: the password goes to the credential
/// store like a typed one, and the rest to plain settings (S1).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MongoUri {
    pub srv: bool,
    /// The first host, as written (an IPv6 address keeps its brackets).
    pub host: String,
    /// `None` when not given, or for an SRV address, which has none.
    pub port: Option<u16>,
    /// How many more hosts the string listed. A connection keeps one; with `replicaSet`
    /// the driver finds the others from it.
    pub other_hosts: usize,
    pub user: String,
    pub password: Option<String>,
    pub database: String,
    /// The options, as written, without any that would put a secret in the settings.
    pub options: String,
    /// The names of the options left out for holding a secret.
    pub dropped: Vec<String>,
}

/// Splits a MongoDB connection string into the form's fields. Errors never quote the
/// string, which may hold a password.
pub fn parse_mongo_uri(text: &str) -> Result<MongoUri> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let (srv, rest) = if lower.starts_with("mongodb+srv://") {
        (true, &text["mongodb+srv://".len()..])
    } else if lower.starts_with("mongodb://") {
        (false, &text["mongodb://".len()..])
    } else {
        bail!("a connection string starts with mongodb:// or mongodb+srv://");
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let (userinfo, hosts) = match authority.rfind('@') {
        Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        None => (String::new(), None),
        Some(info) => {
            let (user, password) = match info.split_once(':') {
                Some((user, password)) => (user, Some(password)),
                None => (info, None),
            };
            let password = password.map(percent_decode).filter(|p| !p.is_empty());
            (percent_decode(user), password)
        }
    };
    let mut list = hosts.split(',').map(str::trim).filter(|h| !h.is_empty());
    let first = list
        .next()
        .ok_or_else(|| anyhow!("the connection string names no host"))?;
    let other_hosts = list.count();
    let (host, port) = split_host_port(first)?;
    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, query),
        None => (tail, ""),
    };
    let database = percent_decode(path.trim_start_matches('/'));
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for pair in query.split('&').filter(|p| !p.trim().is_empty()) {
        let settings = MongoSettings {
            srv: false,
            options: pair.to_owned(),
        };
        if settings.check().is_ok() {
            kept.push(pair);
        } else {
            let key = pair.split_once('=').map_or(pair, |(key, _)| key);
            dropped.push(percent_decode(key));
        }
    }
    Ok(MongoUri {
        srv,
        host,
        port: if srv { None } else { port },
        other_hosts,
        user,
        password,
        database,
        options: kept.join("&"),
        dropped,
    })
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`.
fn split_host_port(text: &str) -> Result<(String, Option<u16>)> {
    let (host, port) = if text.starts_with('[') {
        match text.find(']') {
            Some(end) => (&text[..=end], text[end + 1..].strip_prefix(':')),
            None => bail!("an IPv6 host in the connection string lacks its closing ]"),
        }
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        }
    };
    let port = match port {
        None | Some("") => None,
        Some(port) => Some(
            port.parse()
                .map_err(|_| anyhow!("the port in the connection string isn't a number"))?,
        ),
    };
    Ok((percent_decode(host), port))
}

/// `%XX` decoded, with `+` left as it is; bytes that aren't UTF-8 are replaced.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// Unique name; also the keychain account for this connection's password.
    pub name: String,
    /// Sidebar folder path, like `RGG/storefront/bear`. Empty means no folder.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub folder: String,
    pub adapter: Adapter,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    pub user: String,
    /// Blocks writes from the app and from agents.
    #[serde(default)]
    pub readonly: bool,
    /// Icon color as `#RRGGBB`. Empty means the database's own color.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    #[serde(default, skip_serializing_if = "TlsMode::is_default")]
    pub tls: TlsMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    /// Where the password comes from. Left out of the file for the OS credential store;
    /// `null`, which older versions could write, means that too.
    #[serde(
        default,
        deserialize_with = "null_is_password",
        skip_serializing_if = "Auth::is_password"
    )]
    pub auth: Auth,
    /// Where the connection was discovered, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// MongoDB and DocumentDB only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mongo: Option<MongoSettings>,
}

impl Connection {
    /// Whether a search for `query` finds it: each word of the query, ignoring case, is
    /// in its name, its folder path or where it connects to. An empty query finds all.
    pub fn matches(&self, query: &str) -> bool {
        let fields = [&self.name, &self.folder, &self.location()].map(|f| f.to_lowercase());
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| fields.iter().any(|field| field.contains(word)))
    }

    /// Where it connects to, for display: the host, a database file's name, or the
    /// secret that names the host.
    pub fn location(&self) -> String {
        if self.adapter.is_file() {
            let name = Path::new(&self.database).file_name();
            name.map_or_else(
                || self.database.clone(),
                |n| n.to_string_lossy().into_owned(),
            )
        } else if let Some((_, secret)) = self.auth.secret().filter(|(_, s)| s.use_secret_host) {
            secret.name.clone()
        } else {
            self.host.clone()
        }
    }

    /// Whether AWS, not the connection, names the database user: IAM sign-in to a
    /// Redshift Serverless workgroup, whose `GetCredentials` takes the user from the IAM
    /// identity (`IAM:<user>` or `IAMR:<role>`). The connection's user is then not used.
    pub fn iam_names_the_user(&self) -> bool {
        self.adapter == Adapter::Redshift
            && matches!(self.auth, Auth::AwsIam(_))
            && is_redshift_serverless_host(&self.host)
    }

    /// RDS refuses IAM tokens over a connection that isn't encrypted, and a token sent
    /// in the clear could be read on the way. Checked on save and again on connect, for
    /// files edited by hand.
    pub fn check_iam_encryption(&self) -> Result<()> {
        if matches!(self.auth, Auth::AwsIam(_)) && self.tls == TlsMode::Prefer {
            bail!(
                "an IAM token needs an encrypted connection: set Encryption to Required \
                 or Verified for \"{}\"",
                self.name
            );
        }
        Ok(())
    }

    /// The rules for MongoDB and DocumentDB, and that no other engine has their settings.
    fn check_mongo(&self) -> Result<()> {
        if self.adapter.family() != Family::Document {
            if self.mongo.is_some() {
                bail!("MongoDB settings apply only to MongoDB and DocumentDB");
            }
            return Ok(());
        }
        if matches!(self.auth, Auth::AwsIam(_)) {
            // MONGODB-AWS needs the driver's `aws-auth`, which pulls a C library (D2).
            bail!(
                "IAM sign-in to {} isn't supported yet: use a password or a secret",
                self.adapter.label()
            );
        }
        let settings = self.mongo.clone().unwrap_or_default();
        settings.check()?;
        if settings.srv && self.ssh.is_some() {
            bail!("an SRV address names several servers, which one SSH tunnel can't reach");
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("connection name is required");
        }
        if self.adapter.is_file() {
            if self.database.trim().is_empty() {
                bail!("a database file is required for \"{}\"", self.name);
            }
            if self.ssh.is_some() {
                bail!("SSH tunnels don't apply to database files");
            }
            if !self.auth.is_password() {
                bail!("AWS credentials don't apply to database files");
            }
        } else if let Some((_, secret)) = self.auth.secret() {
            // The user can come from the secret, and so can the host.
            if secret.name.trim().is_empty() {
                bail!(
                    "a secret or parameter name is required for \"{}\"",
                    self.name
                );
            }
            if !secret.use_secret_host && self.host.trim().is_empty() {
                bail!("host is required for \"{}\"", self.name);
            }
        } else {
            if self.host.trim().is_empty() {
                bail!("host is required for \"{}\"", self.name);
            }
            // A MongoDB server may run without users.
            let user_optional =
                self.iam_names_the_user() || self.adapter.family() == Family::Document;
            if self.user.trim().is_empty() && !user_optional {
                bail!("user is required for \"{}\"", self.name);
            }
            self.check_iam_encryption()?;
        }
        self.check_mongo()?;
        let color_is_valid = self.color.is_empty()
            || (self.color.len() == 7
                && self.color.starts_with('#')
                && self.color[1..].chars().all(|c| c.is_ascii_hexdigit()));
        if !color_is_valid {
            bail!("color must look like #RRGGBB");
        }
        if let Some(ssh) = &self.ssh {
            if ssh.host.trim().is_empty() || ssh.user.trim().is_empty() {
                bail!("SSH host and user are required for \"{}\"", self.name);
            }
            if !ssh.fingerprint.trim().starts_with("SHA256:") {
                bail!("the SSH server fingerprint must look like SHA256:…");
            }
            if self.tls == TlsMode::Verify {
                bail!("certificates can't be verified through an SSH tunnel; use Required");
            }
        }
        Ok(())
    }
}

/// A folder with what's directly inside it. The root has an empty path.
#[derive(Debug, PartialEq, Eq)]
pub struct FolderNode<'a> {
    pub path: String,
    pub folders: Vec<FolderNode<'a>>,
    pub connections: Vec<&'a Connection>,
}

impl<'a> FolderNode<'a> {
    /// Connections in this folder and every folder below it.
    pub fn connection_count(&self) -> usize {
        self.count(|_| true)
    }

    /// Connections in this folder and every folder below it for which `pred` holds.
    pub fn count(&self, pred: impl Fn(&Connection) -> bool + Copy) -> usize {
        self.connections.iter().filter(|c| pred(c)).count()
            + self.folders.iter().map(|f| f.count(pred)).sum::<usize>()
    }

    /// This folder with only the connections `keep` holds for, and only the folders below
    /// that still hold one of them.
    pub fn filtered(&self, keep: impl Fn(&Connection) -> bool + Copy) -> FolderNode<'a> {
        FolderNode {
            path: self.path.clone(),
            folders: self
                .folders
                .iter()
                .map(|f| f.filtered(keep))
                .filter(|f| f.connection_count() > 0)
                .collect(),
            connections: self
                .connections
                .iter()
                .copied()
                .filter(|c| keep(c))
                .collect(),
        }
    }
}

/// The settings in [`CONFIG_FILE`]. Read through [`ConfigFile`] and written by its own
/// [`Serialize`], so that what this version doesn't understand is kept (see [`Kept`]).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "ConfigFile")]
pub struct Config {
    /// The file format: [`VERSION_IAM`] once a connection uses an IAM token, which older
    /// versions of the app can't read, so that they refuse the file instead of failing to
    /// parse it; otherwise [`VERSION_BASE`]. Kept up to date by [`Config::upsert`] and
    /// [`Config::remove`].
    pub version: u32,
    /// Sidebar folder paths, so that empty folders are kept. Left out of the file when
    /// empty.
    pub folders: Vec<String>,
    /// The connections this version understands.
    pub connections: Vec<Connection>,
    /// Ask GitHub for a newer version at startup.
    pub check_for_updates: bool,
    /// Size of the app's text, in pixels.
    pub font_size: u8,
    /// Light or dark colors, or whichever the system uses. Left out of the file for
    /// [`Appearance::System`].
    pub appearance: Appearance,
    /// What the file holds that this version doesn't understand, written back unchanged.
    pub kept: Kept,
    /// Why this config must not be saved: the file it stands in for couldn't be read, and
    /// saving would delete what is in it. Never in the file.
    pub unsaved_because: Option<String>,
}

/// What a config file holds that this version doesn't understand, such as a connection
/// a newer version added with an engine or sign-in this one doesn't know. It is never
/// shown or connected to, and is written back unchanged, so that the newer version still
/// finds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    /// Each connection this version can't read, as the file held it, with its place
    /// among all the connections there.
    connections: Vec<(usize, Value)>,
    /// The file's version, when it had such connections: one of them may need it, so
    /// saving never writes a lower one. 0 otherwise.
    version: u32,
    /// Top-level settings this version doesn't know.
    fields: Map<String, Value>,
}

impl Kept {
    /// The names of the connections kept, where they have one.
    fn names(&self) -> impl Iterator<Item = &str> {
        self.connections
            .iter()
            .filter_map(|(_, raw)| raw.get("name")?.as_str())
    }
}

/// [`Config`] as the file holds it, with each connection still raw: one this version
/// can't read is kept instead of making the whole file unreadable.
#[derive(Deserialize)]
struct ConfigFile {
    version: u32,
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default)]
    connections: Vec<Value>,
    #[serde(default = "enabled")]
    check_for_updates: bool,
    #[serde(default = "default_font_size")]
    font_size: u8,
    #[serde(default)]
    appearance: Appearance,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl From<ConfigFile> for Config {
    fn from(file: ConfigFile) -> Self {
        let mut connections = Vec::new();
        let mut unsupported = Vec::new();
        for (place, raw) in file.connections.into_iter().enumerate() {
            match Connection::deserialize(&raw) {
                Ok(conn) => connections.push(conn),
                Err(_) => unsupported.push((place, raw)),
            }
        }
        let version = if unsupported.is_empty() {
            0
        } else {
            file.version
        };
        Self {
            version: file.version,
            folders: file.folders,
            connections,
            check_for_updates: file.check_for_updates,
            font_size: file.font_size,
            appearance: file.appearance,
            kept: Kept {
                connections: unsupported,
                version,
                fields: file.fields,
            },
            unsaved_because: None,
        }
    }
}

impl Serialize for Config {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct File<'a> {
            version: u32,
            #[serde(skip_serializing_if = "no_folders")]
            folders: &'a [String],
            connections: AllConnections<'a>,
            check_for_updates: bool,
            font_size: u8,
            #[serde(skip_serializing_if = "Appearance::is_system")]
            appearance: Appearance,
            #[serde(flatten)]
            fields: &'a Map<String, Value>,
        }
        fn no_folders(folders: &&[String]) -> bool {
            folders.is_empty()
        }
        File {
            version: self.version,
            folders: &self.folders,
            connections: AllConnections(self),
            check_for_updates: self.check_for_updates,
            font_size: self.font_size,
            appearance: self.appearance,
            fields: &self.kept.fields,
        }
        .serialize(serializer)
    }
}

/// A config's connections as the file lists them: those this version understands, with
/// each kept one back in its place. That is exactly where it was when nothing else
/// changed, and otherwise as near as there is room.
struct AllConnections<'a>(&'a Config);

impl Serialize for AllConnections<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Config {
            connections, kept, ..
        } = self.0;
        let total = connections.len() + kept.connections.len();
        let mut seq = serializer.serialize_seq(Some(total))?;
        let mut known = connections.iter();
        let mut kept = kept.connections.iter().peekable();
        for place in 0..total {
            if let Some((_, raw)) = kept.next_if(|(at, _)| *at <= place) {
                seq.serialize_element(raw)?;
            } else if let Some(conn) = known.next() {
                seq.serialize_element(conn)?;
            } else if let Some((_, raw)) = kept.next() {
                seq.serialize_element(raw)?;
            }
        }
        seq.end()
    }
}

fn null_is_password<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Auth, D::Error> {
    Ok(Option::<Auth>::deserialize(d)?.unwrap_or_default())
}

/// The app's colors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    /// Follows the system's light or dark setting, and changes when it does.
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    pub fn is_system(&self) -> bool {
        *self == Appearance::System
    }

    pub fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }
}

/// The file format every version of the app reads.
pub const VERSION_BASE: u32 = 1;
/// The file format of a config with an IAM token connection.
pub const VERSION_IAM: u32 = 2;

pub const DEFAULT_FONT_SIZE: u8 = 15;
/// The smallest and largest text the app offers.
pub const FONT_SIZES: RangeInclusive<u8> = 10..=28;

fn enabled() -> bool {
    true
}

fn default_font_size() -> u8 {
    DEFAULT_FONT_SIZE
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION_BASE,
            folders: Vec::new(),
            connections: Vec::new(),
            check_for_updates: true,
            font_size: DEFAULT_FONT_SIZE,
            appearance: Appearance::System,
            kept: Kept::default(),
            unsaved_because: None,
        }
    }
}

/// Directory holding [`CONFIG_FILE`]: `$TUPLEBASE_CONFIG_DIR` if set, otherwise the
/// platform config dir (e.g. `~/Library/Application Support/app.tuplebase.TupleBase`).
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        return Ok(dir.into());
    }
    directories::ProjectDirs::from("app", "tuplebase", "TupleBase")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .context("cannot locate the user config directory")
}

/// Writes a file via a temp file and rename, so a crash never leaves half a file.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    write_bytes_atomic(path, format!("{text}\n").as_bytes())
}

/// Each write has its own temp file, so two at once never write into or rename away
/// each other's. It is flushed to disk before it replaces the file.
fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}-{n}.tmp", std::process::id()));
    let written = fs::File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| {
            fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
        });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// The file's bytes, or `None` when there is no file.
fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Where the last file older versions can read is kept, once it is upgraded.
pub const BASE_VERSION_BACKUP: &str = "connections.v1-backup.json";

/// Keeps the [`VERSION_BASE`] file at `path` as [`BASE_VERSION_BACKUP`] beside it, before
/// it is raised to [`VERSION_IAM`]. A backup that isn't a whole version 1 file, such as one
/// cut short, is written again. A good one that differs, from an earlier upgrade, is kept
/// as `connections.v1-backup-<time>.json` first, so no copy older versions can read is lost.
fn keep_base_version(path: &Path) -> Result<()> {
    let Some(current) = read_file(path)? else {
        return Ok(());
    };
    if !Config::parse(&current).is_ok_and(|c| c.version == VERSION_BASE) {
        return Ok(());
    }
    let backup = path.with_file_name(BASE_VERSION_BACKUP);
    match fs::read(&backup) {
        Ok(old) if old == current => return Ok(()),
        Ok(old) if Config::parse(&old).is_ok_and(|c| c.version == VERSION_BASE) => {
            let older = unused_name(path, BASE_VERSION_BACKUP.trim_end_matches(".json"));
            fs::rename(&backup, &older)
                .with_context(|| format!("keeping {} as {}", backup.display(), older.display()))?;
        }
        // Missing, or not a version 1 file this version reads: write it (again).
        _ => {}
    }
    write_bytes_atomic(&backup, &current)
}

/// `<prefix>-<time>.json` beside `path`, with `-N` added so it never names a file that
/// exists.
fn unused_name(path: &Path, prefix: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (0..)
        .map(|n| {
            let suffix = if n == 0 {
                String::new()
            } else {
                format!("-{n}")
            };
            path.with_file_name(format!("{prefix}-{stamp}{suffix}.json"))
        })
        .find(|candidate| !candidate.exists())
        .expect("an unused name exists")
}

/// Where a file this version couldn't read is kept: next to it, named with the
/// time, and never overwriting an earlier copy.
fn unreadable_copy(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("connections");
    unused_name(path, &format!("{stem}.unreadable"))
}

/// What to tell the user once saving moved a file this version can't read to `kept`.
pub fn kept_unreadable_message(kept: &Path) -> String {
    format!(
        "Your earlier connections file, which this version can't read, was kept as {}.",
        kept.display()
    )
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

impl Config {
    /// Loads the user's config; a missing file is an empty config.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    /// Saves the user's config; see [`Config::save_to`].
    pub fn save(&self) -> Result<Option<PathBuf>> {
        self.save_to(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match read_file(path)? {
            None => Ok(Self::default()),
            Some(bytes) => {
                Self::parse(&bytes).with_context(|| format!("parsing {}", path.display()))
            }
        }
    }

    /// A config file's contents, if this version reads them.
    fn parse(bytes: &[u8]) -> Result<Self> {
        let mut config: Config = serde_json::from_slice(bytes)?;
        if !(VERSION_BASE..=VERSION_IAM).contains(&config.version) {
            bail!("unsupported config version {}", config.version);
        }
        config.resize_font(0);
        config.sync_version();
        Ok(config)
    }

    /// Loads the user's config like [`Config::load`], but a file that can't be used
    /// doesn't stop the app and is left exactly as it is. Also returns what to tell the
    /// user, then.
    pub fn load_or_keep() -> Result<(Self, Option<String>)> {
        Ok(Self::load_or_keep_from(&config_path()?))
    }

    /// The config starts empty when the file can't be used:
    /// - one this version can't parse, such as a newer version's, is moved aside by
    ///   [`Config::save_to`], and only when something is saved;
    /// - one that can't be read at all, perhaps only for now, is never saved over.
    pub fn load_or_keep_from(path: &Path) -> (Self, Option<String>) {
        let bytes = match read_file(path) {
            Ok(None) => return (Self::default(), None),
            Ok(Some(bytes)) => bytes,
            Err(e) => {
                let config = Self {
                    unsaved_because: Some(format!(
                        "{} couldn't be read, so it isn't written over",
                        path.display()
                    )),
                    ..Self::default()
                };
                let message = format!(
                    "Couldn't read your connections ({e:#}). The file is left untouched and \
                     changes won't be saved; restart TupleBase once it can be read."
                );
                return (config, Some(message));
            }
        };
        match Self::parse(&bytes) {
            Ok(config) => (config, None),
            Err(e) => (
                Self::default(),
                Some(format!(
                    "This version can't read your connections file {} ({e:#}). It is left \
                     untouched; if you change anything, it is first kept beside it as \
                     connections.unreadable-<time>.json, never written over.",
                    path.display()
                )),
            ),
        }
    }

    /// Writes the config. A file already there that this version can't parse is moved
    /// aside first rather than overwritten, so its connections are never lost; returns
    /// where it went. A file that can't be read at all is left alone and nothing is
    /// saved. The first time it raises a file to [`VERSION_IAM`], which older versions
    /// can't read, it keeps a copy of the old file as [`BASE_VERSION_BACKUP`].
    pub fn save_to(&self, path: &Path) -> Result<Option<PathBuf>> {
        if let Some(reason) = &self.unsaved_because {
            bail!("not saved: {reason}");
        }
        // One save at a time: a file another save has only half written would look
        // unreadable here and be moved aside.
        static SAVING: Mutex<()> = Mutex::new(());
        let _saving = SAVING.lock().unwrap_or_else(PoisonError::into_inner);
        let existing = read_file(path).with_context(|| {
            format!(
                "not saved: {} couldn't be checked, so it isn't written over",
                path.display()
            )
        })?;
        let mut moved = None;
        let mut config = self.clone();
        match existing.map(|bytes| Self::parse(&bytes)) {
            Some(Err(_)) => {
                // E.g. a newer TupleBase wrote it.
                let to = unreadable_copy(path);
                fs::rename(path, &to).with_context(|| {
                    format!(
                        "keeping the unreadable {} as {}",
                        path.display(),
                        to.display()
                    )
                })?;
                moved = Some(to);
            }
            // What the file holds now that this version doesn't understand, perhaps
            // written by a newer version since this config was loaded: this version can
            // never change it, so the file's own is the one to keep.
            Some(Ok(on_disk)) => config.kept = on_disk.kept,
            None => {}
        }
        config.sync_version();
        if config.version > VERSION_BASE {
            keep_base_version(path)?;
        }
        write_atomic(path, &serde_json::to_string_pretty(&config)?)?;
        Ok(moved)
    }

    /// Sets the oldest file format that holds every connection. It is never below the
    /// version of a file holding connections this version can't read, which may need it.
    fn sync_version(&mut self) {
        let iam = self
            .connections
            .iter()
            .any(|c| matches!(c.auth, Auth::AwsIam(_)));
        let needed = if iam { VERSION_IAM } else { VERSION_BASE };
        self.version = needed.max(self.kept.version);
    }

    /// How many connections the file holds that this version can't read, such as ones
    /// a newer version added. They are kept but not shown.
    pub fn unsupported_count(&self) -> usize {
        self.kept.connections.len()
    }

    /// Makes the app's text `steps` pixels larger, or smaller for a negative number,
    /// staying within [`FONT_SIZES`].
    pub fn resize_font(&mut self, steps: i8) {
        self.font_size = self
            .font_size
            .saturating_add_signed(steps)
            .clamp(*FONT_SIZES.start(), *FONT_SIZES.end());
    }

    /// Every folder path, parents before their children.
    pub fn folders(&self) -> Vec<String> {
        let mut paths = BTreeSet::new();
        let saved = self.folders.iter().map(String::as_str);
        let used = self.connections.iter().map(|c| c.folder.as_str());
        for path in saved.chain(used).filter(|p| !p.is_empty()) {
            let mut current = path;
            while !current.is_empty() {
                paths.insert(current.to_owned());
                current = parent_of(current);
            }
        }
        let mut paths: Vec<String> = paths.into_iter().collect();
        paths.sort_by_key(|p| {
            p.split(FOLDER_SEPARATOR)
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        });
        paths
    }

    /// The whole folder structure, from the root.
    pub fn tree(&self) -> FolderNode<'_> {
        self.node(String::new(), &self.folders())
    }

    fn node(&self, path: String, all: &[String]) -> FolderNode<'_> {
        let folders = all
            .iter()
            .filter(|f| parent_of(f) == path)
            .map(|f| self.node(f.clone(), all))
            .collect();
        let mut connections: Vec<&Connection> = self
            .connections
            .iter()
            .filter(|c| c.folder == path)
            .collect();
        connections.sort_by_key(|c| c.name.to_lowercase());
        FolderNode {
            path,
            folders,
            connections,
        }
    }

    fn check_folder_name(&self, parent: &str, name: &str, current: Option<&str>) -> Result<()> {
        if name.is_empty() {
            bail!("folder name is required");
        }
        if name.contains(FOLDER_SEPARATOR) {
            bail!("folder names can't contain \"{FOLDER_SEPARATOR}\"");
        }
        let taken = self.folders().iter().any(|f| {
            parent_of(f) == parent
                && folder_name(f).to_lowercase() == name.to_lowercase()
                && Some(f.as_str()) != current
        });
        if taken {
            bail!("a folder named \"{name}\" already exists here");
        }
        Ok(())
    }

    fn rewrite_folders(&mut self, old: &str, new: &str) {
        let paths = self
            .folders
            .iter_mut()
            .chain(self.connections.iter_mut().map(|c| &mut c.folder));
        for path in paths {
            if let Some(moved) = rebase_folder(path, old, new) {
                *path = moved;
            }
        }
        self.folders.retain(|f| !f.is_empty());
    }

    /// Creates a folder inside `parent` (`""` for the top level) and returns its path.
    pub fn add_folder(&mut self, parent: &str, name: &str) -> Result<String> {
        let name = name.trim();
        self.check_folder_name(parent, name, None)?;
        let path = join_folder(parent, name);
        self.folders.push(path.clone());
        Ok(path)
    }

    /// Renames a folder, keeping what's inside it, and returns its new path.
    pub fn rename_folder(&mut self, path: &str, new_name: &str) -> Result<String> {
        let new_name = new_name.trim();
        let parent = parent_of(path).to_owned();
        self.check_folder_name(&parent, new_name, Some(path))?;
        let new_path = join_folder(&parent, new_name);
        self.rewrite_folders(path, &new_path);
        Ok(new_path)
    }

    /// Removes a folder. What's inside it moves up one level; nothing is deleted.
    pub fn remove_folder(&mut self, path: &str) {
        let parent = parent_of(path).to_owned();
        self.folders.retain(|f| f != path);
        self.rewrite_folders(path, &parent);
    }

    /// Moves a folder into `new_parent` (`""` for the top level). Returns the new path,
    /// or `None` if it was already there.
    pub fn move_folder(&mut self, path: &str, new_parent: &str) -> Result<Option<String>> {
        if parent_of(path) == new_parent {
            return Ok(None);
        }
        if rebase_folder(new_parent, path, "").is_some() {
            bail!("a folder can't be moved inside itself");
        }
        let name = folder_name(path).to_owned();
        self.check_folder_name(new_parent, &name, None)?;
        let new_path = join_folder(new_parent, &name);
        self.rewrite_folders(path, &new_path);
        Ok(Some(new_path))
    }

    /// Moves a connection into `folder` (`""` for none). Returns whether anything changed.
    pub fn move_connection(&mut self, name: &str, folder: &str) -> bool {
        match self.connections.iter_mut().find(|c| c.name == name) {
            Some(conn) if conn.folder != folder => {
                conn.folder = folder.to_owned();
                true
            }
            _ => false,
        }
    }

    pub fn find(&self, name: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.name == name)
    }

    /// Adds `conn`, or replaces the connection currently named `previous` (a rename
    /// when the names differ).
    ///
    /// Names are unique ignoring case, since the OS credential store keys passwords by
    /// name and Windows' ignores case. A rename may change only the case. A connection
    /// whose name is unchanged still saves when a file from before this rule holds
    /// another that differs only in case.
    pub fn upsert(&mut self, conn: Connection, previous: Option<&str>) -> Result<()> {
        conn.validate()?;
        let unchanged = previous == Some(conn.name.as_str());
        let lower = conn.name.to_lowercase();
        let clash = self.connections.iter().find(|c| {
            Some(c.name.as_str()) != previous
                && (c.name == conn.name || (!unchanged && c.name.to_lowercase() == lower))
        });
        if let Some(existing) = clash {
            bail!("a connection named \"{}\" already exists", existing.name);
        }
        // One this version can't read still owns its name, in the credential store too.
        if !unchanged && let Some(kept) = self.kept.names().find(|n| n.to_lowercase() == lower) {
            bail!("a connection named \"{kept}\" already exists, saved by a newer TupleBase");
        }
        match previous.and_then(|p| self.connections.iter().position(|c| c.name == p)) {
            Some(i) => self.connections[i] = conn,
            None => self.connections.push(conn),
        }
        self.sync_version();
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.connections.len();
        self.connections.retain(|c| c.name != name);
        self.sync_version();
        self.connections.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(name: &str) -> Connection {
        Connection {
            name: name.into(),
            folder: String::new(),
            adapter: Adapter::Postgres,
            host: "localhost".into(),
            port: 5432,
            database: "app".into(),
            user: "dev".into(),
            readonly: false,
            color: String::new(),
            tls: TlsMode::default(),
            ssh: None,
            auth: Auth::Password,
            source: None,
            mongo: None,
        }
    }

    #[test]
    fn every_adapter_is_listed_once() {
        // Fails to compile when a variant is added, as a reminder to add it to ALL too.
        let position = |adapter: Adapter| match adapter {
            Adapter::Postgres => 0,
            Adapter::Mariadb => 1,
            Adapter::Mysql => 2,
            Adapter::Sqlite => 3,
            Adapter::Redshift => 4,
            Adapter::Mongodb => 5,
            Adapter::Documentdb => 6,
        };
        for (i, adapter) in Adapter::ALL.into_iter().enumerate() {
            assert_eq!(position(adapter), i);
        }
    }

    #[test]
    fn each_engine_has_its_own_facts() {
        let labels = Adapter::ALL.map(Adapter::label);
        assert_eq!(
            labels,
            [
                "PostgreSQL",
                "MariaDB",
                "MySQL",
                "SQLite",
                "Redshift",
                "MongoDB",
                "Amazon DocumentDB"
            ]
        );
        let ports = Adapter::ALL.map(Adapter::default_port);
        assert_eq!(ports, [5432, 3306, 3306, 0, 5439, 27017, 27017]);
        let users = Adapter::ALL.map(Adapter::user_hint);
        assert_eq!(
            users,
            [
                "postgres",
                "root",
                "root",
                "",
                "awsuser",
                "admin",
                "docdbadmin"
            ]
        );
        for adapter in Adapter::ALL {
            // Only a server has a port, and a user.
            let info = adapter.info();
            let is_file = info.place == Place::File;
            assert_eq!(is_file, info.default_port == 0, "{}", info.label);
            assert_eq!(is_file, info.user_hint.is_empty(), "{}", info.label);
        }
        assert!(Adapter::Sqlite.is_file());
        assert_eq!(Adapter::Sqlite.quote_ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(Adapter::Mysql.quote_ident("a`b"), "`a``b`");
        assert_eq!(
            Adapter::Postgres.preview_query("public", "users"),
            r#"SELECT * FROM "public"."users" LIMIT 100"#
        );
        assert_eq!(Adapter::Postgres.info().script, ScriptFlavor::Postgres);
        assert_eq!(Adapter::Mariadb.info().script, ScriptFlavor::Mysql);
        assert_eq!(Adapter::Mysql.info().script, ScriptFlavor::Mysql);
        assert_eq!(Adapter::Sqlite.info().script, ScriptFlavor::Sqlite);
        assert_eq!(Adapter::Redshift.info().script, ScriptFlavor::Redshift);
        assert_eq!(Adapter::Mongodb.info().script, ScriptFlavor::MongoShell);
        assert_eq!(Adapter::Documentdb.info().script, ScriptFlavor::MongoShell);
        assert!(!Adapter::Redshift.is_file());
        assert_eq!(Adapter::Redshift.quote_ident(r#"a"b"#), r#""a""b""#);
        // Saved in every connections.json: it can never change.
        assert_eq!(
            serde_json::to_string(&Adapter::Redshift).unwrap(),
            r#""redshift""#
        );
        assert_eq!(
            serde_json::to_string(&Adapter::Mongodb).unwrap(),
            r#""mongodb""#
        );
        assert_eq!(
            serde_json::to_string(&Adapter::Documentdb).unwrap(),
            r#""documentdb""#
        );
    }

    #[test]
    fn each_engine_has_its_family_and_place() {
        let families = Adapter::ALL.map(Adapter::family);
        assert_eq!(
            families,
            [
                Family::Sql,
                Family::Sql,
                Family::Sql,
                Family::Sql,
                Family::Sql,
                Family::Document,
                Family::Document
            ]
        );
        let places = Adapter::ALL.map(|a| a.info().place);
        assert_eq!(
            places,
            [
                Place::Server,
                Place::Server,
                Place::Server,
                Place::File,
                Place::Server,
                Place::Server,
                Place::Server
            ]
        );
        for adapter in Adapter::ALL {
            assert_eq!(
                adapter.is_file(),
                adapter.info().place == Place::File,
                "{adapter:?}"
            );
        }
    }

    #[test]
    fn preview_query_is_the_sql_it_always_was() {
        let previews = Adapter::ALL.map(|a| a.preview_query(r#"my"s`c"#, "users"));
        assert_eq!(
            previews,
            [
                r#"SELECT * FROM "my""s`c"."users" LIMIT 100"#,
                r#"SELECT * FROM `my"s``c`.`users` LIMIT 100"#,
                r#"SELECT * FROM `my"s``c`.`users` LIMIT 100"#,
                r#"SELECT * FROM "my""s`c"."users" LIMIT 100"#,
                r#"SELECT * FROM "my""s`c"."users" LIMIT 100"#,
                r#"db.getSiblingDB("my\"s`c").getCollection("users").find({}).limit(100)"#,
                r#"db.getSiblingDB("my\"s`c").getCollection("users").find({}).limit(100)"#,
            ]
        );
    }

    #[test]
    fn preview_query_follows_the_family() {
        let quote = |ident: &str| Adapter::Postgres.quote_ident(ident);
        assert_eq!(
            preview_query(Family::Document, quote, "app", r#"us"ers"#),
            r#"db.getSiblingDB("app").getCollection("us\"ers").find({}).limit(100)"#
        );
        assert_eq!(
            preview_query(Family::KeyValue, quote, "eu-west-1", r#"us"ers"#),
            r#"SELECT * FROM "us""ers""#
        );
    }

    #[test]
    fn identifiers_are_quoted_for_each_database() {
        assert_eq!(Adapter::Postgres.quote_ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(Adapter::Mariadb.quote_ident("a`b"), "`a``b`");
        assert_eq!(
            Adapter::Mysql.preview_query("app", "users"),
            "SELECT * FROM `app`.`users` LIMIT 100"
        );
    }

    fn ssh() -> SshConfig {
        SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "me".into(),
            key_path: String::new(),
            fingerprint: "SHA256:abc".into(),
        }
    }

    #[test]
    fn colors_must_be_hex_or_empty() {
        let mut config = Config::default();
        let mut conn = pg("a");
        conn.color = "#3E8BD6".into();
        assert!(config.upsert(conn.clone(), None).is_ok());
        conn.name = "b".into();
        conn.color = "blue".into();
        assert!(config.upsert(conn, None).is_err());
    }

    /// The OS credential store keys passwords by name, and Windows' ignores case.
    #[test]
    fn names_are_unique_ignoring_case_and_can_change_only_case() {
        let mut config = Config::default();
        config.upsert(pg("prod"), None).unwrap();
        let error = config.upsert(pg("PROD"), None).unwrap_err().to_string();
        assert!(error.contains("already exists"), "{error}");
        config.upsert(pg("other"), None).unwrap();
        assert!(config.upsert(pg("Prod"), Some("other")).is_err());
        // A rename only by case is allowed.
        config.upsert(pg("Prod"), Some("prod")).unwrap();
        let names: Vec<_> = config.connections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Prod", "other"]);
        // A file from before this rule may hold two such names: each still saves.
        config.connections.push(pg("PROD"));
        config.upsert(pg("PROD"), Some("PROD")).unwrap();
        config.upsert(pg("Prod"), Some("Prod")).unwrap();
        assert!(config.upsert(pg("prod"), Some("PROD")).is_err());
    }

    fn sqlite(file: &str) -> Connection {
        Connection {
            adapter: Adapter::Sqlite,
            host: String::new(),
            port: 0,
            user: String::new(),
            database: file.into(),
            ..pg("file")
        }
    }

    #[test]
    fn sqlite_needs_a_file_but_no_host_or_user() {
        let mut config = Config::default();
        assert!(config.upsert(sqlite(""), None).is_err());
        assert!(config.upsert(sqlite("/data/app.sqlite"), None).is_ok());
        assert_eq!(config.connections[0].location(), "app.sqlite");
    }

    #[test]
    fn sqlite_cannot_use_a_tunnel() {
        let mut conn = sqlite("/data/app.sqlite");
        conn.ssh = Some(ssh());
        assert!(Config::default().upsert(conn, None).is_err());
    }

    #[test]
    fn ssh_needs_a_fingerprint() {
        let mut config = Config::default();
        let mut conn = pg("a");
        conn.ssh = Some(SshConfig {
            fingerprint: String::new(),
            ..ssh()
        });
        assert!(config.upsert(conn, None).is_err());
    }

    #[test]
    fn certificate_verification_is_refused_through_a_tunnel() {
        let mut config = Config::default();
        let mut conn = pg("a");
        conn.ssh = Some(ssh());
        conn.tls = TlsMode::Verify;
        assert!(config.upsert(conn.clone(), None).is_err());
        conn.tls = TlsMode::Require;
        assert!(config.upsert(conn, None).is_ok());
    }

    #[test]
    fn default_tls_and_no_ssh_are_not_written() {
        let json = serde_json::to_string(&pg("a")).unwrap();
        assert!(!json.contains("tls") && !json.contains("ssh") && !json.contains("auth"));
    }

    fn aws(name: &str) -> Auth {
        Auth::AwsSecret(AwsSecretRef {
            profile: "prod-sso".into(),
            region: String::new(),
            name: name.into(),
            use_secret_host: false,
        })
    }

    fn iam(name: &str) -> Connection {
        Connection {
            tls: TlsMode::Require,
            auth: Auth::AwsIam(AwsIamAuth {
                profile: "prod-sso".into(),
                region: "eu-west-1".into(),
            }),
            ..pg(name)
        }
    }

    #[test]
    fn aws_auth_is_saved_as_a_pointer() {
        let mut conn = pg("a");
        conn.auth = aws("prod/orders");
        let json = serde_json::to_value(&conn).unwrap();
        assert_eq!(
            json["auth"],
            serde_json::json!({
                "type": "aws_secret",
                "profile": "prod-sso",
                "name": "prod/orders",
                "use_secret_host": false,
            })
        );
        assert_eq!(serde_json::from_value::<Connection>(json).unwrap(), conn);
    }

    #[test]
    fn aws_auth_needs_a_name_and_can_supply_the_user_and_host() {
        let mut config = Config::default();
        let mut conn = pg("a");
        conn.auth = aws("");
        assert!(config.upsert(conn.clone(), None).is_err());

        conn.auth = aws("prod/orders");
        conn.user.clear();
        assert!(config.upsert(conn.clone(), None).is_ok());

        conn.host.clear();
        assert!(config.upsert(conn.clone(), Some("a")).is_err());
        let Auth::AwsSecret(secret) = &mut conn.auth else {
            unreachable!()
        };
        secret.use_secret_host = true;
        assert!(config.upsert(conn.clone(), Some("a")).is_ok());
        assert_eq!(conn.location(), "prod/orders");

        let mut file = sqlite("/data/app.sqlite");
        file.auth = aws("prod/orders");
        assert!(config.upsert(file, None).is_err());
    }

    /// A config written before IAM tokens existed, as the app wrote it.
    const V1_FILE: &str = r#"{
  "version": 1,
  "folders": [
    "Prod"
  ],
  "connections": [
    {
      "name": "plain",
      "folder": "Prod",
      "adapter": "postgres",
      "host": "db.example.com",
      "port": 5432,
      "database": "app",
      "user": "dev",
      "readonly": false
    },
    {
      "name": "secret",
      "adapter": "mariadb",
      "host": "",
      "port": 3306,
      "user": "",
      "readonly": true,
      "tls": "require",
      "auth": {
        "type": "aws_secret",
        "profile": "prod-sso",
        "region": "eu-west-1",
        "name": "prod/orders-db",
        "use_secret_host": true
      }
    },
    {
      "name": "parameter",
      "adapter": "mysql",
      "host": "10.0.0.5",
      "port": 3306,
      "user": "app",
      "readonly": false,
      "auth": {
        "type": "aws_parameter",
        "name": "/prod/password",
        "use_secret_host": false
      }
    }
  ],
  "check_for_updates": true,
  "font_size": 15
}"#;

    #[test]
    fn a_version_1_file_loads_and_saves_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, format!("{V1_FILE}\n")).unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.version, VERSION_BASE);
        assert!(config.connections[0].auth.is_password());
        assert_eq!(
            config.connections[1].auth,
            Auth::AwsSecret(AwsSecretRef {
                profile: "prod-sso".into(),
                region: "eu-west-1".into(),
                name: "prod/orders-db".into(),
                use_secret_host: true,
            })
        );
        assert_eq!(
            config.connections[2]
                .auth
                .secret()
                .map(|(store, s)| (store, &*s.name)),
            Some((AwsStore::ParameterStore, "/prod/password"))
        );
        config.save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{V1_FILE}\n"));
    }

    #[test]
    fn an_iam_connection_raises_the_version_and_removing_it_lowers_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let mut config = Config::default();
        config.upsert(pg("plain"), None).unwrap();
        config.save_to(&path).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""version": 1"#)
        );

        config.upsert(iam("iam"), None).unwrap();
        assert_eq!(config.version, VERSION_IAM);
        config.save_to(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""version": 2"#), "{text}");
        assert_eq!(Config::load_from(&path).unwrap(), config);

        config.remove("iam");
        assert_eq!(config.version, VERSION_BASE);
        config.save_to(&path).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""version": 1"#)
        );
    }

    #[test]
    fn unknown_versions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, r#"{ "version": 3 }"#).unwrap();
        let error = format!("{:#}", Config::load_from(&path).unwrap_err());
        assert!(error.contains("unsupported config version 3"), "{error}");
        fs::write(&path, r#"{ "version": 2 }"#).unwrap();
        assert!(Config::load_from(&path).is_ok());
    }

    /// The files in `dir` whose names contain `part`.
    fn files_named(dir: &Path, part: &str) -> Vec<PathBuf> {
        let mut found: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().contains(part))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn a_file_this_version_cannot_parse_is_left_alone_until_something_is_saved() {
        let texts: [&[u8]; 3] = [
            br#"{ "version": 3, "connections": [] }"#,
            b"not json",
            b"\xff\xfe not utf-8",
        ];
        for text in texts {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(CONFIG_FILE);
            fs::write(&path, text).unwrap();
            let (mut config, message) = Config::load_or_keep_from(&path);
            let message = message.expect("the user is told");
            assert!(message.contains("left untouched"), "{message}");
            assert_eq!(config.connections, []);
            // Loading changes nothing on disk.
            assert_eq!(fs::read(&path).unwrap(), text);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

            // Saving moves it aside, byte for byte, and says where.
            config.resize_font(1);
            let kept = config.save_to(&path).unwrap().expect("moved aside");
            assert_eq!(
                files_named(dir.path(), "connections.unreadable-"),
                std::slice::from_ref(&kept)
            );
            assert_eq!(fs::read(&kept).unwrap(), text);
            assert!(Config::load_from(&path).is_ok());
            let told = kept_unreadable_message(&kept);
            assert!(told.contains(&*kept.to_string_lossy()), "{told}");
            // Later saves write over their own file and leave the kept one alone.
            config.resize_font(1);
            assert_eq!(config.save_to(&path).unwrap(), None);
            assert_eq!(fs::read(&kept).unwrap(), text);
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_saved_over_or_moved() {
        // A directory can't be read as a file on any system, even by root.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), "kept").unwrap();
        let (config, message) = Config::load_or_keep_from(&path);
        let message = message.expect("the user is told");
        assert!(message.contains("won't be saved"), "{message}");
        let error = config.save_to(&path).unwrap_err().to_string();
        assert!(error.contains("not saved"), "{error}");
        // Even a config from a successful load doesn't write over what it can't check.
        let error = Config::default().save_to(&path).unwrap_err().to_string();
        assert!(error.contains("not saved"), "{error}");
        assert!(path.is_dir());
        assert_eq!(fs::read_to_string(path.join("inside")).unwrap(), "kept");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_config_standing_in_for_a_file_kept_in_place_is_never_saved() {
        // What a file that couldn't be read gives: saving would delete what is in it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, "not json").unwrap();
        let config = Config {
            unsaved_because: Some("it couldn't be read".into()),
            ..Config::default()
        };
        let error = config.save_to(&path).unwrap_err().to_string();
        assert!(error.contains("not saved"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn a_readable_file_loads_as_usual() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, V1_FILE).unwrap();
        let (config, message) = Config::load_or_keep_from(&path);
        assert_eq!(message, None);
        assert_eq!(config, Config::load_from(&path).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// `config` with a connection that uses an IAM token, which raises it to version 2.
    fn with_iam(config: &Config) -> Config {
        let mut config = config.clone();
        let mut iam = config.connections[0].clone();
        iam.name = "iam".into();
        iam.tls = TlsMode::Require;
        iam.auth = Auth::AwsIam(AwsIamAuth {
            profile: String::new(),
            region: "eu-west-1".into(),
        });
        config.upsert(iam, None).unwrap();
        config
    }

    #[test]
    fn a_backup_cut_short_is_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let backup = dir.path().join(BASE_VERSION_BACKUP);
        fs::write(&path, V1_FILE).unwrap();
        // As a copy interrupted midway would leave it.
        fs::write(&backup, &V1_FILE[..V1_FILE.len() / 2]).unwrap();
        let config = Config::load_from(&path).unwrap();
        with_iam(&config).save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), V1_FILE);
        assert_eq!(files_named(dir.path(), ".tmp"), Vec::<PathBuf>::new());
    }

    #[test]
    fn saves_at_the_same_time_all_succeed_and_leave_a_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        std::thread::scope(|scope| {
            for thread in 0..8u8 {
                let path = &path;
                scope.spawn(move || {
                    for round in 0..40u8 {
                        let mut config = Config {
                            font_size: 10 + (thread + round) % 10,
                            ..Config::default()
                        };
                        config.upsert(pg(&format!("db{thread}")), None).unwrap();
                        assert_eq!(config.save_to(path).unwrap(), None);
                    }
                });
            }
        });
        Config::load_from(&path).unwrap();
        assert_eq!(
            files_named(dir.path(), ".unreadable-"),
            Vec::<PathBuf>::new()
        );
        assert_eq!(files_named(dir.path(), ".tmp"), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_later_upgrade_keeps_the_earlier_backup_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let backup = dir.path().join(BASE_VERSION_BACKUP);
        fs::write(&path, V1_FILE).unwrap();
        let config = Config::load_from(&path).unwrap();
        with_iam(&config).save_to(&path).unwrap();
        // The IAM connection is removed, the file goes back to version 1 and changes...
        let mut later = config.clone();
        later.resize_font(2);
        later.save_to(&path).unwrap();
        let later_v1 = fs::read_to_string(&path).unwrap();
        assert_ne!(later_v1, V1_FILE);
        // ...and is raised again: the backup is the newer file, the older one kept.
        with_iam(&later).save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), later_v1);
        let older = files_named(dir.path(), "connections.v1-backup-");
        assert_eq!(older.len(), 1, "{older:?}");
        assert_eq!(fs::read_to_string(&older[0]).unwrap(), V1_FILE);
        // Raising the same version 1 file again changes nothing.
        later.save_to(&path).unwrap();
        with_iam(&later).save_to(&path).unwrap();
        assert_eq!(files_named(dir.path(), "connections.v1-backup-").len(), 1);
    }

    #[test]
    fn the_first_save_as_version_2_keeps_the_version_1_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, V1_FILE).unwrap();
        let mut config = Config::load_from(&path).unwrap();
        config.save_to(&path).unwrap();
        let v1 = fs::read_to_string(&path).unwrap();
        let backup = dir.path().join(BASE_VERSION_BACKUP);
        assert!(!backup.exists(), "no backup while the file stays version 1");
        let mut iam = config.connections[0].clone();
        iam.name = "iam".into();
        iam.tls = TlsMode::Require;
        iam.auth = Auth::AwsIam(AwsIamAuth {
            profile: String::new(),
            region: "eu-west-1".into(),
        });
        config.upsert(iam, None).unwrap();
        config.save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), v1);
        // Later saves leave the backup as it was.
        config.resize_font(2);
        config.save_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&backup).unwrap(), v1);
    }

    #[test]
    fn a_null_auth_is_the_credential_store() {
        let conn: Connection = serde_json::from_str(
            r#"{ "name": "a", "adapter": "postgres", "host": "h", "port": 5432,
                 "database": "", "user": "u", "auth": null }"#,
        )
        .unwrap();
        assert_eq!(conn.auth, Auth::Password);
    }

    #[test]
    fn each_auth_is_saved_with_its_type() {
        let cases = [
            (Auth::Password, None),
            (
                aws("s"),
                Some(
                    r#"{"type":"aws_secret","profile":"prod-sso","name":"s","use_secret_host":false}"#,
                ),
            ),
            (
                Auth::AwsParameter(AwsSecretRef {
                    name: "/p".into(),
                    ..Default::default()
                }),
                Some(r#"{"type":"aws_parameter","name":"/p","use_secret_host":false}"#),
            ),
            (
                Auth::AwsIam(AwsIamAuth::default()),
                Some(r#"{"type":"aws_iam"}"#),
            ),
        ];
        for (auth, expected) in cases {
            let conn = Connection {
                auth: auth.clone(),
                ..pg("a")
            };
            let json = serde_json::to_value(&conn).unwrap();
            let written = json.get("auth").map(|a| a.to_string());
            assert_eq!(written.as_deref(), expected);
            let back: Connection = serde_json::from_value(json).unwrap();
            assert_eq!(back.auth, auth);
        }
    }

    fn mongo(options: &str) -> Connection {
        Connection {
            adapter: Adapter::Mongodb,
            port: 27017,
            user: String::new(),
            mongo: Some(MongoSettings {
                srv: false,
                options: options.into(),
            }),
            ..pg("m")
        }
    }

    #[test]
    fn mongo_settings_are_saved_only_when_set() {
        let conn = mongo("replicaSet=rs0");
        let json = serde_json::to_value(&conn).unwrap();
        assert_eq!(json["adapter"], "mongodb");
        assert_eq!(
            json["mongo"],
            serde_json::json!({"options": "replicaSet=rs0"})
        );
        assert_eq!(serde_json::from_value::<Connection>(json).unwrap(), conn);
        let srv = Connection {
            mongo: Some(MongoSettings {
                srv: true,
                options: String::new(),
            }),
            ..conn
        };
        let json = serde_json::to_value(&srv).unwrap();
        assert_eq!(json["mongo"], serde_json::json!({"srv": true}));
        // Other engines don't write the key at all.
        assert!(
            serde_json::to_value(pg("a"))
                .unwrap()
                .get("mongo")
                .is_none()
        );
        let documentdb: Connection = serde_json::from_value(serde_json::json!({
            "name": "d", "adapter": "documentdb", "host": "h", "port": 27017, "user": "u",
        }))
        .unwrap();
        assert_eq!(documentdb.adapter, Adapter::Documentdb);
        assert_eq!(documentdb.mongo, None);
    }

    #[test]
    fn mongo_connections_are_validated() {
        assert!(mongo("").validate().is_ok(), "no user is fine on MongoDB");
        assert!(mongo("authSource=admin&replicaSet=rs0").validate().is_ok());
        for options in [
            "password=x",
            "authSource=admin&PASSWORD=x",
            "pass%77ord=x",
            "pwd=x",
            "tlsCertificateKeyFilePassword=x",
            "proxyPassword=x",
            "authMechanism=MONGODB-AWS&authMechanismProperties=AWS_SESSION_TOKEN:abc",
            "authMechanismProperties=aws_session_token%3Aabc",
        ] {
            assert!(mongo(options).validate().is_err(), "{options}");
        }
        let iam = Connection {
            auth: Auth::AwsIam(AwsIamAuth::default()),
            tls: TlsMode::Require,
            user: "u".into(),
            ..mongo("")
        };
        assert!(iam.validate().unwrap_err().to_string().contains("IAM"));
        let documentdb_iam = Connection {
            adapter: Adapter::Documentdb,
            ..iam
        };
        assert!(documentdb_iam.validate().is_err());
        let tunnel = SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "ec2-user".into(),
            key_path: String::new(),
            fingerprint: "SHA256:abc".into(),
        };
        let mut srv = mongo("");
        srv.mongo.as_mut().unwrap().srv = true;
        assert!(srv.validate().is_ok());
        srv.ssh = Some(tunnel.clone());
        assert!(srv.validate().unwrap_err().to_string().contains("SRV"));
        let mut tunneled = mongo("");
        tunneled.ssh = Some(tunnel);
        assert!(tunneled.validate().is_ok());
        // Only document engines take these settings.
        let mut wrong = pg("p");
        wrong.mongo = Some(MongoSettings::default());
        assert!(wrong.validate().is_err());
    }

    #[test]
    fn a_connection_string_is_split_into_fields() {
        let uri = parse_mongo_uri(
            " mongodb://ada%40corp:p%40ss%3Aw0rd@db1.example.com:27018,db2.example.com/app\
             ?replicaSet=rs0&authSource=admin ",
        )
        .unwrap();
        assert_eq!(
            uri,
            MongoUri {
                srv: false,
                host: "db1.example.com".into(),
                port: Some(27018),
                other_hosts: 1,
                user: "ada@corp".into(),
                password: Some("p@ss:w0rd".into()),
                database: "app".into(),
                options: "replicaSet=rs0&authSource=admin".into(),
                dropped: Vec::new(),
            }
        );
    }

    #[test]
    fn an_srv_string_has_no_port_and_secret_options_are_left_out() {
        let uri = parse_mongo_uri(
            "MONGODB+SRV://u@cluster0.example.net/?retryWrites=true&password=x\
             &authMechanismProperties=AWS_SESSION_TOKEN:t&w=majority",
        )
        .unwrap();
        assert!(uri.srv);
        assert_eq!(uri.host, "cluster0.example.net");
        assert_eq!(uri.port, None);
        assert_eq!(uri.user, "u");
        assert_eq!(uri.password, None);
        assert_eq!(uri.database, "");
        assert_eq!(uri.options, "retryWrites=true&w=majority");
        assert_eq!(uri.dropped, ["password", "authMechanismProperties"]);
        let settings = MongoSettings {
            srv: true,
            options: uri.options,
        };
        assert!(settings.check().is_ok());
    }

    #[test]
    fn plain_and_ipv6_hosts_and_bad_strings() {
        let uri = parse_mongo_uri("mongodb://localhost").unwrap();
        assert_eq!((uri.host.as_str(), uri.port), ("localhost", None));
        let uri = parse_mongo_uri("mongodb://[::1]:57017/x").unwrap();
        assert_eq!((uri.host.as_str(), uri.port), ("[::1]", Some(57017)));
        assert_eq!(uri.database, "x");
        for bad in [
            "postgres://u:secret@h",
            "mongodb://u:secret@",
            "mongodb://u:secret@h:port",
            "mongodb://u:secret@[::1",
        ] {
            let error = parse_mongo_uri(bad).unwrap_err().to_string();
            assert!(!error.contains("secret"), "{error}");
        }
    }

    #[test]
    fn percent_decoding_reads_escapes_and_leaves_the_rest() {
        assert_eq!(percent_decode("a%20b%3d"), "a b=");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn a_source_is_saved_without_secrets() {
        let conn = Connection {
            source: Some(Source {
                provider: Provider::Aws,
                resource: "arn:aws:rds:eu-west-1:123456789012:db:orders".into(),
                region: "eu-west-1".into(),
            }),
            ..pg("a")
        };
        let json = serde_json::to_value(&conn).unwrap();
        assert_eq!(
            json["source"],
            serde_json::json!({
                "provider": "aws",
                "resource": "arn:aws:rds:eu-west-1:123456789012:db:orders",
                "region": "eu-west-1",
            })
        );
        assert_eq!(serde_json::from_value::<Connection>(json).unwrap(), conn);
    }

    #[test]
    fn an_iam_token_needs_a_user_a_host_and_encryption() {
        let mut config = Config::default();
        let mut conn = iam("a");
        conn.tls = TlsMode::Prefer;
        let error = config.upsert(conn.clone(), None).unwrap_err().to_string();
        assert!(error.contains("encrypted"), "{error}");
        conn.tls = TlsMode::Verify;
        assert!(config.upsert(conn.clone(), None).is_ok());
        conn.user.clear();
        assert!(config.upsert(conn.clone(), Some("a")).is_err());
        conn.user = "dev".into();
        conn.host.clear();
        assert!(config.upsert(conn, Some("a")).is_err());
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join(CONFIG_FILE)).unwrap();
        assert!(config.connections.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CONFIG_FILE);
        let mut config = Config::default();
        config.upsert(pg("app-db"), None).unwrap();
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);
    }

    #[test]
    fn upsert_rejects_duplicate_names() {
        let mut config = Config::default();
        config.upsert(pg("a"), None).unwrap();
        assert!(config.upsert(pg("a"), None).is_err());
    }

    #[test]
    fn upsert_renames_in_place() {
        let mut config = Config::default();
        config.upsert(pg("a"), None).unwrap();
        config.upsert(pg("b"), Some("a")).unwrap();
        assert_eq!(config.connections.len(), 1);
        assert_eq!(config.connections[0].name, "b");
    }

    #[test]
    fn upsert_requires_name_host_user() {
        let mut config = Config::default();
        let mut conn = pg("");
        assert!(config.upsert(conn.clone(), None).is_err());
        conn.name = "x".into();
        conn.host = " ".into();
        assert!(config.upsert(conn, None).is_err());
    }

    #[test]
    fn serverless_iam_needs_no_user_but_a_cluster_does() {
        let mut config = Config::default();
        let mut conn = pg("wg");
        conn.adapter = Adapter::Redshift;
        conn.user.clear();
        conn.tls = TlsMode::Require;
        conn.auth = Auth::AwsIam(AwsIamAuth::default());
        conn.host = "analytics.123456789012.eu-west-1.redshift-serverless.amazonaws.com".into();
        assert!(conn.iam_names_the_user());
        config.upsert(conn.clone(), None).unwrap();
        // A cluster signs in as the user it is asked for.
        conn.name = "cluster".into();
        conn.host = "warehouse.abc123.eu-west-1.redshift.amazonaws.com".into();
        assert!(!conn.iam_names_the_user());
        assert!(config.upsert(conn.clone(), None).is_err());
        // So does a workgroup with a password.
        conn.host = "analytics.123456789012.eu-west-1.redshift-serverless.amazonaws.com".into();
        conn.auth = Auth::Password;
        assert!(!conn.iam_names_the_user());
        assert!(config.upsert(conn, None).is_err());
        assert!(is_redshift_serverless_host(
            "WG.123456789012.cn-north-1.redshift-serverless.amazonaws.com.cn."
        ));
        assert!(!is_redshift_serverless_host(
            "redshift-serverless.amazonaws.com"
        ));
    }

    fn in_folder(name: &str, folder: &str) -> Connection {
        let mut conn = pg(name);
        conn.folder = folder.into();
        conn
    }

    /// `(indented folder or connection)` lines, depth first.
    fn outline(config: &Config) -> Vec<String> {
        fn walk(node: &FolderNode<'_>, depth: usize, out: &mut Vec<String>) {
            for folder in &node.folders {
                out.push(format!(
                    "{}{}/",
                    "  ".repeat(depth),
                    folder_name(&folder.path)
                ));
                walk(folder, depth + 1, out);
            }
            for conn in &node.connections {
                out.push(format!("{}{}", "  ".repeat(depth), conn.name));
            }
        }
        let mut out = Vec::new();
        walk(&config.tree(), 0, &mut out);
        out
    }

    #[test]
    fn tree_nests_folders_before_connections_and_loose_ones_last() {
        let mut config = Config::default();
        for (name, folder) in [("z", ""), ("b", "Prod"), ("a", "Prod/EU"), ("d", "Dev")] {
            config.upsert(in_folder(name, folder), None).unwrap();
        }
        assert_eq!(
            outline(&config),
            ["Dev/", "  d", "Prod/", "  EU/", "    a", "  b", "z"]
        );
        assert_eq!(config.folders(), ["Dev", "Prod", "Prod/EU"]);
    }

    #[test]
    fn folders_sort_by_level_not_by_raw_text() {
        let mut config = Config::default();
        config.add_folder("", "a b").unwrap();
        let a = config.add_folder("", "a").unwrap();
        config.add_folder(&a, "z").unwrap();
        assert_eq!(config.folders(), ["a", "a/z", "a b"]);
    }

    #[test]
    fn empty_nested_folders_are_kept() {
        let mut config = Config::default();
        let top = config.add_folder("", " RGG ").unwrap();
        let child = config.add_folder(&top, "storefront").unwrap();
        assert_eq!(child, "RGG/storefront");
        assert_eq!(outline(&config), ["RGG/", "  storefront/"]);
    }

    #[test]
    fn empty_folders_survive_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let mut config = Config::default();
        let top = config.add_folder("", "RGG").unwrap();
        config.add_folder(&top, "storefront").unwrap();
        config.save_to(&path).unwrap();
        assert_eq!(
            Config::load_from(&path).unwrap().folders(),
            ["RGG", "RGG/storefront"]
        );
    }

    #[test]
    fn connection_count_includes_nested_folders() {
        let mut config = Config::default();
        config.upsert(in_folder("a", "P"), None).unwrap();
        config.upsert(in_folder("b", "P/Q"), None).unwrap();
        let tree = config.tree();
        assert_eq!(tree.folders[0].connection_count(), 2);
    }

    #[test]
    fn count_and_filtered_keep_only_matching_connections() {
        let mut config = Config::default();
        config.upsert(in_folder("a", "P"), None).unwrap();
        config.upsert(in_folder("b", "P/Q"), None).unwrap();
        config.upsert(in_folder("c", "R"), None).unwrap();
        config.add_folder("", "Empty").unwrap();
        let tree = config.tree();
        assert_eq!(tree.count(|c| c.name != "a"), 2);
        assert_eq!(tree.folders[1].count(|c| c.name == "b"), 1);
        assert_eq!(tree.count(|_| false), 0);

        let only_b = tree.filtered(|c| c.name == "b");
        assert_eq!(only_b.folders.len(), 1);
        assert_eq!(only_b.folders[0].path, "P");
        assert!(only_b.folders[0].connections.is_empty());
        assert_eq!(only_b.folders[0].folders[0].connections[0].name, "b");
        // A folder left with nothing, like an empty one, goes.
        assert_eq!(tree.folders.len(), 3);
        assert_eq!(tree.filtered(|_| true).folders.len(), 2);
        assert!(tree.filtered(|_| false).folders.is_empty());
    }

    #[test]
    fn a_search_matches_name_folder_or_host_in_any_case() {
        let mut conn = in_folder("shop-bear", "RGG/storefront");
        conn.host = "bear-db.internal".into();
        assert!(conn.matches(""));
        assert!(conn.matches("  "));
        assert!(conn.matches("BEAR"));
        assert!(conn.matches("storefront"));
        assert!(conn.matches("db.internal"));
        // Every word must be found, each anywhere.
        assert!(conn.matches("rgg bear"));
        assert!(!conn.matches("rgg fox"));
        // Not what it doesn't show, such as the user or the database.
        assert!(!conn.matches("dev"));
        let mut file = in_folder("local", "");
        file.adapter = Adapter::Sqlite;
        file.database = "/data/shop.db".into();
        assert!(file.matches("shop.db"));
    }

    #[test]
    fn folder_names_are_validated_per_level() {
        let mut config = Config::default();
        let p = config.add_folder("", "Prod").unwrap();
        assert!(config.add_folder("", "  ").is_err());
        assert!(config.add_folder("", "prod").is_err());
        assert!(config.add_folder("", "a/b").is_err());
        assert!(config.add_folder(&p, "prod").is_ok());
    }

    #[test]
    fn renaming_a_folder_updates_everything_inside() {
        let mut config = Config::default();
        config.upsert(in_folder("a", "P/Q"), None).unwrap();
        config.upsert(in_folder("b", "P"), None).unwrap();
        assert_eq!(config.rename_folder("P", "Live").unwrap(), "Live");
        assert_eq!(config.find("a").unwrap().folder, "Live/Q");
        assert_eq!(config.find("b").unwrap().folder, "Live");
    }

    #[test]
    fn renaming_can_change_only_case_but_not_clash() {
        let mut config = Config::default();
        config.add_folder("", "prod").unwrap();
        config.add_folder("", "Dev").unwrap();
        config.rename_folder("prod", "Prod").unwrap();
        assert!(config.rename_folder("Prod", "dev").is_err());
    }

    #[test]
    fn removing_a_folder_moves_its_contents_up_one_level() {
        let mut config = Config::default();
        config.upsert(in_folder("a", "P/Q"), None).unwrap();
        config.upsert(in_folder("b", "P/Q/R"), None).unwrap();
        config.remove_folder("P/Q");
        assert_eq!(config.find("a").unwrap().folder, "P");
        assert_eq!(config.find("b").unwrap().folder, "P/R");
        config.remove_folder("P");
        assert_eq!(config.find("a").unwrap().folder, "");
        assert_eq!(config.find("b").unwrap().folder, "R");
    }

    #[test]
    fn folders_move_with_their_contents() {
        let mut config = Config::default();
        config.upsert(in_folder("a", "P/Q"), None).unwrap();
        config.add_folder("", "T").unwrap();
        assert_eq!(config.move_folder("P/Q", "T").unwrap().unwrap(), "T/Q");
        assert_eq!(config.find("a").unwrap().folder, "T/Q");
        assert_eq!(config.move_folder("T/Q", "T").unwrap(), None);
    }

    #[test]
    fn a_folder_cannot_move_inside_itself() {
        let mut config = Config::default();
        config.add_folder("", "P").unwrap();
        config.add_folder("P", "Q").unwrap();
        assert!(config.move_folder("P", "P/Q").is_err());
        assert!(config.move_folder("P", "P").is_err());
    }

    #[test]
    fn moving_a_connection_reports_whether_it_changed() {
        let mut config = Config::default();
        config.upsert(in_folder("a", "Prod"), None).unwrap();
        assert!(config.move_connection("a", "Dev"));
        assert!(!config.move_connection("a", "Dev"));
        assert!(!config.move_connection("missing", "Dev"));
    }

    #[test]
    fn a_connection_without_a_folder_does_not_write_the_field() {
        let json = serde_json::to_string(&pg("a")).unwrap();
        assert!(!json.contains("folder"));
    }

    #[test]
    fn update_check_defaults_to_on_for_existing_files() {
        let config: Config = serde_json::from_str(r#"{ "version": 1 }"#).unwrap();
        assert!(config.check_for_updates);
    }

    #[test]
    fn saving_over_a_file_this_version_cannot_read_keeps_that_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        // As written by a newer TupleBase, in a format this one doesn't know.
        let newer = r#"{"version": 3, "connections": [{"name": "kept"}]}"#;
        fs::write(&path, newer).unwrap();
        assert!(Config::load_from(&path).is_err());

        Config::default().save_to(&path).unwrap();

        assert!(Config::load_from(&path).is_ok());
        let kept: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".unreadable-"))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(fs::read_to_string(&kept[0]).unwrap(), newer);

        // A readable file is simply replaced, with no copy made.
        Config::default().save_to(&path).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn font_size_defaults_for_existing_files() {
        let config: Config = serde_json::from_str(r#"{ "version": 1 }"#).unwrap();
        assert_eq!(config.font_size, DEFAULT_FONT_SIZE);
    }

    #[test]
    fn appearance_follows_the_system_unless_chosen() {
        let config: Config = serde_json::from_str(r#"{ "version": 1 }"#).unwrap();
        assert_eq!(config.appearance, Appearance::System);
        // Following the system is the default, so it isn't written.
        assert!(
            !serde_json::to_string(&config)
                .unwrap()
                .contains("appearance")
        );
        let dark = Config {
            appearance: Appearance::Dark,
            ..config
        };
        let text = serde_json::to_string(&dark).unwrap();
        assert!(text.contains(r#""appearance":"dark""#));
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.appearance, Appearance::Dark);
    }

    #[test]
    fn resizing_the_font_stops_at_the_limits() {
        let mut config = Config::default();
        config.resize_font(2);
        assert_eq!(config.font_size, DEFAULT_FONT_SIZE + 2);
        config.resize_font(i8::MAX);
        assert_eq!(config.font_size, *FONT_SIZES.end());
        config.resize_font(i8::MIN);
        assert_eq!(config.font_size, *FONT_SIZES.start());
    }

    #[test]
    fn a_font_size_out_of_range_in_the_file_is_brought_back_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, r#"{ "version": 1, "font_size": 200 }"#).unwrap();
        assert_eq!(
            Config::load_from(&path).unwrap().font_size,
            *FONT_SIZES.end()
        );
    }

    #[test]
    fn adapter_serializes_lowercase() {
        let json = serde_json::to_string(&pg("a")).unwrap();
        assert!(json.contains(r#""adapter":"postgres""#));
    }

    /// A file from a newer version: the second connection uses an engine this version
    /// doesn't know, the fourth a sign-in, the fifth a TLS mode; two entries aren't even
    /// connections. Version 2, though no connection here needs it.
    const NEWER_FILE: &str = r#"{
  "version": 2,
  "folders": [
    "Prod"
  ],
  "connections": [
    {
      "name": "plain",
      "adapter": "postgres",
      "host": "db.example.com",
      "port": 5432,
      "user": "dev",
      "readonly": false
    },
    {
      "name": "Future",
      "folder": "Prod/Old",
      "adapter": "oracle",
      "host": "ora.example.com",
      "port": 1521,
      "user": "scott",
      "readonly": false,
      "wallet": {
        "z": 1,
        "a": [
          1.5,
          null
        ]
      }
    },
    {
      "name": "second",
      "adapter": "mysql",
      "host": "10.0.0.5",
      "port": 3306,
      "user": "app",
      "readonly": false
    },
    {
      "name": "vault",
      "adapter": "postgres",
      "host": "db.example.com",
      "port": 5432,
      "user": "dev",
      "readonly": false,
      "auth": {
        "type": "hashicorp_vault",
        "path": "db/creds"
      }
    },
    {
      "name": "pinned",
      "adapter": "postgres",
      "host": "db.example.com",
      "port": 5432,
      "user": "dev",
      "readonly": false,
      "tls": "verify-pinned"
    },
    42,
    {
      "adapter": "postgres"
    }
  ],
  "check_for_updates": true,
  "font_size": 15,
  "theme_accent": "teal"
}"#;

    fn names(config: &Config) -> Vec<&str> {
        config.connections.iter().map(|c| &*c.name).collect()
    }

    #[test]
    fn connections_this_version_cannot_read_are_hidden_but_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, format!("{NEWER_FILE}\n")).unwrap();
        let (config, message) = Config::load_or_keep_from(&path);
        assert_eq!(message, None);
        assert_eq!(names(&config), ["plain", "second"]);
        assert_eq!(config.unsupported_count(), 5);
        // Nothing offers them: not the tree, not a lookup by name (which MCP uses).
        assert_eq!(config.tree().connection_count(), 2);
        assert_eq!(config.find("Future"), None);
        assert_eq!(config.find("vault"), None);
        // Their folders aren't shown either.
        assert_eq!(config.folders(), ["Prod"]);

        // Saved back unchanged, in place, with the unknown top-level setting.
        config.save_to(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{NEWER_FILE}\n")
        );
        assert_eq!(files_named(dir.path(), "unreadable").len(), 0);
        let raw = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        assert_eq!(raw(&fs::read_to_string(&path).unwrap()), raw(NEWER_FILE));
    }

    #[test]
    fn kept_connections_survive_edits_to_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, NEWER_FILE).unwrap();
        let mut config = Config::load_from(&path).unwrap();
        config.remove("plain");
        config.upsert(pg("added"), None).unwrap();
        config.remove_folder("Prod");
        config.add_folder("", "Prod").unwrap();
        config.rename_folder("Prod", "Live").unwrap();
        config.save_to(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();
        let original: Value = serde_json::from_str(NEWER_FILE).unwrap();
        let kept = |file: &Value| -> Vec<Value> {
            file["connections"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| Connection::deserialize(*c).is_err())
                .cloned()
                .collect()
        };
        // Each byte-for-byte as it was, its folder included.
        assert_eq!(kept(&file), kept(&original));
        assert_eq!(file["theme_accent"], "teal");
        let config = Config::load_from(&path).unwrap();
        assert_eq!(names(&config), ["second", "added"]);
        assert_eq!(config.unsupported_count(), 5);
        assert_eq!(config.folders(), ["Live"]);
    }

    #[test]
    fn a_name_a_kept_connection_has_is_taken() {
        let mut config: Config = serde_json::from_str(NEWER_FILE).unwrap();
        for name in ["future", "FUTURE", "Vault", "pinned"] {
            let error = config.upsert(pg(name), None).unwrap_err().to_string();
            assert!(error.contains("newer TupleBase"), "{name}: {error}");
            let error = config.upsert(pg(name), Some("plain")).unwrap_err();
            assert!(error.to_string().contains("already exists"), "{name}");
        }
        assert_eq!(names(&config), ["plain", "second"]);
        config.upsert(pg("present"), None).unwrap();
        config.upsert(pg("plain2"), Some("plain")).unwrap();
        assert_eq!(names(&config), ["plain2", "second", "present"]);
    }

    #[test]
    fn kept_connections_never_lower_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, NEWER_FILE).unwrap();
        let mut config = Config::load_from(&path).unwrap();
        assert_eq!(config.version, VERSION_IAM);
        config.upsert(iam("iam"), None).unwrap();
        config.remove("iam");
        assert_eq!(config.version, VERSION_IAM);
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap().version, VERSION_IAM);
        // A version 1 file with one stays at version 1 until an IAM connection needs 2.
        let v1 = NEWER_FILE.replacen(r#""version": 2"#, r#""version": 1"#, 1);
        let mut config: Config = serde_json::from_str(&v1).unwrap();
        config.sync_version();
        assert_eq!(config.version, VERSION_BASE);
        config.upsert(iam("iam"), None).unwrap();
        assert_eq!(config.version, VERSION_IAM);
        // A version this one doesn't know is still refused as a whole.
        let v3 = NEWER_FILE.replacen(r#""version": 2"#, r#""version": 3"#, 1);
        let error = format!("{:#}", Config::parse(v3.as_bytes()).unwrap_err());
        assert!(error.contains("unsupported config version 3"), "{error}");
    }

    #[test]
    fn a_save_keeps_connections_a_newer_version_added_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, V1_FILE).unwrap();
        let mut config = Config::load_from(&path).unwrap();
        // A newer TupleBase saves the file while this one runs.
        fs::write(&path, NEWER_FILE).unwrap();
        config.upsert(pg("mine"), None).unwrap();
        assert_eq!(config.save_to(&path).unwrap(), None);
        let saved = Config::load_from(&path).unwrap();
        assert_eq!(saved.unsupported_count(), 5);
        assert_eq!(saved.version, VERSION_IAM);
        assert!(saved.find("mine").is_some());
        assert_eq!(saved.find("plain"), config.find("plain"));
    }
}
