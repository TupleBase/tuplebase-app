//! SQL autocomplete: keywords, plus the table and column names of the connection.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::input::CompletionProvider;
use gpui_kit::{App, Task, Window};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse};
use ropey::Rope;

const MAX_SUGGESTIONS: usize = 50;

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "COMMIT",
    "COUNT",
    "CREATE",
    "CROSS",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXISTS",
    "EXPLAIN",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "THEN",
    "TRANSACTION",
    "UNION",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

/// Table and column names of the current connection, updated by the schema panel.
pub type SchemaNames = Rc<RefCell<Vec<String>>>;

pub struct SqlCompletions {
    pub names: SchemaNames,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The partly typed word that ends at the cursor.
fn word_before(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    &text[start..]
}

/// Suggestions for `prefix`: names from the database first, then keywords.
fn suggestions(prefix: &str, names: &[String]) -> Vec<(String, CompletionItemKind)> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let prefix = prefix.to_lowercase();
    let starts = |candidate: &str| candidate.to_lowercase().starts_with(&prefix);
    names
        .iter()
        .filter(|name| starts(name))
        .map(|name| (name.clone(), CompletionItemKind::FIELD))
        .chain(
            KEYWORDS
                .iter()
                .filter(|keyword| starts(keyword))
                .map(|keyword| ((*keyword).to_owned(), CompletionItemKind::KEYWORD)),
        )
        .take(MAX_SUGGESTIONS)
        .collect()
}

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let text = text.to_string();
        let prefix = word_before(&text[..offset.min(text.len())]);
        let items = suggestions(prefix, &self.names.borrow())
            .into_iter()
            .map(|(label, kind)| CompletionItem {
                label,
                kind: Some(kind),
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text.chars().next_back().is_some_and(is_word_char)
    }
}

#[cfg(test)]
mod tests {
    use super::{suggestions, word_before};

    fn labels(prefix: &str, names: &[&str]) -> Vec<String> {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        suggestions(prefix, &names)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    #[test]
    fn the_word_at_the_cursor_is_used() {
        assert_eq!(word_before("select na"), "na");
        assert_eq!(word_before("users.em"), "em");
        assert_eq!(word_before("select "), "");
    }

    #[test]
    fn database_names_come_before_keywords_ignoring_case() {
        assert_eq!(labels("SEL", &["selections"]), ["selections", "SELECT"]);
    }

    #[test]
    fn nothing_is_suggested_for_an_empty_word() {
        assert!(labels("", &["users"]).is_empty());
    }
}
