//! Connection settings, saved as JSON in the user's config directory.
//!
//! The file never holds secrets; passwords live in the OS keychain (see
//! [`crate::secrets`]).

use std::collections::BTreeSet;
use std::fs;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Separates folder levels in a stored path, e.g. `RGG/storefront/bear`.
const FOLDER_SEPARATOR: char = '/';

/// The folder a folder path sits in; `""` at the top level.
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
}

impl Adapter {
    pub const ALL: [Adapter; 4] = [
        Adapter::Postgres,
        Adapter::Mariadb,
        Adapter::Mysql,
        Adapter::Sqlite,
    ];

    /// Whether the database is a local file, with no server, user or password.
    pub fn is_file(self) -> bool {
        self == Adapter::Sqlite
    }

    pub fn label(self) -> &'static str {
        match self {
            Adapter::Postgres => "PostgreSQL",
            Adapter::Mariadb => "MariaDB",
            Adapter::Mysql => "MySQL",
            Adapter::Sqlite => "SQLite",
        }
    }

    /// Quotes an identifier for this database.
    pub fn quote_ident(self, ident: &str) -> String {
        match self {
            Adapter::Postgres | Adapter::Sqlite => format!("\"{}\"", ident.replace('"', "\"\"")),
            Adapter::Mariadb | Adapter::Mysql => format!("`{}`", ident.replace('`', "``")),
        }
    }

    /// SQL showing the first rows of a table.
    pub fn preview_sql(self, schema: &str, table: &str) -> String {
        format!(
            "SELECT * FROM {}.{} LIMIT 100",
            self.quote_ident(schema),
            self.quote_ident(table)
        )
    }

    pub fn default_port(self) -> u16 {
        match self {
            Adapter::Postgres => 5432,
            Adapter::Mariadb | Adapter::Mysql => 3306,
            Adapter::Sqlite => 0,
        }
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

/// Sign in to the database with a short-lived RDS IAM token made from an AWS profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwsIamAuth {
    /// Profile in `~/.aws/config`. Empty means the default AWS credentials.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    /// The database's region. Empty means the one in its `*.rds.amazonaws.com` host
    /// name, or else the profile's.
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
    /// An RDS IAM auth token for the connection's user, made on every connect.
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
}

impl Connection {
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
            if self.user.trim().is_empty() {
                bail!("user is required for \"{}\"", self.name);
            }
            self.check_iam_encryption()?;
        }
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

impl FolderNode<'_> {
    /// Connections in this folder and every folder below it.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
            + self
                .folders
                .iter()
                .map(FolderNode::connection_count)
                .sum::<usize>()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The file format: [`VERSION_IAM`] once a connection uses an IAM token, which older
    /// versions of the app can't read, so that they refuse the file instead of failing to
    /// parse it; otherwise [`VERSION_BASE`]. Kept up to date by [`Config::upsert`] and
    /// [`Config::remove`].
    pub version: u32,
    /// Sidebar folder paths, so that empty folders are kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    #[serde(default)]
    pub connections: Vec<Connection>,
    /// Ask GitHub for a newer version at startup.
    #[serde(default = "enabled")]
    pub check_for_updates: bool,
    /// Size of the app's text, in pixels.
    #[serde(default = "default_font_size")]
    pub font_size: u8,
    /// Light or dark colors, or whichever the system uses.
    #[serde(default, skip_serializing_if = "Appearance::is_system")]
    pub appearance: Appearance,
    /// Why this config must not be saved: the file it stands in for couldn't be read or
    /// set aside, and saving would delete what is in it. Never in the file.
    #[serde(skip)]
    pub unsaved_because: Option<String>,
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
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{text}\n")).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Where the last file older versions can read is kept, once it is upgraded.
pub const BASE_VERSION_BACKUP: &str = "connections.v1-backup.json";

/// Copies a [`VERSION_BASE`] file at `path` to [`BASE_VERSION_BACKUP`] beside it, unless
/// that copy exists already.
fn keep_base_version(path: &Path) -> Result<()> {
    let backup = path.with_file_name(BASE_VERSION_BACKUP);
    if backup.exists() || !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let version = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("version").and_then(serde_json::Value::as_u64));
    if version == Some(u64::from(VERSION_BASE)) {
        fs::copy(path, &backup).with_context(|| format!("writing {}", backup.display()))?;
    }
    Ok(())
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

impl Config {
    /// Loads the user's config; a missing file is an empty config.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if !(VERSION_BASE..=VERSION_IAM).contains(&config.version) {
            bail!("unsupported config version {}", config.version);
        }
        config.resize_font(0);
        config.sync_version();
        Ok(config)
    }

    /// Loads the user's config like [`Config::load`], but a file that can't be read
    /// doesn't stop the app: it is moved aside, so that saving can't write over it, and
    /// the config starts empty. Also returns what to tell the user, then.
    pub fn load_or_set_aside() -> Result<(Self, Option<String>)> {
        Ok(Self::load_or_set_aside_from(&config_path()?))
    }

    pub fn load_or_set_aside_from(path: &Path) -> (Self, Option<String>) {
        let error = match Self::load_from(path) {
            Ok(config) => return (config, None),
            Err(e) => format!("{e:#}"),
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let kept = path.with_file_name(format!("connections.unreadable-{stamp}.json"));
        match fs::rename(path, &kept) {
            Ok(()) => (
                Self::default(),
                Some(format!(
                    "Couldn't read your connections ({error}). The file was kept as {}; \
                     TupleBase started without it.",
                    kept.display()
                )),
            ),
            Err(e) => {
                let config = Self {
                    unsaved_because: Some(format!(
                        "{} couldn't be read, so it isn't written over",
                        path.display()
                    )),
                    ..Self::default()
                };
                let message = format!(
                    "Couldn't read your connections ({error}), nor set the file aside ({e}). \
                     Changes won't be saved until {} is fixed or moved.",
                    path.display()
                );
                (config, Some(message))
            }
        }
    }

    /// Writes the config. The first time it raises a file to [`VERSION_IAM`], which
    /// older versions can't read, it keeps a copy of the old file as
    /// [`BASE_VERSION_BACKUP`].
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(reason) = &self.unsaved_because {
            bail!("not saved: {reason}");
        }
        let mut config = self.clone();
        config.sync_version();
        if config.version > VERSION_BASE {
            keep_base_version(path)?;
        }
        write_atomic(path, &serde_json::to_string_pretty(&config)?)
    }

    /// Sets the oldest file format that holds every connection.
    fn sync_version(&mut self) {
        let iam = self
            .connections
            .iter()
            .any(|c| matches!(c.auth, Auth::AwsIam(_)));
        self.version = if iam { VERSION_IAM } else { VERSION_BASE };
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
    pub fn upsert(&mut self, conn: Connection, previous: Option<&str>) -> Result<()> {
        conn.validate()?;
        let clash = self
            .connections
            .iter()
            .any(|c| c.name == conn.name && Some(c.name.as_str()) != previous);
        if clash {
            bail!("a connection named \"{}\" already exists", conn.name);
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
        }
    }

    #[test]
    fn identifiers_are_quoted_for_each_database() {
        assert_eq!(Adapter::Postgres.quote_ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(Adapter::Mariadb.quote_ident("a`b"), "`a``b`");
        assert_eq!(
            Adapter::Mysql.preview_sql("app", "users"),
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
        let error = Config::load_from(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported config version 3"), "{error}");
        fs::write(&path, r#"{ "version": 2 }"#).unwrap();
        assert!(Config::load_from(&path).is_ok());
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_never_written_over() {
        for text in [r#"{ "version": 3, "connections": [] }"#, "not json"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(CONFIG_FILE);
            fs::write(&path, text).unwrap();
            let (mut config, message) = Config::load_or_set_aside_from(&path);
            let message = message.expect("the user is told");
            assert_eq!(config.connections, []);
            let kept: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.to_string_lossy().contains("connections.unreadable-"))
                .collect();
            assert_eq!(kept.len(), 1, "{kept:?}");
            assert_eq!(fs::read_to_string(&kept[0]).unwrap(), text);
            assert!(message.contains(&*kept[0].to_string_lossy()), "{message}");
            // Saving afterwards writes a new file and leaves the kept one alone.
            config.resize_font(1);
            config.save_to(&path).unwrap();
            assert_eq!(fs::read_to_string(&kept[0]).unwrap(), text);
        }
    }

    #[test]
    fn a_config_standing_in_for_a_file_kept_in_place_is_never_saved() {
        // What a file that couldn't be set aside gives: saving would delete it.
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
        let (config, message) = Config::load_or_set_aside_from(&path);
        assert_eq!(message, None);
        assert_eq!(config, Config::load_from(&path).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
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
}
