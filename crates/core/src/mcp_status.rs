//! Whether agents are running the MCP server.
//!
//! `tuplebase-mcp` is a stdio process that each agent starts, so there is nothing to
//! connect to. Instead every server keeps a heartbeat file fresh next to the config and
//! the app counts the fresh ones. A server that crashes stops beating and ages out.
//! A running server puts its file back if the app removed it, say after the computer slept.
//! The file holds the name of the agent the server works for, once the agent has said it.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::config::config_dir;

const DIR: &str = "mcp-servers";
const BEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Allows for a few late beats on a busy machine.
const STALE_AFTER: Duration = Duration::from_secs(20);
/// Agents choose their own name, so the app shows only this much of it.
const MAX_NAME_CHARS: usize = 40;

/// Marks this process as a running server until dropped.
pub struct Heartbeat {
    path: PathBuf,
    /// Shared with the beating thread, so a beat never brings the file back after drop.
    state: Arc<Mutex<Beat>>,
}

#[derive(Default)]
struct Beat {
    stopped: bool,
    /// What the file holds, to write again if it was removed.
    client: String,
}

impl Heartbeat {
    pub fn start() -> Result<Self> {
        Self::start_in(&config_dir()?.join(DIR))
    }

    fn start_in(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(std::process::id().to_string());
        File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let heartbeat = Self {
            path,
            state: Arc::default(),
        };
        let (path, state) = (heartbeat.path.clone(), heartbeat.state.clone());
        std::thread::spawn(move || {
            // A failed beat is tried again: the folder may be back by the next one.
            while beat(&path, &state) {
                std::thread::sleep(BEAT_INTERVAL);
            }
        });
        Ok(heartbeat)
    }

    /// Records which agent this server works for, as the agent named itself.
    pub fn set_client(&self, name: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.client = clean_name(name);
        write(&self.path, &state.client).with_context(|| format!("writing {}", self.path.display()))
    }
}

/// Freshens the file, or writes it again if it is gone. False once the server stopped.
fn beat(path: &Path, state: &Mutex<Beat>) -> bool {
    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
    if state.stopped {
        return false;
    }
    if touch(path).is_err() {
        let _ = fs::create_dir_all(path.parent().unwrap_or(path));
        let _ = write(path, &state.client);
    }
    true
}

fn write(path: &Path, client: &str) -> io::Result<()> {
    File::create(path)?.write_all(client.as_bytes())
}

fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stopped = true;
        let _ = fs::remove_file(&self.path);
    }
}

/// Never creates the file; `beat` decides when it may come back.
fn touch(path: &Path) -> io::Result<()> {
    File::options()
        .write(true)
        .open(path)?
        .set_modified(SystemTime::now())
}

/// The running servers, one entry each: the agent's name, or `None` while it is unknown.
/// Sorted, so the list only changes when the servers do. Reads the disk; call it off the
/// UI thread.
pub fn running_servers() -> Result<Vec<Option<String>>> {
    fresh_servers(&config_dir()?.join(DIR), SystemTime::now())
}

fn fresh_servers(dir: &Path, now: SystemTime) -> Result<Vec<Option<String>>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut fresh = Vec::new();
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > STALE_AFTER)
        {
            let _ = fs::remove_file(entry.path());
        } else {
            let name = fs::read_to_string(entry.path()).unwrap_or_default();
            let name = clean_name(&name);
            fresh.push((!name.is_empty()).then_some(name));
        }
    }
    fresh.sort();
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_counts_until_it_stops() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = Heartbeat::start_in(dir.path()).unwrap();
        assert_eq!(
            fresh_servers(dir.path(), SystemTime::now()).unwrap(),
            [None]
        );
        drop(heartbeat);
        assert!(
            fresh_servers(dir.path(), SystemTime::now())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_server_is_listed_with_its_agents_name() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = Heartbeat::start_in(dir.path()).unwrap();
        heartbeat.set_client("claude-code\n").unwrap();
        assert_eq!(
            fresh_servers(dir.path(), SystemTime::now()).unwrap(),
            [Some("claude-code".to_owned())]
        );
    }

    #[test]
    fn a_long_agent_name_is_cut() {
        assert_eq!(clean_name(&"x".repeat(500)).len(), MAX_NAME_CHARS);
    }

    #[test]
    fn a_crashed_server_ages_out_and_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("12345");
        File::create(&file).unwrap();
        let later = SystemTime::now() + STALE_AFTER + Duration::from_secs(1);
        assert!(fresh_servers(dir.path(), later).unwrap().is_empty());
        assert!(!file.exists());
    }

    #[test]
    fn no_folder_means_no_servers() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(
            fresh_servers(&missing, SystemTime::now())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_running_server_comes_back_after_its_file_was_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = Heartbeat::start_in(dir.path()).unwrap();
        heartbeat.set_client("codex").unwrap();
        // As after a sleep: the app saw the file stale and removed it.
        let later = SystemTime::now() + STALE_AFTER + Duration::from_secs(1);
        assert!(fresh_servers(dir.path(), later).unwrap().is_empty());
        assert!(beat(&heartbeat.path, &heartbeat.state));
        assert_eq!(
            fresh_servers(dir.path(), SystemTime::now()).unwrap(),
            [Some("codex".to_owned())]
        );
    }

    #[test]
    fn a_beat_does_not_bring_back_a_stopped_server() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = Heartbeat::start_in(dir.path()).unwrap();
        let (path, state) = (heartbeat.path.clone(), heartbeat.state.clone());
        drop(heartbeat);
        assert!(!beat(&path, &state));
        assert!(!path.exists());
    }
}
