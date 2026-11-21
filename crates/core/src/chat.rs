//! Chat with the user's own AI agent about their databases.
//!
//! The agent is the one the user already has and is signed in to. Each message runs it
//! once, headless, with our MCP server as its only tool, so it can do here exactly what
//! the server allows and nothing else on this computer. Only Claude Code for now.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

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

/// Stops one turn's agent, and the MCP server it started, from any thread: when the chat
/// is cleared, another message is sent, or the app closes. Never blocks.
#[derive(Clone, Default)]
pub struct Stop(Arc<Mutex<Running>>);

#[derive(Default)]
struct Running {
    stopped: bool,
    agent: Option<Child>,
}

impl Stop {
    pub fn stop(&self) {
        let mut running = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        running.stopped = true;
        if let Some(agent) = running.agent.as_mut() {
            kill(agent);
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopped
    }

    /// Keeps the agent here while it runs, so `stop` can reach it; a turn stopped before
    /// the agent started kills it at once.
    fn hold(&self, mut agent: Child) {
        let mut running = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if running.stopped {
            kill(&mut agent);
        }
        running.agent = Some(agent);
    }

    fn release(&self) -> Option<Child> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .agent
            .take()
    }
}

/// Doesn't wait: the reader sees the agent's output end and reaps it.
fn kill(agent: &mut Child) {
    // On Windows the agent is often a batch file: cmd.exe running node, which runs the
    // MCP server. Killing cmd.exe alone would leave the others running, so the whole tree
    // goes. `taskkill` finds it by parent, so cmd.exe must still be alive when it looks.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let tree = Command::new("taskkill")
            .args(["/T", "/F", "/PID"])
            .arg(agent.id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        if tree.is_ok() {
            return;
        }
    }
    // The agent's MCP server reads its requests from the agent, so it ends with it.
    let _ = agent.kill();
}

/// The app is a GUI program on Windows: without this every agent run opens a console.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Ends the agents with the app, also when the app crashes or is killed and so never
/// stops them itself. Every agent goes in one Windows job that kills what is in it when
/// it closes. The app never closes it: Windows does when the app's process ends. What
/// the agent starts afterwards (node, the MCP server) is in the job too.
#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Child;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// Puts `agent` in the app's job. Best effort: without it, stopping a turn still
    /// kills the agent's tree (`kill`), only a crash of the app may leave it running.
    pub(super) fn add(agent: &Child) {
        // A handle isn't `Send`; the job's handle is kept as a number for the app's life.
        static JOB: OnceLock<Option<usize>> = OnceLock::new();
        let Some(job) = *JOB.get_or_init(|| create().map(|job| job as usize)) else {
            return;
        };
        // SAFETY: both handles are open: the job's is never closed, and `agent` owns its
        // process handle for as long as the borrow lasts.
        unsafe { AssignProcessToJobObject(job as HANDLE, agent.as_raw_handle() as HANDLE) };
    }

    /// A new job that kills its processes when its last handle closes.
    pub(super) fn create() -> Option<HANDLE> {
        // SAFETY: plain calls with valid arguments; the handle is closed on failure.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 {
                CloseHandle(job);
                return None;
            }
            Some(job)
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::windows::io::AsRawHandle as _;
        use std::process::Command;
        use std::time::{Duration, Instant};

        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        #[test]
        fn closing_the_job_ends_the_agent_in_it() {
            let job = super::create().expect("no job");
            let mut agent = Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            // SAFETY: both handles are open here.
            let assigned =
                unsafe { AssignProcessToJobObject(job, agent.as_raw_handle() as HANDLE) };
            assert_ne!(assigned, 0, "the agent was not put in the job");
            assert!(agent.try_wait().unwrap().is_none());
            // What Windows does for the app when its process ends.
            // SAFETY: the job's handle is open and not used again.
            unsafe { CloseHandle(job) };
            let start = Instant::now();
            while agent.try_wait().unwrap().is_none() {
                if start.elapsed() > Duration::from_secs(10) {
                    let _ = agent.kill();
                    panic!("the agent outlived its job");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Sends `prompt` and reports what the agent does until it has answered. `session`
/// continues an earlier conversation; the result is the session to pass next time.
/// Blocks for the whole answer, or until `stop`; call it off the UI thread.
pub fn ask(
    prompt: &str,
    session: Option<&str>,
    stop: &Stop,
    on_event: impl FnMut(Event),
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
    command
        // The server inherits our environment, and writes are never on here.
        .env_remove(ALLOW_WRITES_ENV)
        .current_dir(std::env::temp_dir());
    run(command, prompt, session, stop, on_event)
}

/// Runs the prepared agent `command` for one turn.
fn run(
    mut command: Command,
    prompt: &str,
    session: Option<&str>,
    stop: &Stop,
    mut on_event: impl FnMut(Event),
) -> Result<Option<String>> {
    if stop.is_stopped() {
        bail!("stopped");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {AGENT_NAME}"))?;
    #[cfg(windows)]
    job::add(&child);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("no stdout")?;
    let mut stderr = child.stderr.take().context("no stderr")?;
    stop.hold(child);

    // The prompt goes in by stdin, so one that starts with a dash is never read as a flag.
    // An agent that has already stopped can't take it; its own error says why, below.
    if let Some(mut stdin) = stdin {
        let _ = stdin.write_all(prompt.as_bytes());
    }
    // Read on the side, so a chatty stderr can't fill its pipe and stall the agent.
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let mut session = session.map(str::to_owned);
    let mut failure = None;
    // Ends when the agent does, also when it is stopped.
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let line = parse_line(&line);
        line.events.into_iter().for_each(&mut on_event);
        session = line.session.or(session);
        failure = line.error.or(failure);
    }
    let status = stop.release().context("the agent was lost")?.wait()?;
    let errors = errors.join().unwrap_or_default();
    if stop.is_stopped() {
        bail!("stopped");
    }
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

    /// Runs `agent` on another thread: its result, or `None` if it hangs.
    fn run_agent(agent: Command, stop: &Stop) -> Option<Result<Option<String>>> {
        let stop = stop.clone();
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(run(agent, "hi", None, &stop, |_| {}));
        });
        result.recv_timeout(std::time::Duration::from_secs(10)).ok()
    }

    /// An agent that takes long to answer. On Windows, like `claude.cmd`, a child of
    /// cmd.exe holds the output, so killing cmd.exe alone would leave the turn hanging.
    fn slow_agent() -> Command {
        if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", "ping -n 30 127.0.0.1"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "exec sleep 30"]);
            command
        }
    }

    #[test]
    fn stopping_a_turn_kills_its_agent() {
        let stop = Stop::default();
        let stopper = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            stopper.stop();
        });
        let result = run_agent(slow_agent(), &stop).expect("the agent kept running");
        assert!(result.is_err());
        assert!(stop.release().is_none());
    }

    #[test]
    fn a_turn_stopped_before_its_agent_starts_never_runs_it() {
        let stop = Stop::default();
        stop.stop();
        let result = run_agent(slow_agent(), &stop).expect("the agent ran");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_agent_that_answers_names_the_session() {
        let line = r#"{"type":"assistant","session_id":"s9","message":{"content":[]}}"#;
        let mut agent = Command::new("sh");
        agent.args(["-c", &format!("cat >/dev/null; echo '{line}'")]);
        let result = run_agent(agent, &Stop::default()).expect("the agent hung");
        assert_eq!(result.unwrap().as_deref(), Some("s9"));
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
