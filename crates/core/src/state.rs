//! Working state kept between launches: unsaved SQL, query history, panel sizes and the
//! open tabs.
//!
//! Stored next to the connections file. It never holds passwords, but query text can
//! contain any value someone typed into it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{self, write_atomic};

pub const STATE_FILE: &str = "state.json";
const HISTORY_LIMIT: usize = 500;
const STALE_SIDEBAR_PANEL: &str = "sidebar";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub connection: String,
    pub sql: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// The SQL last left in the editor, per connection.
    #[serde(default)]
    pub drafts: BTreeMap<String, String>,
    /// Oldest first.
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    /// Sizes the user dragged panels to, in whole pixels, by panel name.
    #[serde(default)]
    pub panels: BTreeMap<String, u32>,
    /// Where the last search for databases in AWS looked, to look there again.
    #[serde(default, skip_serializing_if = "CloudSearch::is_empty")]
    pub aws_search: CloudSearch,
    /// The connections open in tabs, in the order the tabs show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_tabs: Vec<String>,
    /// The tab that was on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<String>,
}

/// A search for databases in a cloud account: the profile and regions it used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudSearch {
    /// Empty means the default credentials.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
}

impl CloudSearch {
    pub fn is_empty(&self) -> bool {
        self.profile.is_empty() && self.regions.is_empty()
    }
}

impl SessionState {
    /// Loads the saved state. A missing or damaged file just means starting fresh.
    pub fn load() -> Self {
        config::config_dir()
            .map(|dir| Self::load_from(&dir.join(STATE_FILE)))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config::config_dir()?.join(STATE_FILE))
    }

    pub fn load_from(path: &Path) -> Self {
        let mut state: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        // The width of the sidebar the tabs replaced.
        state.panels.remove(STALE_SIDEBAR_PANEL);
        state
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_string_pretty(self)?)
    }

    /// Remembers a query as the newest for its connection, without duplicates.
    pub fn record(&mut self, connection: &str, sql: &str) {
        let sql = sql.trim();
        if sql.is_empty() {
            return;
        }
        self.history
            .retain(|e| !(e.connection == connection && e.sql == sql));
        self.history.push(HistoryEntry {
            connection: connection.to_owned(),
            sql: sql.to_owned(),
        });
        let excess = self.history.len().saturating_sub(HISTORY_LIMIT);
        self.history.drain(..excess);
    }

    /// A connection's queries, newest first.
    pub fn history_for<'a>(&'a self, connection: &'a str) -> impl Iterator<Item = &'a str> {
        self.history
            .iter()
            .rev()
            .filter(move |e| e.connection == connection)
            .map(|e| e.sql.as_str())
    }

    /// The connections queries last ran on, newest first, at most `n` of them.
    pub fn recent_connections(&self, n: usize) -> Vec<&str> {
        let mut recent: Vec<&str> = Vec::new();
        for entry in self.history.iter().rev() {
            if recent.len() == n {
                break;
            }
            if !recent.contains(&entry.connection.as_str()) {
                recent.push(&entry.connection);
            }
        }
        recent
    }

    /// Remembers what's in the editor; an empty editor needs no draft.
    pub fn set_draft(&mut self, connection: &str, sql: &str) {
        if sql.trim().is_empty() {
            self.drafts.remove(connection);
        } else {
            self.drafts.insert(connection.to_owned(), sql.to_owned());
        }
    }

    pub fn rename_connection(&mut self, old: &str, new: &str) {
        if let Some(draft) = self.drafts.remove(old) {
            self.drafts.insert(new.to_owned(), draft);
        }
        for entry in self.history.iter_mut().filter(|e| e.connection == old) {
            entry.connection = new.to_owned();
        }
        for tab in self.open_tabs.iter_mut().filter(|tab| *tab == old) {
            *tab = new.to_owned();
        }
        if self.active_tab.as_deref() == Some(old) {
            self.active_tab = Some(new.to_owned());
        }
    }

    pub fn remove_connection(&mut self, name: &str) {
        self.drafts.remove(name);
        self.history.retain(|e| e.connection != name);
        self.open_tabs.retain(|tab| tab != name);
        if self.active_tab.as_deref() == Some(name) {
            self.active_tab = None;
        }
    }

    /// Remembers the open tabs, in order, and the one on screen.
    pub fn set_tabs(&mut self, open: Vec<String>, active: Option<String>) {
        self.open_tabs = open;
        self.active_tab = active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_newest_first_per_connection() {
        let mut state = SessionState::default();
        state.record("a", "one");
        state.record("b", "other");
        state.record("a", "two");
        assert_eq!(state.history_for("a").collect::<Vec<_>>(), ["two", "one"]);
    }

    #[test]
    fn rerunning_a_query_moves_it_to_the_top() {
        let mut state = SessionState::default();
        state.record("a", "one");
        state.record("a", "two");
        state.record("a", " one ");
        assert_eq!(state.history_for("a").collect::<Vec<_>>(), ["one", "two"]);
    }

    #[test]
    fn recent_connections_are_newest_first_once_each() {
        let mut state = SessionState::default();
        assert!(state.recent_connections(5).is_empty());
        state.record("a", "one");
        state.record("b", "one");
        state.record("a", "two");
        state.record("c", "one");
        assert_eq!(state.recent_connections(5), ["c", "a", "b"]);
        assert_eq!(state.recent_connections(2), ["c", "a"]);
        assert!(state.recent_connections(0).is_empty());
        state.remove_connection("a");
        assert_eq!(state.recent_connections(5), ["c", "b"]);
    }

    #[test]
    fn history_is_capped() {
        let mut state = SessionState::default();
        for i in 0..HISTORY_LIMIT + 5 {
            state.record("a", &format!("q{i}"));
        }
        assert_eq!(state.history.len(), HISTORY_LIMIT);
        assert_eq!(state.history_for("a").next(), Some("q504"));
    }

    #[test]
    fn empty_sql_is_not_recorded_and_clears_the_draft() {
        let mut state = SessionState::default();
        state.record("a", "  ");
        assert!(state.history.is_empty());
        state.set_draft("a", "select 1");
        state.set_draft("a", "  ");
        assert!(state.drafts.is_empty());
    }

    #[test]
    fn renaming_and_removing_a_connection_follow_its_state() {
        let mut state = SessionState::default();
        state.record("a", "one");
        state.set_draft("a", "draft");
        state.set_tabs(vec!["x".into(), "a".into()], Some("a".into()));
        state.rename_connection("a", "b");
        assert_eq!(state.drafts["b"], "draft");
        assert_eq!(state.history_for("b").count(), 1);
        assert_eq!(state.open_tabs, ["x", "b"]);
        assert_eq!(state.active_tab.as_deref(), Some("b"));
        state.remove_connection("b");
        assert_eq!(state.open_tabs, ["x"]);
        assert_eq!(state.active_tab, None);
        state.remove_connection("x");
        assert_eq!(state, SessionState::default());
    }

    #[test]
    fn save_and_load_round_trip_and_damage_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let mut state = SessionState::default();
        state.set_draft("a", "select 1");
        state.panels.insert("chat".into(), 340);
        state.set_tabs(vec!["b".into(), "a".into()], Some("a".into()));
        state.aws_search = CloudSearch {
            profile: "prod".into(),
            regions: vec!["eu-west-1".into(), "us-east-1".into()],
        };
        state.save_to(&path).unwrap();
        assert_eq!(SessionState::load_from(&path), state);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(SessionState::load_from(&path), SessionState::default());
    }

    #[test]
    fn no_search_writes_nothing_and_older_files_load() {
        let text = serde_json::to_string(&SessionState::default()).unwrap();
        assert!(!text.contains("aws_search"), "{text}");
        let old: SessionState = serde_json::from_str(r#"{"drafts":{}}"#).unwrap();
        assert!(old.aws_search.is_empty());
    }

    #[test]
    fn no_tabs_write_nothing_and_a_file_from_before_tabs_loads() {
        let text = serde_json::to_string(&SessionState::default()).unwrap();
        assert!(
            !text.contains("open_tabs") && !text.contains("active_tab"),
            "{text}"
        );
        // As written before tabs were remembered, with the old sidebar's size in it.
        let old = r#"{"drafts":{"a":"select 1"},"history":[],"panels":{"sidebar":300}}"#;
        let old: SessionState = serde_json::from_str(old).unwrap();
        assert_eq!(old.drafts["a"], "select 1");
        assert!(old.open_tabs.is_empty());
        assert_eq!(old.active_tab, None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        std::fs::write(&path, r#"{"panels":{"sidebar":300,"chat":420}}"#).unwrap();
        let loaded = SessionState::load_from(&path);
        assert_eq!(loaded.panels.keys().collect::<Vec<_>>(), ["chat"]);
    }
}
