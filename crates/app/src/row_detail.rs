//! The row detail panel: one result row with its fields listed top to bottom.
//!
//! It reads like JSON (braces, `name: value`, quoted text) without being JSON: nothing
//! is escaped, so a value shows exactly as the database returned it. Copying gives the
//! value as the table shows it, and the whole row as real JSON.

use gpui_kit::component::button::*;
use gpui_kit::component::*;
use gpui_kit::*;
use serde_json::Value;

use crate::results::{cell_text, row_json};

pub enum RowDetailEvent {
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Text,
    Null,
    /// Numbers, booleans, and the arrays and objects of a JSON column.
    Other,
}

struct Field {
    name: SharedString,
    value: SharedString,
    kind: Kind,
    /// What its copy button copies.
    copy: SharedString,
}

/// The row on show: its fields, and the row as JSON for "Copy as JSON".
struct Shown {
    fields: Vec<Field>,
    json: SharedString,
}

#[derive(Default)]
pub struct RowDetail {
    /// `None` while no row is chosen, which keeps the panel closed.
    shown: Option<Shown>,
}

impl EventEmitter<RowDetailEvent> for RowDetail {}

impl RowDetail {
    pub fn is_open(&self) -> bool {
        self.shown.is_some()
    }

    pub fn show(&mut self, row: Vec<(String, Value)>, cx: &mut Context<Self>) {
        let json = row_json(&row).into();
        let fields = row
            .into_iter()
            .map(|(name, value)| {
                let (text, kind) = value_text(&value);
                Field {
                    name: name.into(),
                    value: text.into(),
                    kind,
                    copy: cell_text(&value).into(),
                }
            })
            .collect();
        self.shown = Some(Shown { fields, json });
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.shown.take().is_some() {
            cx.notify();
        }
    }
}

/// How a value reads in the panel: text in quotes, a JSON column spread over lines.
fn value_text(value: &Value) -> (String, Kind) {
    match value {
        Value::Null => ("null".into(), Kind::Null),
        Value::String(text) => (format!("\"{text}\""), Kind::Text),
        Value::Array(_) | Value::Object(_) => (
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
            Kind::Other,
        ),
        other => (other.to_string(), Kind::Other),
    }
}

impl Render for RowDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (fields, json) = match &self.shown {
            Some(shown) => (shown.fields.as_slice(), shown.json.clone()),
            None => (&[][..], SharedString::default()),
        };
        let brace = |text: &'static str| div().text_color(muted).child(text);
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
                    .child(copy_button("row-detail-copy", json, "Copy as JSON"))
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
                v_flex()
                    .id("row-detail-fields")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .gap_0p5()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
                    .child(brace("{"))
                    .children(fields.iter().enumerate().map(|(ix, field)| {
                        let color = match field.kind {
                            Kind::Text => theme.foreground,
                            Kind::Null => muted,
                            Kind::Other => theme.info,
                        };
                        let group = SharedString::from(format!("row-detail-field-{ix}"));
                        h_flex()
                            .group(group.clone())
                            .pl_4()
                            .gap_1()
                            .items_start()
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(muted)
                                    .child(SharedString::from(format!("{}: ", field.name))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(color)
                                    .child(field.value.clone()),
                            )
                            .child(
                                // Shown on hover only, so the fields stay easy to read.
                                div()
                                    .flex_none()
                                    .opacity(0.)
                                    .group_hover(group, |style| style.opacity(1.))
                                    .child(copy_button(
                                        ("row-detail-copy-field", ix),
                                        field.copy.clone(),
                                        "Copy value",
                                    )),
                            )
                    }))
                    .child(brace("}")),
            )
    }
}

fn copy_button(id: impl Into<ElementId>, text: SharedString, tooltip: &'static str) -> Button {
    Button::new(id)
        .cursor_pointer()
        .ghost()
        .xsmall()
        .icon(IconName::Copy)
        .tooltip(tooltip)
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        })
}

#[cfg(test)]
impl RowDetail {
    pub(crate) fn shown(&self) -> Vec<(String, String)> {
        self.shown
            .iter()
            .flat_map(|shown| &shown.fields)
            .map(|field| (field.name.to_string(), field.value.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    // No glob: gpui_kit's `test` macro would shadow the built-in `#[test]`.
    use super::{Kind, value_text};
    use serde_json::json;

    #[test]
    fn values_read_like_json_without_escaping() {
        assert_eq!(value_text(&json!(null)), ("null".into(), Kind::Null));
        assert_eq!(value_text(&json!(7)), ("7".into(), Kind::Other));
        assert_eq!(value_text(&json!(true)), ("true".into(), Kind::Other));
        assert_eq!(
            value_text(&json!("say \"hi\"")),
            ("\"say \"hi\"\"".into(), Kind::Text)
        );
        assert_eq!(
            value_text(&json!({"a": [1]})).0,
            "{\n  \"a\": [\n    1\n  ]\n}"
        );
    }
}
