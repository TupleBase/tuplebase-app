//! Adds the MCP server to the AI agents on this computer.
//!
//! Each agent keeps its servers in its own config file. An edit changes only the
//! server's entry: everything else stays, and a file that can't be parsed is left alone.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::updates;

/// Overrides the home directory agents are looked up in, so a development build can try
/// this without touching real agents.
pub const HOME_ENV: &str = "TUPLEBASE_AGENTS_HOME";

const SERVER_NAME: &str = "tuplebase";
/// The table Codex keeps its servers in.
const TOML_SERVERS: &str = "mcp_servers";

#[derive(Clone, Copy)]
enum Format {
    /// JSON with servers under `key`; `typed` agents also want `"type": "stdio"`.
    Json {
        key: &'static str,
        typed: bool,
    },
    Toml,
}

struct Agent {
    name: &'static str,
    /// Exists when the agent is installed.
    marker: PathBuf,
    config: PathBuf,
    format: Format,
}

#[derive(Clone, Copy)]
enum Os {
    Mac,
    Windows,
    Linux,
}

fn current_os() -> Os {
    if cfg!(target_os = "macos") {
        Os::Mac
    } else if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    }
}

fn known_agents(home: &Path, os: Os) -> Vec<Agent> {
    let app_data = match os {
        Os::Mac => home.join("Library/Application Support"),
        Os::Windows => home.join("AppData/Roaming"),
        Os::Linux => home.join(".config"),
    };
    let plain = Format::Json {
        key: "mcpServers",
        typed: false,
    };
    vec![
        Agent {
            name: "Claude Code",
            marker: home.join(".claude"),
            config: home.join(".claude.json"),
            format: Format::Json {
                key: "mcpServers",
                typed: true,
            },
        },
        Agent {
            name: "Claude Desktop",
            marker: app_data.join("Claude"),
            config: app_data.join("Claude/claude_desktop_config.json"),
            format: plain,
        },
        Agent {
            name: "Cursor",
            marker: home.join(".cursor"),
            config: home.join(".cursor/mcp.json"),
            format: plain,
        },
        Agent {
            name: "Windsurf",
            marker: home.join(".codeium/windsurf"),
            config: home.join(".codeium/windsurf/mcp_config.json"),
            format: plain,
        },
        Agent {
            name: "VS Code",
            marker: app_data.join("Code/User"),
            config: app_data.join("Code/User/mcp.json"),
            format: Format::Json {
                key: "servers",
                typed: true,
            },
        },
        Agent {
            name: "Codex",
            marker: home.join(".codex"),
            config: home.join(".codex/config.toml"),
            format: Format::Toml,
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Added,
    NotAdded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub name: &'static str,
    pub state: AgentState,
}

struct Target {
    home: PathBuf,
    server: PathBuf,
}

/// Where agents live and the server to give them. `None` for a development build, which
/// must not point real agents at a binary that comes and goes.
fn target() -> Option<Target> {
    let exe = std::env::current_exe().ok()?;
    let home = match std::env::var_os(HOME_ENV) {
        Some(home) => PathBuf::from(home),
        None => {
            updates::detect_installation(&exe)?;
            directories::BaseDirs::new()?.home_dir().to_path_buf()
        }
    };
    Some(Target {
        home,
        server: server_next_to(&exe)?,
    })
}

pub(crate) fn server_next_to(exe: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "tuplebase-mcp.exe"
    } else {
        "tuplebase-mcp"
    };
    let path = exe.parent()?.join(name);
    path.is_file().then_some(path)
}

/// The agents installed here. Reads files; call it off the UI thread.
pub fn statuses() -> Vec<AgentStatus> {
    target().map_or_else(Vec::new, |t| {
        statuses_in(&t.home, current_os(), &t.server.to_string_lossy())
    })
}

/// Adds the server to every installed agent. Writes files; call it off the UI thread.
pub fn add_to_all() -> Vec<AgentStatus> {
    target().map_or_else(Vec::new, |t| {
        add_in(&t.home, current_os(), &t.server.to_string_lossy(), None)
    })
}

/// Adds the server to the one installed agent called `name` and reports that agent only.
/// Writes a file; call it off the UI thread.
pub fn add_to(name: &str) -> Vec<AgentStatus> {
    target().map_or_else(Vec::new, |t| {
        add_in(
            &t.home,
            current_os(),
            &t.server.to_string_lossy(),
            Some(name),
        )
    })
}

fn installed(home: &Path, os: Os) -> impl Iterator<Item = Agent> {
    known_agents(home, os)
        .into_iter()
        .filter(|agent| agent.marker.exists())
}

fn statuses_in(home: &Path, os: Os, server: &str) -> Vec<AgentStatus> {
    installed(home, os)
        .map(|agent| {
            let change =
                read_config(&agent.config).and_then(|text| change(agent.format, &text, server));
            let state = match change {
                Ok(None) => AgentState::Added,
                Ok(Some(_)) => AgentState::NotAdded,
                Err(e) => AgentState::Failed(format!("{e:#}")),
            };
            AgentStatus {
                name: agent.name,
                state,
            }
        })
        .collect()
}

/// Adds the server to the agent called `only`, or to all of them, and reports those.
fn add_in(home: &Path, os: Os, server: &str, only: Option<&str>) -> Vec<AgentStatus> {
    installed(home, os)
        .filter(|agent| only.is_none_or(|name| name == agent.name))
        .map(|agent| {
            let state = match add(&agent, server) {
                Ok(()) => AgentState::Added,
                Err(e) => AgentState::Failed(format!("{e:#}")),
            };
            AgentStatus {
                name: agent.name,
                state,
            }
        })
        .collect()
}

fn add(agent: &Agent, server: &str) -> Result<()> {
    let text = read_config(&agent.config)?;
    if let Some(updated) = change(agent.format, &text, server)? {
        write_keeping_file(&agent.config, &updated)?;
    }
    Ok(())
}

/// A file that doesn't exist yet reads as empty.
fn read_config(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The file's new text, or `None` when it already points at `server`.
fn change(format: Format, text: &str, server: &str) -> Result<Option<String>> {
    match format {
        Format::Json { key, typed } => change_json(text, key, typed, server),
        Format::Toml => change_toml(text, server),
    }
}

/// Only touches `command` and a missing `type`, so settings such as a server's `env`
/// survive.
fn change_json(text: &str, key: &str, typed: bool, server: &str) -> Result<Option<String>> {
    let mut root: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(text).context("the file isn't plain JSON, so it was left alone")?
    };
    let servers = root
        .as_object_mut()
        .context("the file isn't a JSON object, so it was left alone")?
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .with_context(|| format!("`{key}` isn't an object, so the file was left alone"))?;
    let entry = servers
        .entry(SERVER_NAME)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("the existing entry isn't an object, so the file was left alone")?;
    let mut changed = false;
    if entry.get("command").and_then(Value::as_str) != Some(server) {
        entry.insert("command".to_owned(), json!(server));
        changed = true;
    }
    if typed && !entry.contains_key("type") {
        entry.insert("type".to_owned(), json!("stdio"));
        changed = true;
    }
    if !changed {
        return Ok(None);
    }
    let text = serde_json::to_string_pretty(&root).context("formatting the file")?;
    Ok(Some(format!("{text}\n")))
}

/// Edits the parsed document, so the rest of the file keeps its comments and layout, and
/// the entry is found however it is written: `[mcp_servers.tuplebase]`, a quoted key, or
/// `tuplebase = { … }` under `[mcp_servers]`. Only touches `command`, like JSON.
fn change_toml(text: &str, server: &str) -> Result<Option<String>> {
    // Lines added here end in `\n`; a Windows file keeps its `\r\n` throughout.
    let crlf = text.contains("\r\n");
    let mut doc: toml_edit::DocumentMut = text
        .replace("\r\n", "\n")
        .parse()
        .context("the file isn't valid TOML, so it was left alone")?;
    // A new table goes at the end, after comments that ended the file, and a blank line.
    let mut prefix = doc.trailing().as_str().unwrap_or_default().to_owned();
    if !text.trim().is_empty() {
        prefix.push('\n');
    }
    let mut header_added = false;
    let servers = doc.entry(TOML_SERVERS).or_insert_with(|| {
        // No `[mcp_servers]` header of its own, only `[mcp_servers.tuplebase]`.
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        table.into()
    });
    let inline = servers.is_inline_table();
    let servers = servers
        .as_table_like_mut()
        .with_context(|| format!("`{TOML_SERVERS}` isn't a table, so the file was left alone"))?;
    let entry = servers
        .entry(SERVER_NAME)
        .or_insert_with(|| {
            // A table header can't go inside `mcp_servers = { … }`.
            if inline {
                return toml_edit::value(toml_edit::InlineTable::new());
            }
            header_added = true;
            let mut table = toml_edit::Table::new();
            table.decor_mut().set_prefix(prefix);
            table.into()
        })
        .as_table_like_mut()
        .context("the existing entry isn't a table, so the file was left alone")?;
    if entry.get("command").and_then(toml_edit::Item::as_str) == Some(server) {
        return Ok(None);
    }
    entry.insert("command", toml_edit::value(server));
    if header_added {
        doc.set_trailing("");
    }
    let updated = doc.to_string();
    Ok(Some(if crlf {
        updated.replace('\n', "\r\n")
    } else {
        updated
    }))
}

/// Replaces the file atomically, following a symlink and keeping its permissions: these
/// files can hold tokens.
fn write_keeping_file(path: &Path, text: &str) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let permissions = fs::metadata(&target).ok().map(|m| m.permissions());
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = target.with_extension("tuplebase-tmp");
    let _ = fs::remove_file(&tmp);
    write_private(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    if let Some(permissions) = permissions {
        fs::set_permissions(&tmp, permissions)
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    fs::rename(&tmp, &target).with_context(|| format!("writing {}", target.display()))
}

/// Not readable by others even for a moment.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = fs::File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "/opt/tb/tuplebase-mcp";

    fn added(home: &Path, marker: &str) {
        fs::create_dir_all(home.join(marker)).unwrap();
    }

    fn state_of(statuses: &[AgentStatus], name: &str) -> AgentState {
        statuses
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .state
            .clone()
    }

    #[test]
    fn only_installed_agents_are_listed() {
        let home = tempfile::tempdir().unwrap();
        added(home.path(), ".cursor");
        let statuses = statuses_in(home.path(), Os::Linux, SERVER);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].name, "Cursor");
        assert_eq!(statuses[0].state, AgentState::NotAdded);
    }

    #[test]
    fn adding_creates_the_config_and_is_then_reported_as_added() {
        let home = tempfile::tempdir().unwrap();
        added(home.path(), ".cursor");
        let result = add_in(home.path(), Os::Linux, SERVER, None);
        assert_eq!(state_of(&result, "Cursor"), AgentState::Added);
        let written: Value = serde_json::from_str(
            &fs::read_to_string(home.path().join(".cursor/mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["mcpServers"]["tuplebase"]["command"], SERVER);
        let again = statuses_in(home.path(), Os::Linux, SERVER);
        assert_eq!(state_of(&again, "Cursor"), AgentState::Added);
    }

    #[test]
    fn adding_to_one_agent_leaves_the_others_alone() {
        let home = tempfile::tempdir().unwrap();
        added(home.path(), ".cursor");
        added(home.path(), ".codex");
        let result = add_in(home.path(), Os::Linux, SERVER, Some("Codex"));
        assert_eq!(result.len(), 1);
        assert_eq!(state_of(&result, "Codex"), AgentState::Added);
        assert!(!home.path().join(".cursor/mcp.json").exists());
    }

    #[test]
    fn vs_code_uses_its_own_key_and_type() {
        let updated = change_json("", "servers", true, SERVER).unwrap().unwrap();
        let value: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(value["servers"]["tuplebase"]["type"], "stdio");
        assert_eq!(value["servers"]["tuplebase"]["command"], SERVER);
    }

    #[test]
    fn other_settings_and_their_order_survive() {
        let text = r#"{"zeta":1,"mcpServers":{"other":{"command":"x"}},"alpha":2}"#;
        let updated = change_json(text, "mcpServers", false, SERVER)
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["zeta", "mcpServers", "alpha"]);
    }

    #[test]
    fn a_moved_app_updates_the_path_but_keeps_the_entrys_env() {
        let text = r#"{"mcpServers":{"tuplebase":{"command":"/old","env":{"TUPLEBASE_MCP_ALLOW_WRITES":"1"}}}}"#;
        let updated = change_json(text, "mcpServers", false, SERVER)
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&updated).unwrap();
        let entry = &value["mcpServers"]["tuplebase"];
        assert_eq!(entry["command"], SERVER);
        assert_eq!(entry["env"]["TUPLEBASE_MCP_ALLOW_WRITES"], "1");
    }

    #[test]
    fn a_new_entry_never_turns_on_writes() {
        let updated = change_json("", "mcpServers", false, SERVER)
            .unwrap()
            .unwrap();
        assert!(!updated.contains("ALLOW_WRITES"));
    }

    #[test]
    fn an_unchanged_entry_needs_no_write() {
        let text = format!(r#"{{"mcpServers":{{"tuplebase":{{"command":"{SERVER}"}}}}}}"#);
        assert_eq!(
            change_json(&text, "mcpServers", false, SERVER).unwrap(),
            None
        );
    }

    #[test]
    fn a_file_with_comments_is_left_alone_and_reported() {
        let home = tempfile::tempdir().unwrap();
        added(home.path(), ".cursor");
        let config = home.path().join(".cursor/mcp.json");
        let original = "// mine\n{\"mcpServers\":{}}\n";
        fs::write(&config, original).unwrap();
        let result = add_in(home.path(), Os::Linux, SERVER, None);
        assert!(matches!(state_of(&result, "Cursor"), AgentState::Failed(_)));
        assert_eq!(fs::read_to_string(config).unwrap(), original);
    }

    /// The updated file: valid, with one server entry that runs `server`, and needing no
    /// second change.
    fn toml_changed(text: &str, server: &str) -> String {
        let updated = change_toml(text, server).unwrap().expect("a change");
        let doc: toml_edit::DocumentMut = updated.parse().expect("valid TOML");
        let servers = doc[TOML_SERVERS].as_table_like().unwrap();
        assert_eq!(
            servers
                .iter()
                .filter(|(key, _)| *key == SERVER_NAME)
                .count(),
            1
        );
        assert_eq!(
            servers.get(SERVER_NAME).unwrap()["command"].as_str(),
            Some(server)
        );
        assert_eq!(change_toml(&updated, server).unwrap(), None);
        updated
    }

    #[test]
    fn toml_is_appended_without_disturbing_the_file() {
        for text in ["# only a comment\n", "model = \"x\"\n# tail\n"] {
            assert!(toml_changed(text, SERVER).starts_with(text));
        }
        let text = "# mine\nmodel = \"x\"\n";
        let updated = toml_changed(text, SERVER);
        assert!(updated.starts_with(text));
        assert!(updated.ends_with(&format!(
            "\n\n[mcp_servers.tuplebase]\ncommand = \"{SERVER}\"\n"
        )));
        assert_eq!(
            toml_changed("", SERVER),
            format!("[mcp_servers.tuplebase]\ncommand = \"{SERVER}\"\n")
        );
    }

    #[test]
    fn toml_only_replaces_this_servers_command() {
        let text = "[mcp_servers.other]\ncommand = \"a\"\n\n[mcp_servers.tuplebase]\ncommand = \"/old\"\nenv = { A = \"1\" }\n";
        let updated = toml_changed(text, SERVER);
        assert!(updated.contains("[mcp_servers.other]\ncommand = \"a\""));
        assert!(updated.contains(&format!("command = \"{SERVER}\"\nenv = {{ A = \"1\" }}")));
    }

    #[test]
    fn toml_finds_the_entry_however_it_is_written() {
        for text in [
            "[mcp_servers.tuplebase] # mine\ncommand = \"/old\"\nenv = { A = \"1\" }\n",
            "[mcp_servers.\"tuplebase\"]\ncommand = \"/old\"\nenv = { A = \"1\" }\n",
            "[ mcp_servers . tuplebase ]\ncommand = \"/old\"\nenv = { A = \"1\" }\n",
            "[mcp_servers]\ntuplebase = { command = \"/old\", env = { A = \"1\" } }\n",
            "mcp_servers.tuplebase.command = \"/old\"\nmcp_servers.tuplebase.env = { A = \"1\" }\n",
            "mcp_servers = { tuplebase = { command = \"/old\", env = { A = \"1\" } } }\n",
        ] {
            let updated = toml_changed(text, SERVER);
            assert!(!updated.contains("/old"), "{updated}");
            assert!(updated.contains("A = \"1\""), "{updated}");
        }
    }

    #[test]
    fn toml_adds_to_servers_written_any_way() {
        for text in [
            "[mcp_servers]\nother = { command = \"a\" }\n",
            "[mcp_servers.other]\ncommand = \"a\"\n",
            "mcp_servers = { other = { command = \"a\" } }\n",
            "mcp_servers.other.command = \"a\"\n",
        ] {
            let updated = toml_changed(text, SERVER);
            assert!(updated.contains("command = \"a\""), "{updated}");
        }
    }

    #[test]
    fn toml_keeps_windows_line_endings() {
        let text = "# mine\r\n[mcp_servers.tuplebase]\r\ncommand = \"/old\"\r\n";
        let updated = toml_changed(text, SERVER);
        assert!(!updated.replace("\r\n", "").contains('\n'), "{updated:?}");
        let added = toml_changed("model = \"x\"\r\n", SERVER);
        assert!(!added.replace("\r\n", "").contains('\n'), "{added:?}");
    }

    #[test]
    fn invalid_toml_is_left_alone_and_reported() {
        let home = tempfile::tempdir().unwrap();
        added(home.path(), ".codex");
        let config = home.path().join(".codex/config.toml");
        for original in ["[mcp_servers\n", "mcp_servers = 1\n"] {
            fs::write(&config, original).unwrap();
            let result = add_in(home.path(), Os::Linux, SERVER, None);
            assert!(matches!(state_of(&result, "Codex"), AgentState::Failed(_)));
            assert_eq!(fs::read_to_string(&config).unwrap(), original);
        }
    }

    #[test]
    fn toml_escapes_windows_paths() {
        toml_changed("", r"C:\Tools\tuplebase-mcp.exe");
    }

    #[test]
    fn app_data_agents_follow_the_operating_system() {
        let home = Path::new("/h");
        let config = |os| {
            known_agents(home, os)
                .into_iter()
                .find(|a| a.name == "Claude Desktop")
                .unwrap()
                .config
        };
        assert!(config(Os::Mac).starts_with("/h/Library/Application Support/Claude"));
        assert!(config(Os::Windows).starts_with("/h/AppData/Roaming/Claude"));
        assert!(config(Os::Linux).starts_with("/h/.config/Claude"));
    }

    #[cfg(unix)]
    #[test]
    fn writing_keeps_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        fs::write(&real, "{}").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("link.json");
        symlink(&real, &link).unwrap();
        write_keeping_file(&link, "{\"a\":1}\n").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "{\"a\":1}\n");
        let mode = fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }
}
