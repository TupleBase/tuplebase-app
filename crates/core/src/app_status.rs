//! Which connections the running app has open.
//!
//! The app and `tuplebase-mcp` are separate processes, so the app keeps a small file
//! fresh next to the config and the server reads it. An app that quits or crashes stops
//! writing, and the file ages out.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::config::{config_dir, write_atomic};

const FILE: &str = "open-connections.json";
/// The app writes every few seconds; this allows for a few late writes on a busy machine.
const STALE_AFTER: Duration = Duration::from_secs(20);

/// Records the connections open right now. The app calls this again and again, also when
/// nothing changed, to keep the file fresh. Writes the disk; call it off the UI thread.
pub fn publish(open: &HashSet<String>) -> Result<()> {
    publish_in(&config_dir()?, open)
}

fn publish_in(dir: &Path, open: &HashSet<String>) -> Result<()> {
    let mut names: Vec<&String> = open.iter().collect();
    names.sort();
    write_atomic(&dir.join(FILE), &serde_json::to_string(&names)?)
}

/// The connections open in the app, sorted, or `None` when the app is not running.
pub fn open_connections() -> Result<Option<Vec<String>>> {
    read_in(&config_dir()?, SystemTime::now())
}

fn read_in(dir: &Path, now: SystemTime) -> Result<Option<Vec<String>>> {
    let path = dir.join(FILE);
    let modified = match fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(modified) => modified,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if now
        .duration_since(modified)
        .is_ok_and(|age| age > STALE_AFTER)
    {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let names =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(names))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_connections_are_read_back_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let open = HashSet::from(["prod".to_owned(), "dev".to_owned()]);
        publish_in(dir.path(), &open).unwrap();
        assert_eq!(
            read_in(dir.path(), SystemTime::now()).unwrap(),
            Some(vec!["dev".to_owned(), "prod".to_owned()])
        );
    }

    #[test]
    fn a_running_app_with_nothing_open_is_not_a_stopped_app() {
        let dir = tempfile::tempdir().unwrap();
        publish_in(dir.path(), &HashSet::new()).unwrap();
        assert_eq!(
            read_in(dir.path(), SystemTime::now()).unwrap(),
            Some(Vec::new())
        );
    }

    #[test]
    fn an_app_that_stopped_writing_counts_as_not_running() {
        let dir = tempfile::tempdir().unwrap();
        publish_in(dir.path(), &HashSet::from(["dev".to_owned()])).unwrap();
        let later = SystemTime::now() + STALE_AFTER + Duration::from_secs(1);
        assert_eq!(read_in(dir.path(), later).unwrap(), None);
    }

    #[test]
    fn no_file_means_the_app_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_in(dir.path(), SystemTime::now()).unwrap(), None);
    }
}
