//! The row detail panel: one result row with its fields listed top to bottom.
//!
//! It reads like JSON (braces, `name: value`, quoted text) without being JSON: nothing
//! is escaped, so a value shows exactly as the database returned it. The text sits in a
//! read-only editor, so any part of it can be selected and copied; a button copies the
//! whole row as real JSON. Keys and values take the SQL editor's syntax colors.
//!
//! A document from a document engine, such as MongoDB, shows as itself instead: pretty
//! JSON with nested indentation, highlighted as JSON, each object and array folding from
//! the gutter. "Copy as JSON" then copies the document. The panel also shows a document
//! engine's index or view definition, as text to read.

use std::ops::Range;

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Editor, EditorState, TextDecoration, TextDecorationCollection};
use gpui_kit::component::*;
use gpui_kit::*;
use serde_json::Value;

use crate::results::row_json;

pub enum RowDetailEvent {
    Close,
}

/// What the panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// A result row, field by field.
    Row,
    /// A document, as JSON that folds.
    Document,
    /// A definition, to read.
    Definition,
}

pub struct RowDetail {
    text: Entity<EditorState>,
    /// The colors over `text`, one range per key and value.
    colors: TextDecorationCollection,
    tokens: Vec<(Range<usize>, Token)>,
    /// What the copy button copies: the row or document as JSON, or the definition.
    /// `None` while nothing is chosen, which keeps the panel closed.
    json: Option<SharedString>,
    shown: Shown,
    title: SharedString,
}

impl EventEmitter<RowDetailEvent> for RowDetail {}

impl RowDetail {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| {
            // No grammar: the panel isn't strict JSON, so it is colored from the ranges
            // `row_text` reports instead.
            EditorState::new(window, cx)
                .folding(false)
                .line_number(false)
                .soft_wrap(true)
                .searchable(true)
        });
        let colors = text.update(cx, |state, cx| {
            state.create_decorations_collection(Vec::new(), cx)
        });
        // Light and dark themes color differently.
        cx.observe_global::<Theme>(|this, cx| this.paint(cx))
            .detach();
        Self {
            text,
            colors,
            tokens: Vec::new(),
            json: None,
            shown: Shown::Row,
            title: "Row".into(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.json.is_some()
    }

    pub fn show(&mut self, row: Vec<(String, Value)>, window: &mut Window, cx: &mut Context<Self>) {
        self.json = Some(row_json(&row).into());
        let (text, tokens) = row_text(&row);
        self.set_mode(Shown::Row, "Row", window, cx);
        self.text
            .update(cx, |state, cx| state.set_value(text, window, cx));
        self.tokens = tokens;
        self.paint(cx);
        cx.notify();
    }

    /// Shows a document as pretty JSON, each nested object or array folding.
    pub fn show_document(&mut self, document: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let text = document_text(document);
        self.json = Some(text.clone().into());
        self.show_plain(Shown::Document, "Document".into(), text, window, cx);
    }

    /// Shows a definition to read, such as a document engine's index, under `title`.
    pub fn show_definition(
        &mut self,
        title: String,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.json = Some(text.clone().into());
        self.show_plain(Shown::Definition, title.into(), text, window, cx);
    }

    fn show_plain(
        &mut self,
        shown: Shown,
        title: SharedString,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_mode(shown, title, window, cx);
        self.text
            .update(cx, |state, cx| state.set_value(text, window, cx));
        // The grammar colors it; the row's own colors would land on the wrong text.
        self.tokens.clear();
        self.paint(cx);
        cx.notify();
    }

    /// Sets the grammar and folding for what is shown, when it changes.
    fn set_mode(
        &mut self,
        shown: Shown,
        title: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.title = title.into();
        if self.shown == shown {
            return;
        }
        self.shown = shown;
        let (language, folding) = match shown {
            Shown::Row => ("text", false),
            Shown::Document => ("json", true),
            Shown::Definition => ("javascript", false),
        };
        self.text.update(cx, |state, cx| {
            state.set_highlighter(language, cx);
            state.set_folding(folding, window, cx);
        });
    }

    fn paint(&self, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let syntax = |name: &str| theme.highlight_theme.style(name).unwrap_or_default();
        let muted = HighlightStyle {
            color: Some(theme.muted_foreground),
            ..Default::default()
        };
        let style = |token| match token {
            Token::Key => syntax("property"),
            Token::Punctuation => muted,
            Token::Text => syntax("string"),
            Token::Number => syntax("number"),
            Token::Boolean => syntax("boolean"),
            Token::Null => syntax("comment"),
        };
        let decorations = self
            .tokens
            .iter()
            .map(|(range, token)| TextDecoration::new(range.clone(), style(*token)))
            .collect();
        self.colors.set(decorations, cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.json.take().is_some() {
            cx.notify();
        }
    }
}

/// What a stretch of the panel's text is, for its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Key,
    Punctuation,
    Text,
    Number,
    Boolean,
    Null,
}

/// Builds the panel's text and notes where each key and value lies in it.
#[derive(Default)]
struct Writer {
    text: String,
    tokens: Vec<(Range<usize>, Token)>,
}

impl Writer {
    fn push(&mut self, text: &str, token: Token) {
        let start = self.text.len();
        self.text.push_str(text);
        self.tokens.push((start..self.text.len(), token));
    }

    fn plain(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// A value as the panel shows it: text in quotes and not escaped, a JSON column
    /// spread over lines like JSON, starting at the current column of `indent` spaces.
    fn value(&mut self, value: &Value, indent: usize) {
        match value {
            Value::Null => self.push("null", Token::Null),
            Value::Bool(b) => self.push(&b.to_string(), Token::Boolean),
            Value::Number(n) => self.push(&n.to_string(), Token::Number),
            Value::String(text) => self.push(&format!("\"{text}\""), Token::Text),
            Value::Array(items) => {
                self.list(("[", "]"), items.iter().map(|item| (None, item)), indent)
            }
            Value::Object(fields) => self.list(
                ("{", "}"),
                fields.iter().map(|(key, value)| (Some(key), value)),
                indent,
            ),
        }
    }

    fn list<'a>(
        &mut self,
        (open, close): (&str, &str),
        items: impl ExactSizeIterator<Item = (Option<&'a String>, &'a Value)>,
        indent: usize,
    ) {
        let len = items.len();
        self.push(open, Token::Punctuation);
        if len == 0 {
            self.push(close, Token::Punctuation);
            return;
        }
        let inner = indent + 2;
        for (ix, (key, value)) in items.enumerate() {
            self.plain(&format!("\n{:inner$}", ""));
            if let Some(key) = key {
                self.push(&format!("\"{key}\""), Token::Key);
                self.push(":", Token::Punctuation);
                self.plain(" ");
            }
            self.value(value, inner);
            if ix + 1 < len {
                self.push(",", Token::Punctuation);
            }
        }
        self.plain(&format!("\n{:indent$}", ""));
        self.push(close, Token::Punctuation);
    }
}

/// A document as the panel shows it: JSON, two spaces per level.
fn document_text(document: &Value) -> String {
    serde_json::to_string_pretty(document).unwrap_or_default()
}

/// The row as the panel shows it, one field per line inside braces, with where each key
/// and value lies in the text.
fn row_text(row: &[(String, Value)]) -> (String, Vec<(Range<usize>, Token)>) {
    let mut out = Writer::default();
    out.push("{", Token::Punctuation);
    for (name, value) in row {
        out.plain("\n  ");
        out.push(name, Token::Key);
        out.push(":", Token::Punctuation);
        out.plain(" ");
        out.value(value, 2);
    }
    out.plain("\n");
    out.push("}", Token::Punctuation);
    (out.text, out.tokens)
}

impl Render for RowDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let json = self.json.clone().unwrap_or_default();
        let copy_tip = match self.shown {
            Shown::Definition => "Copy",
            Shown::Row | Shown::Document => "Copy as JSON",
        };
        v_flex()
            .id("row-detail")
            .test_support()
            .size_full()
            .border_l_1()
            .border_color(theme.border)
            .child(
                crate::theme::panel_header(self.title.clone(), cx)
                    .child(
                        Button::new("row-detail-copy")
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip(copy_tip)
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(json.to_string()));
                            }),
                    )
                    .child(
                        Button::new("row-detail-close")
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(RowDetailEvent::Close))),
                    ),
            )
            .child(
                div().flex_1().min_h_0().p_1().child(
                    Editor::new(&self.text)
                        .readonly(true)
                        .appearance(false)
                        .h_full(),
                ),
            )
    }
}

#[cfg(test)]
impl RowDetail {
    pub(crate) fn shown(&self, cx: &App) -> String {
        self.text.read(cx).value().to_string()
    }

    pub(crate) fn text(&self) -> &Entity<EditorState> {
        &self.text
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }
}

#[cfg(test)]
mod tests {
    // No glob: gpui_kit's `test` macro would shadow the built-in `#[test]`.
    use super::{Token, row_text};
    use serde_json::json;

    #[test]
    fn rows_read_like_json_without_escaping() {
        let row = [
            ("id".to_owned(), json!(7)),
            ("ok".to_owned(), json!(true)),
            ("note".to_owned(), json!("say \"hi\"")),
            ("gone".to_owned(), json!(null)),
            ("meta".to_owned(), json!({"a": [1, "x"], "b": {}})),
        ];
        assert_eq!(
            row_text(&row).0,
            "{\n  id: 7\n  ok: true\n  note: \"say \"hi\"\"\n  gone: null\n  \
             meta: {\n    \"a\": [\n      1,\n      \"x\"\n    ],\n    \"b\": {}\n  }\n}"
        );
    }

    #[test]
    fn every_key_and_value_is_marked_for_its_color() {
        let row = [
            ("id".to_owned(), json!(7)),
            ("ok".to_owned(), json!(false)),
            ("gone".to_owned(), json!(null)),
            ("meta".to_owned(), json!({"a": "x"})),
        ];
        let (text, tokens) = row_text(&row);
        let marked: Vec<(&str, Token)> = tokens
            .iter()
            .filter(|(_, token)| *token != Token::Punctuation)
            .map(|(range, token)| (&text[range.clone()], *token))
            .collect();
        assert_eq!(
            marked,
            [
                ("id", Token::Key),
                ("7", Token::Number),
                ("ok", Token::Key),
                ("false", Token::Boolean),
                ("gone", Token::Key),
                ("null", Token::Null),
                ("meta", Token::Key),
                ("\"a\"", Token::Key),
                ("\"x\"", Token::Text),
            ]
        );
    }
}
