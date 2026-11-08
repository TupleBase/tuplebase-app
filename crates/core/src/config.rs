//! Connection settings, saved as JSON in the user's config directory.
//!
//! The file never holds secrets; passwords live in the OS keychain (see
//! [`crate::secrets`]).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
}

impl Adapter {
    pub const ALL: [Adapter; 3] = [Adapter::Postgres, Adapter::Mariadb, Adapter::Mysql];

    pub fn label(self) -> &'static str {
        match self {
            Adapter::Postgres => "PostgreSQL",
            Adapter::Mariadb => "MariaDB",
            Adapter::Mysql => "MySQL",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Adapter::Postgres => 5432,
            Adapter::Mariadb | Adapter::Mysql => 3306,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// Unique name; also the keychain account for this connection's password.
    pub name: String,
    pub adapter: Adapter,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub database: String,
    pub user: String,
    /// Blocks writes from the app and from agents.
    #[serde(default)]
    pub readonly: bool,
}

impl Connection {
    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("connection name is required");
        }
        if self.host.trim().is_empty() {
            bail!("host is required for \"{}\"", self.name);
        }
        if self.user.trim().is_empty() {
            bail!("user is required for \"{}\"", self.name);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
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

    /// Writes atomically (temp file + rename) so a crash never leaves a half file.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
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
            adapter: Adapter::Postgres,
            host: "localhost".into(),
            port: 5432,
            database: "app".into(),
            user: "dev".into(),
            readonly: false,
        }
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
