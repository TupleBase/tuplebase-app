//! Chat with the user's own AI agent about their databases.
//!
//! The agent is the one the user already has and is signed in to. Each message runs it
//! once, headless, with our MCP server as its only tool, so it can do here exactly what
//! the server allows and nothing else on this computer. Only Claude Code for now.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::agents::server_next_to;
use crate::config::{CONFIG_DIR_ENV, config_dir, write_atomic};

pub const AGENT_NAME: &str = "Claude Code";

const SERVER_NAME: &str = "tuplebase";
const TOOL_PREFIX: &str = "mcp__tuplebase__";
/// Tells the agent where our server is. Kept next to the app's config.
const MCP_CONFIG_FILE: &str = "chat-mcp.json";
/// The server's opt-in for writes. The panel never gives it.
const ALLOW_WRITES_ENV: &str = "TUPLEBASE_MCP_ALLOW_WRITES";
/// The agent prints its whole input; the panel shows this much of it.
const MAX_INPUT_CHARS: usize = 300;
const INSTRUCTIONS: &str = "You are answering in the chat panel of TupleBase, a database client. \
    Your only tools are TupleBase's: they reach the databases the user saved in the app. \
    Work only on the connections open in the app: call list_open_connections first, and if \
    none is open ask the user to open one. Keep answers short.";

/// Something the agent did while answering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Markdown.
    Text(String),
    /// A call to one of the server's tools, with its arguments as JSON.
    Tool {
        name: String,
        input: String,
    },
    ToolFailed(String),
}

/// One line of the agent's output.
#[derive(Debug, Default, PartialEq, Eq)]
struct Line {
    events: Vec<Event>,
    session: Option<String>,
    /// The agent's own report that the turn failed.
    error: Option<String>,
}

/// Sends `prompt` and reports what the agent does until it has answered. `session`
/// continues an earlier conversation; the result is the session to pass next time.
/// Blocks for the whole answer; call it off the UI thread.
pub fn ask(
    prompt: &str,
    session: Option<&str>,
    mut on_event: impl FnMut(Event),
) -> Result<Option<String>> {
    let agent = find_agent().with_context(|| {
        format!("{AGENT_NAME} was not found. Install it and sign in, then try again.")
    })?;
    let server = std::env::current_exe()
        .ok()
        .and_then(|exe| server_next_to(&exe))
        .context("the MCP server is missing next to the app")?;

    // In a file, not on the command line: Windows can't pass JSON's quotes to an agent
    // that was installed as a batch file.
    let config_dir = config_dir()?;
    let servers = config_dir.join(MCP_CONFIG_FILE);
    write_atomic(&servers, &mcp_config(&server, &config_dir))?;

    let mut command = Command::new(agent);
    command
        .args(["--print", "--verbose", "--output-format", "stream-json"])
        .args(["--strict-mcp-config", "--mcp-config"])
        .arg(servers)
        // No tools of its own, and ours need no asking: the server guards itself.
        .args(["--tools", ""])
        .arg("--allowedTools")
        .arg(format!("{TOOL_PREFIX}*"))
        // The user's hooks and project instructions are for their coding, not for this.
        .args(["--setting-sources", ""])
        .args(["--append-system-prompt", INSTRUCTIONS]);
    if let Some(session) = session {
        command.args(["--resume", session]);
    }
    let mut child = command
        // The server inherits our environment, and writes are never on here.
        .env_remove(ALLOW_WRITES_ENV)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {AGENT_NAME}"))?;

    // The prompt goes in by stdin, so one that starts with a dash is never read as a flag.
    // An agent that has already stopped can't take it; its own error says why, below.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    // Read on the side, so a chatty stderr can't fill its pipe and stall the agent.
    let mut stderr = child.stderr.take().context("no stderr")?;
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut session = session.map(str::to_owned);
    let mut failure = None;
    let output = BufReader::new(child.stdout.take().context("no stdout")?);
    for line in output.lines().map_while(Result::ok) {
        let line = parse_line(&line);
        line.events.into_iter().for_each(&mut on_event);
        session = line.session.or(session);
        failure = line.error.or(failure);
    }
    let status = child.wait()?;
    let errors = errors.join().unwrap_or_default();
    if let Some(failure) = failure {
        bail!(failure);
    }
    if !status.success() {
        let errors = errors.trim();
        bail!(if errors.is_empty() {
            format!("{AGENT_NAME} stopped: {status}")
        } else {
            errors.to_owned()
        });
    }
    Ok(session)
}

fn mcp_config(server: &Path, config_dir: &Path) -> String {
    json!({
        "mcpServers": {
            SERVER_NAME: {
                "type": "stdio",
                "command": server,
                // The same config as this app, even when it is not the default one.
                "env": { CONFIG_DIR_ENV: config_dir },
            }
        }
    })
    .to_string()
}

/// An app started from the desktop doesn't get the shell's `PATH`, so the usual install
/// folders are tried too.
fn find_agent() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(base) = directories::BaseDirs::new() {
        let home = base.home_dir();
        dirs.extend([
            home.join(".local/bin"),
            home.join(".claude/local"),
            home.join(".npm-global/bin"),
        ]);
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd"]
    } else {
        &["claude"]
    };
    find_in(&dirs, names)
}

fn find_in(dirs: &[PathBuf], names: &[&str]) -> Option<PathBuf> {
    dirs.iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
}

/// Lines we don't know are skipped: the agent adds new kinds over time.
fn parse_line(line: &str) -> Line {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Line::default();
    };
    let mut parsed = Line {
        session: value["session_id"].as_str().map(str::to_owned),
        ..Line::default()
    };
    let blocks = || value["message"]["content"].as_array().into_iter().flatten();
    match value["type"].as_str() {
        Some("assistant") => {
            for block in blocks() {
                match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or_default().trim();
                        if !text.is_empty() {
                            parsed.events.push(Event::Text(text.to_owned()));
                        }
                    }
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or_default();
                        parsed.events.push(Event::Tool {
                            name: name.strip_prefix(TOOL_PREFIX).unwrap_or(name).to_owned(),
                            input: tool_input(&block["input"]),
                        });
                    }
                    _ => {}
                }
            }
        }
        Some("user") => {
            for block in blocks() {
                if block["type"] == "tool_result" && block["is_error"] == true {
                    parsed
                        .events
                        .push(Event::ToolFailed(result_text(&block["content"])));
                }
            }
        }
        Some("result") if value["is_error"] == true => {
            let text = value["result"].as_str().unwrap_or_default().trim();
            parsed.error = Some(if text.is_empty() {
                format!("{AGENT_NAME} could not answer.")
            } else {
                text.to_owned()
            });
        }
        _ => {}
    }
    parsed
}

fn tool_input(input: &Value) -> String {
    match input.as_object() {
        Some(fields) if !fields.is_empty() => {
            let text = input.to_string();
            if text.chars().count() > MAX_INPUT_CHARS {
                let cut: String = text.chars().take(MAX_INPUT_CHARS).collect();
                format!("{cut}…")
            } else {
                text
            }
        }
        _ => String::new(),
    }
}

/// A tool result is plain text or a list of text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_text_and_names_the_session() {
        let line = parse_line(
            r#"{"type":"assistant","session_id":"s1","message":{"content":[{"type":"text","text":" Two connections.\n"}]}}"#,
        );
        assert_eq!(line.events, [Event::Text("Two connections.".into())]);
        assert_eq!(line.session.as_deref(), Some("s1"));
    }

    #[test]
    fn a_tool_call_shows_the_tool_and_its_arguments() {
        let line = parse_line(
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","name":"mcp__tuplebase__list_connections","input":{}},
                {"type":"tool_use","name":"mcp__tuplebase__list_tables","input":{"connection":"db"}}]}}"#,
        );
        assert_eq!(
            line.events,
            [
                Event::Tool {
                    name: "list_connections".into(),
                    input: String::new()
                },
                Event::Tool {
                    name: "list_tables".into(),
                    input: r#"{"connection":"db"}"#.into()
                },
            ]
        );
    }

    #[test]
    fn long_tool_arguments_are_cut() {
        let input = tool_input(&json!({ "sql": "x".repeat(1000) }));
        assert_eq!(input.chars().count(), MAX_INPUT_CHARS + 1);
        assert!(input.ends_with('…'));
    }

    #[test]
    fn only_a_failed_tool_result_is_reported() {
        let result = |is_error: bool| {
            parse_line(
                &json!({"type": "user", "message": {"content": [{
                    "type": "tool_result",
                    "is_error": is_error,
                    "content": [{"type": "text", "text": "no connection named \"x\""}],
                }]}})
                .to_string(),
            )
            .events
        };
        assert!(result(false).is_empty());
        assert_eq!(
            result(true),
            [Event::ToolFailed("no connection named \"x\"".into())]
        );
    }

    #[test]
    fn a_failed_turn_carries_the_agents_reason() {
        let failed = parse_line(r#"{"type":"result","is_error":true,"result":"Not logged in"}"#);
        assert_eq!(failed.error.as_deref(), Some("Not logged in"));
        let fine = parse_line(r#"{"type":"result","is_error":false,"result":"Done"}"#);
        assert_eq!(fine.error, None);
    }

    #[test]
    fn unknown_lines_are_skipped() {
        assert_eq!(parse_line("not json"), Line::default());
        assert_eq!(
            parse_line(r#"{"type":"rate_limit_event"}"#),
            Line::default()
        );
    }

    #[test]
    fn the_agent_is_found_in_the_first_folder_that_has_it() {
        let dir = tempfile::tempdir().unwrap();
        let (empty, full) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("claude"), "").unwrap();
        assert_eq!(
            find_in(&[empty.clone(), full.clone()], &["claude"]),
            Some(full.join("claude"))
        );
        assert_eq!(find_in(&[empty], &["claude"]), None);
    }

    #[test]
    fn the_agent_gets_only_our_server_on_this_apps_config() {
        let config: Value =
            serde_json::from_str(&mcp_config(Path::new("/bin/server"), Path::new("/cfg"))).unwrap();
        let servers = config["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[SERVER_NAME]["command"], "/bin/server");
        assert_eq!(servers[SERVER_NAME]["env"][CONFIG_DIR_ENV], "/cfg");
    }
}
