//! Connection settings, saved as JSON in the user's config directory.
//!
//! The file never holds secrets; passwords live in the OS keychain (see
//! [`crate::secrets`]).

use std::collections::BTreeSet;
use std::fs;
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
}

impl Connection {
    /// Where it connects to, for display: the host, or a database file's name.
    pub fn location(&self) -> String {
        if self.adapter.is_file() {
            let name = Path::new(&self.database).file_name();
            name.map_or_else(
                || self.database.clone(),
                |n| n.to_string_lossy().into_owned(),
            )
        } else {
            self.host.clone()
        }
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
        } else {
            if self.host.trim().is_empty() {
                bail!("host is required for \"{}\"", self.name);
            }
            if self.user.trim().is_empty() {
                bail!("user is required for \"{}\"", self.name);
            }
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
    pub version: u32,
    /// Sidebar folder paths, so that empty folders are kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<String>,
    #[serde(default)]
    pub connections: Vec<Connection>,
    /// Ask GitHub for a newer version at startup.
    #[serde(default = "enabled")]
    pub check_for_updates: bool,
}

fn enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            folders: Vec::new(),
            connections: Vec::new(),
            check_for_updates: true,
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
        let config: Config =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if config.version != 1 {
            bail!("unsupported config version {}", config.version);
        }
        Ok(config)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_string_pretty(self)?)
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
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.connections.len();
        self.connections.retain(|c| c.name != name);
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
        assert!(!json.contains("tls") && !json.contains("ssh"));
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
    fn adapter_serializes_lowercase() {
        let json = serde_json::to_string(&pg("a")).unwrap();
        assert!(json.contains(r#""adapter":"postgres""#));
    }
}
