//! SQL autocomplete: keywords, plus the table and column names of the connection.
//!
//! The menu opens by itself once a word has [`MIN_TYPED`] characters, or at once with
//! Ctrl+Space. Tab takes the highlighted suggestion; Enter always starts a new line, and
//! Escape closes the menu.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, EditorState, Enter, ShowCompletions};
use gpui_kit::{
    App, Context, Div, Entity, InteractiveElement as _, KeyBinding, Stateful, Task, Window, actions,
};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse,
    CompletionTriggerKind,
};
use ropey::Rope;

const MAX_SUGGESTIONS: usize = 50;

/// Characters of a word typed before the menu opens by itself, so it doesn't pop up at
/// every letter.
const MIN_TYPED: usize = 2;

/// Key context of a code editor, so other inputs keep their own Tab and Enter.
const EDITOR_CONTEXT: &str = "Input && mode == editor";

actions!(completions, [AcceptCompletion, NewLine]);

/// Call after `gpui_kit::init`: these override the editor's own bindings, where Enter
/// takes the suggestion and Tab only indents.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("tab", AcceptCompletion, Some(EDITOR_CONTEXT)),
        KeyBinding::new("enter", NewLine, Some(EDITOR_CONTEXT)),
        KeyBinding::new("shift-enter", NewLine, Some(EDITOR_CONTEXT)),
        KeyBinding::new("ctrl-space", ShowCompletions, Some(EDITOR_CONTEXT)),
    ]
}

/// Handles [`key_bindings`] for `editor`, placed on an element around it.
pub fn handle_keys(
    element: Stateful<Div>,
    editor: &Entity<EditorState>,
    window: &Window,
) -> Stateful<Div> {
    element
        .on_action(window.listener_for(editor, accept))
        .on_action(window.listener_for(editor, new_line))
}

/// Takes the highlighted suggestion when the menu is open, and indents otherwise.
fn accept(
    editor: &mut EditorState,
    _: &AcceptCompletion,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    // The menu takes a suggestion on Enter; it isn't told which key was pressed.
    let enter = Enter {
        secondary: false,
        shift: false,
    };
    if editor.route_overlay_action(Box::new(enter), window, cx) {
        // The menu lets the action go on; stop it before the next binding, Tab's indent.
        cx.stop_propagation();
    } else {
        cx.propagate();
    }
}

/// Closes the menu, so that the editor's own Enter, next, starts a new line.
fn new_line(editor: &mut EditorState, _: &NewLine, _: &mut Window, cx: &mut Context<EditorState>) {
    editor.dismiss_completion_overlay(cx);
    cx.propagate();
}

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
    names: SchemaNames,
    /// Where the word started when the menu was asked for with Ctrl+Space: while that
    /// word is typed, the menu stays open even under [`MIN_TYPED`] characters.
    invoked_at: Cell<Option<usize>>,
}

impl SqlCompletions {
    pub fn new(names: SchemaNames) -> Self {
        Self {
            names,
            invoked_at: Cell::new(None),
        }
    }

    /// The fewest characters the word at the cursor needs for a menu.
    fn min_typed(&self, word_start: usize, trigger: &CompletionContext) -> usize {
        if trigger.trigger_kind == CompletionTriggerKind::INVOKED {
            self.invoked_at.set(Some(word_start));
        } else if self.invoked_at.get() != Some(word_start) {
            self.invoked_at.set(None);
        }
        if self.invoked_at.get().is_some() {
            0
        } else {
            MIN_TYPED
        }
    }
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

/// Suggestions for `prefix`: names from the database first, then keywords. An empty
/// prefix gets the first of all of them.
fn suggestions(prefix: &str, names: &[String]) -> Vec<(String, CompletionItemKind)> {
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
        trigger: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let text = text.to_string();
        let before = &text[..offset.min(text.len())];
        let prefix = word_before(before);
        let min_typed = self.min_typed(before.len() - prefix.len(), &trigger);
        if prefix.chars().count() < min_typed {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        }
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
    use super::{
        CompletionContext, CompletionTriggerKind, MAX_SUGGESTIONS, MIN_TYPED, SchemaNames,
        SqlCompletions, suggestions, word_before,
    };

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
    fn an_empty_word_gets_names_then_keywords() {
        let all = labels("", &["users"]);
        assert_eq!(all.len(), MAX_SUGGESTIONS);
        assert_eq!(all[..2], ["users", "ADD"]);
    }

    fn trigger(kind: CompletionTriggerKind) -> CompletionContext {
        CompletionContext {
            trigger_kind: kind,
            trigger_character: None,
        }
    }

    #[test]
    fn the_menu_waits_for_two_letters_unless_asked_for() {
        let completions = SqlCompletions::new(SchemaNames::default());
        let typed = trigger(CompletionTriggerKind::TRIGGER_CHARACTER);
        let invoked = trigger(CompletionTriggerKind::INVOKED);
        assert_eq!(completions.min_typed(7, &typed), MIN_TYPED);
        // Ctrl+Space, then typing the same word: the menu stays.
        assert_eq!(completions.min_typed(7, &invoked), 0);
        assert_eq!(completions.min_typed(7, &typed), 0);
        // A new word waits again, and so does the old one after it.
        assert_eq!(completions.min_typed(12, &typed), MIN_TYPED);
        assert_eq!(completions.min_typed(7, &typed), MIN_TYPED);
    }
}
