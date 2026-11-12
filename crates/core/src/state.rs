//! Working state kept between launches: unsaved SQL, query history and panel sizes.
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
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
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
    }

    pub fn remove_connection(&mut self, name: &str) {
        self.drafts.remove(name);
        self.history.retain(|e| e.connection != name);
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
        state.rename_connection("a", "b");
        assert_eq!(state.drafts["b"], "draft");
        assert_eq!(state.history_for("b").count(), 1);
        state.remove_connection("b");
        assert_eq!(state, SessionState::default());
    }

    #[test]
    fn save_and_load_round_trip_and_damage_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let mut state = SessionState::default();
        state.set_draft("a", "select 1");
        state.panels.insert("sidebar".into(), 340);
        state.save_to(&path).unwrap();
        assert_eq!(SessionState::load_from(&path), state);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(SessionState::load_from(&path), SessionState::default());
    }
}
