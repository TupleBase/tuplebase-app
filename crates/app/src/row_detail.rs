//! The row detail panel: one result row with its fields listed top to bottom.
//!
//! It reads like JSON (braces, `name: value`, quoted text) without being JSON: nothing
//! is escaped, so a value shows exactly as the database returned it. The text sits in a
//! read-only editor, so any part of it can be selected and copied; a button copies the
//! whole row as real JSON.

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::*;
use gpui_kit::*;
use serde_json::Value;

use crate::results::row_json;

pub enum RowDetailEvent {
    Close,
}

pub struct RowDetail {
    text: Entity<EditorState>,
    /// The row as JSON for "Copy as JSON". `None` while no row is chosen, which keeps
    /// the panel closed.
    json: Option<SharedString>,
}

impl EventEmitter<RowDetailEvent> for RowDetail {}

impl RowDetail {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| {
            // Plain text: the panel isn't strict JSON, so a JSON grammar colors it
            // unevenly.
            EditorState::new(window, cx)
                .folding(false)
                .line_number(false)
                .soft_wrap(true)
                .searchable(true)
        });
        Self { text, json: None }
    }

    pub fn is_open(&self) -> bool {
        self.json.is_some()
    }

    pub fn show(&mut self, row: Vec<(String, Value)>, window: &mut Window, cx: &mut Context<Self>) {
        self.json = Some(row_json(&row).into());
        let text = row_text(&row);
        self.text
            .update(cx, |state, cx| state.set_value(text, window, cx));
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.json.take().is_some() {
            cx.notify();
        }
    }
}

/// The row as the panel shows it: one field per line, inside braces. A JSON column
/// spreads over lines, indented under its field.
fn row_text(row: &[(String, Value)]) -> String {
    let mut text = String::from("{\n");
    for (name, value) in row {
        let value = value_text(value).replace('\n', "\n  ");
        text.push_str(&format!("  {name}: {value}\n"));
    }
    text.push('}');
    text
}

/// How a value reads in the panel: text in quotes, a JSON column spread over lines.
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(text) => format!("\"{text}\""),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        }
        other => other.to_string(),
    }
}

impl Render for RowDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let json = self.json.clone().unwrap_or_default();
        v_flex()
            .id("row-detail")
            .test_support()
            .size_full()
            .border_l_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().pl_1().text_sm().font_semibold().child("Row"))
                    .child(div().flex_1())
                    .child(
                        Button::new("row-detail-copy")
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip("Copy as JSON")
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
}

#[cfg(test)]
mod tests {
    // No glob: gpui_kit's `test` macro would shadow the built-in `#[test]`.
    use super::row_text;
    use serde_json::json;

    #[test]
    fn rows_read_like_json_without_escaping() {
        let row = [
            ("id".to_owned(), json!(7)),
            ("ok".to_owned(), json!(true)),
            ("note".to_owned(), json!("say \"hi\"")),
            ("gone".to_owned(), json!(null)),
            ("meta".to_owned(), json!({"a": [1]})),
        ];
        assert_eq!(
            row_text(&row),
            "{\n  id: 7\n  ok: true\n  note: \"say \"hi\"\"\n  gone: null\n  \
             meta: {\n    \"a\": [\n      1\n    ]\n  }\n}"
        );
    }
}
