//! Table delegate that renders a [`QueryResult`].

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Size, StyledExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;
use tuplebase_core::db::QueryResult;

#[derive(Default)]
pub struct ResultsDelegate {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    /// What the database returned, for the row detail panel.
    values: Vec<Vec<Value>>,
    /// A document engine's documents, one per row, in relaxed Extended JSON; `None` for
    /// SQL. Where set, cells shorten Extended JSON, and a field a document lacks shows
    /// as an empty cell rather than as `NULL`.
    documents: Option<Vec<Value>>,
    ran: bool,
    /// Columns that hold only numbers, which line up on the right in the code font.
    numeric: Vec<bool>,
    /// The cell a right click last landed on, for the row's context menu.
    menu_cell: Option<(usize, usize)>,
}

impl ResultsDelegate {
    /// Shows `result`. Columns are sized for text of `font_size`.
    pub fn set(&mut self, result: &QueryResult, font_size: Pixels) {
        self.ran = true;
        let char_width = f32::from(font_size) * CHAR_WIDTH_EM;
        self.values = result.rows.clone();
        self.documents = result.documents.clone();
        self.rows = match &self.documents {
            None => result
                .rows
                .iter()
                .map(|row| row.iter().map(cell_text).collect())
                .collect(),
            Some(documents) => result
                .rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    row.iter()
                        .zip(&result.columns)
                        .map(|(value, column)| {
                            if is_absent(documents.get(i), column) {
                                String::new()
                            } else {
                                document_cell_text(value)
                            }
                        })
                        .collect()
                })
                .collect(),
        };
        self.columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                // No padding: `render_td` pads the cell itself, so that it covers the
                // whole cell and a right click anywhere in it is seen.
                Column::new(SharedString::from(format!("c{i}")), name.clone())
                    .width(column_width(name, &self.rows, i, char_width))
                    .p_0()
            })
            .collect();
        let documents = self.documents.is_some();
        self.numeric = (0..self.columns.len())
            .map(|i| is_numeric_column(&self.values, i, documents))
            .collect();
    }

    /// Text to show instead of the table when there is nothing to display.
    pub fn placeholder(&self) -> Option<&'static str> {
        if !self.columns.is_empty() {
            None
        } else if self.ran {
            Some("The statement returned no rows.")
        } else {
            Some("Run a query to see rows here.")
        }
    }

    /// Whether a query has run since the table was last cleared.
    pub fn ran(&self) -> bool {
        self.ran
    }

    /// A row's fields as the database returned them, by column name.
    pub fn row(&self, row_ix: usize) -> Option<Vec<(String, Value)>> {
        let values = self.values.get(row_ix)?;
        let names = self.columns.iter().map(|column| column.name.to_string());
        Some(names.zip(values.iter().cloned()).collect())
    }

    /// The document a row shows, when the result holds documents.
    pub fn document(&self, row_ix: usize) -> Option<&Value> {
        self.documents.as_ref()?.get(row_ix)
    }

    /// Whether a cell is a field its document lacks, as opposed to one holding null.
    fn is_absent_cell(&self, row_ix: usize, col_ix: usize) -> bool {
        self.documents.as_ref().is_some_and(|documents| {
            let column = self.columns.get(col_ix).map(|c| c.name.to_string());
            column.is_some_and(|column| is_absent(documents.get(row_ix), &column))
        })
    }

    pub fn clear(&mut self) {
        self.ran = false;
        self.documents = None;
        self.menu_cell = None;
        self.columns.clear();
        self.rows.clear();
        self.values.clear();
        self.numeric.clear();
    }
}

/// Wide enough for the longest value in the first rows, within sensible limits.
/// About how wide a character of the table's text is, in ems.
const CHAR_WIDTH_EM: f32 = 0.6;

fn column_width(name: &str, rows: &[Vec<String>], index: usize, char_width: f32) -> f32 {
    const PADDING: f32 = 28.;
    let longest = rows
        .iter()
        .take(200)
        .map(|row| row[index].chars().count())
        .chain([name.chars().count()])
        .max()
        .unwrap_or(0);
    (longest as f32 * char_width + PADDING).clamp(80., 480.)
}

/// Whether every value in the column the database returned is a number. Exact
/// numbers (`NUMERIC`, `DECIMAL`) arrive as text, so text that is a plain number counts.
/// NULLs don't decide it; a column of NULLs only is not numeric. In `documents`, the
/// Extended JSON numbers (`{"$numberDecimal": "1.5"}`) count too.
fn is_numeric_column(rows: &[Vec<Value>], index: usize, documents: bool) -> bool {
    let mut any = false;
    for value in rows.iter().take(200).filter_map(|row| row.get(index)) {
        let number = match value {
            Value::Null => continue,
            Value::Number(_) => true,
            Value::String(text) => is_plain_number(text),
            Value::Object(object) if documents => ejson_number(object).is_some(),
            _ => false,
        };
        if !number {
            return false;
        }
        any = true;
    }
    any
}

/// `12`, `-3.50`, `0.1`: digits with an optional sign and decimal point, nothing else.
fn is_plain_number(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let mut parts = digits.splitn(2, '.');
    let whole = parts.next().unwrap_or_default();
    let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    all_digits(whole) && parts.next().is_none_or(all_digits)
}

/// A value as the table shows it and as "Copy value" copies it: text without quotes,
/// `NULL` for null, and compact JSON for the rest.
fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Whether `document` lacks the top-level field `column`. A row with no document, or
/// a document that isn't an object, lacks nothing.
fn is_absent(document: Option<&Value>, column: &str) -> bool {
    document
        .and_then(Value::as_object)
        .is_some_and(|object| !object.contains_key(column))
}

/// The number an Extended JSON number wrapper holds, as text: `{"$numberLong": "5"}`.
fn ejson_number(object: &serde_json::Map<String, Value>) -> Option<&str> {
    let (tag, value) = single_entry(object)?;
    matches!(
        tag,
        "$numberDecimal" | "$numberLong" | "$numberInt" | "$numberDouble"
    )
    .then(|| value.as_str())
    .flatten()
}

fn single_entry(object: &serde_json::Map<String, Value>) -> Option<(&str, &Value)> {
    let mut entries = object.iter();
    let (key, value) = entries.next()?;
    entries.next().is_none().then_some((key.as_str(), value))
}

/// A one-key Extended JSON value written as the shell writes it: `ObjectId("…")`, the
/// number of a `$numberDecimal`, a date as ISO text (`top`) or `ISODate("…")` (nested).
fn short_ejson(object: &serde_json::Map<String, Value>, top: bool) -> Option<String> {
    if let Some(number) = ejson_number(object) {
        return Some(number.to_owned());
    }
    let (tag, value) = single_entry(object)?;
    match (tag, value) {
        ("$oid", Value::String(id)) => Some(format!("ObjectId(\"{id}\")")),
        ("$date", Value::String(iso)) if top => Some(iso.clone()),
        ("$date", Value::String(iso)) => Some(format!("ISODate(\"{iso}\")")),
        // Outside years 1970 to 9999 the date comes as milliseconds since 1970.
        ("$date", Value::Object(inner)) => {
            ejson_number(inner).map(|millis| format!("Date({millis})"))
        }
        _ => None,
    }
}

/// A document's value as its cell shows it: text without quotes, `NULL` for null, and
/// compact JSON for the rest, with Extended JSON wrappers shortened at any depth.
fn document_cell_text(value: &Value) -> String {
    match value {
        Value::Object(object) => match short_ejson(object, true) {
            Some(text) => text,
            None => compact_ejson(value),
        },
        Value::Array(_) => compact_ejson(value),
        other => cell_text(other),
    }
}

fn compact_ejson(value: &Value) -> String {
    let mut out = String::new();
    write_compact(&mut out, value);
    out
}

fn write_compact(out: &mut String, value: &Value) {
    match value {
        Value::Object(object) => {
            if let Some(text) = short_ejson(object, false) {
                out.push_str(&text);
                return;
            }
            out.push('{');
            for (i, (key, value)) in object.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push(':');
                write_compact(out, value);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(out, item);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// A row as a pretty JSON object, columns in result order. A column name that repeats,
/// as in `SELECT a.id, b.id`, gets a suffix (`id_2`) so no value is lost.
pub fn row_json(row: &[(String, Value)]) -> String {
    let mut object = serde_json::Map::new();
    for (name, value) in row {
        let mut key = name.clone();
        let mut n = 1;
        while object.contains_key(&key) {
            n += 1;
            key = format!("{name}_{n}");
        }
        object.insert(key, value.clone());
    }
    serde_json::to_string_pretty(&Value::Object(object)).unwrap_or_default()
}

/// A cell filled to its edges, with the padding the table's small size gives a cell.
/// A faint line on its right edge separates it from the next column.
fn padded_cell(cx: &App) -> Div {
    let padding = Size::Small.table_cell_padding();
    div()
        .size_full()
        .border_r_1()
        .border_color(cx.theme().border)
        .pl(padding.left)
        .pr(padding.right)
        .pt(padding.top)
        .pb(padding.bottom)
}

fn copy_item(label: &'static str, text: String) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, _, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
    })
}

impl TableDelegate for ResultsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    /// Stripes here rather than with `DataTable::stripe`, which also pads a short result
    /// with blank striped rows. A click on a row opens its detail panel, hence the hand.
    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let stripe = cx.theme().tokens.table_even;
        div()
            .id(("row", row_ix))
            .cursor_pointer()
            .when(row_ix.is_multiple_of(2), |row| row.bg(stripe))
    }

    /// The columns have no padding (see `set`), so the header pads itself like the cells.
    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let numeric = self.numeric.get(col_ix).copied().unwrap_or_default();
        padded_cell(cx)
            .font_semibold()
            .when(numeric, |cell| cell.text_right())
            .child(self.columns[col_ix].name.clone())
    }

    /// The cell's own padding, so that the right click handler covers all of it.
    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let numeric = self.numeric.get(col_ix).copied().unwrap_or_default();
        let value = &self.values[row_ix][col_ix];
        let absent = self.is_absent_cell(row_ix, col_ix);
        // The SQL editor's colors: numbers and booleans stand out, NULL steps back.
        let syntax = |name: &str| {
            let style = theme.highlight_theme.style(name).unwrap_or_default();
            (style.color, style.font_style)
        };
        let (color, font_style) = match value {
            _ if absent => (None, None),
            Value::Null => syntax("comment"),
            Value::Bool(_) => syntax("boolean"),
            _ if numeric => syntax("number"),
            _ => (None, None),
        };
        padded_cell(cx)
            .when(numeric || (value.is_null() && !absent), |cell| {
                cell.font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
            })
            .when(numeric, |cell| cell.text_right())
            .when_some(color, |cell, color| cell.text_color(color))
            .when(font_style == Some(FontStyle::Italic), |cell| cell.italic())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |table, _, _, _| {
                    table.delegate_mut().menu_cell = Some((row_ix, col_ix));
                }),
            )
            .child(self.rows[row_ix][col_ix].clone())
    }

    /// "Copy value" copies the cell the right click landed on; the menu leaves it out
    /// when the click missed every cell, rather than copy one from an earlier click.
    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        // The text the cell shows, as `cell_text` or the document's own wrote it.
        let value = self
            .menu_cell
            .take()
            .filter(|(row, _)| *row == row_ix)
            .and_then(|(row, col)| self.rows.get(row)?.get(col))
            .cloned();
        let row = match self.document(row_ix) {
            Some(document) => serde_json::to_string_pretty(document).ok(),
            None => self.row(row_ix).map(|row| row_json(&row)),
        };
        menu.when_some(value, |menu, value| {
            menu.item(copy_item("Copy value", value))
        })
        .when_some(row, |menu, row| {
            menu.item(copy_item("Copy row as JSON", row))
        })
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        h_flex()
            .size_full()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("No rows.")
    }
}

#[cfg(test)]
impl ResultsDelegate {
    /// `set` at the default text size.
    pub(crate) fn show(&mut self, result: &QueryResult) {
        self.set(result, px(15.));
    }

    /// The text a cell shows.
    pub(crate) fn cell(&self, row_ix: usize, col_ix: usize) -> &str {
        &self.rows[row_ix][col_ix]
    }
}

#[cfg(test)]
mod tests {
    // No glob: gpui_kit's `test` macro would shadow the built-in `#[test]`.
    use super::{
        column_width, document_cell_text, is_absent, is_numeric_column, is_plain_number, row_json,
    };
    use serde_json::json;

    #[test]
    fn column_width_fits_content_within_limits() {
        let rows = vec![vec!["a".repeat(30)]];
        let fitted = column_width("id", &rows, 0, 8.);
        assert_eq!(fitted, 30. * 8. + 28.);
        assert_eq!(column_width("id", &[], 0, 8.), 80.);
        assert_eq!(column_width("id", &[vec!["a".repeat(500)]], 0, 8.), 480.);
    }

    #[test]
    fn number_columns_are_found_also_when_they_arrive_as_text() {
        let column = |values: Vec<serde_json::Value>| {
            let rows: Vec<_> = values.into_iter().map(|v| vec![v]).collect();
            is_numeric_column(&rows, 0, false)
        };
        assert!(column(vec![json!(1), json!(null), json!(2.5)]));
        assert!(column(vec![json!("3700.00"), json!("-1"), json!(null)]));
        assert!(!column(vec![json!(1), json!("US")]));
        assert!(!column(vec![json!(true)]));
        assert!(!column(vec![json!(null)]));
        assert!(!column(vec![]));
        assert!(is_plain_number("0.1"));
        for text in ["", "-", "1.", ".5", "1e5", "1.2.3", "12a", "2025-09-14"] {
            assert!(!is_plain_number(text), "{text}");
        }
    }

    #[test]
    fn row_json_keeps_column_order_and_repeated_names() {
        let row = [
            ("name".to_owned(), json!("say \"hi\"")),
            ("id".to_owned(), json!(1)),
            ("id".to_owned(), json!(null)),
        ];
        assert_eq!(
            row_json(&row),
            "{\n  \"name\": \"say \\\"hi\\\"\",\n  \"id\": 1,\n  \"id_2\": null\n}"
        );
    }

    #[test]
    fn document_cells_shorten_extended_json() {
        let id = json!({"$oid": "65f0c0ffee0000000000abcd"});
        assert_eq!(
            document_cell_text(&id),
            "ObjectId(\"65f0c0ffee0000000000abcd\")"
        );
        let date = json!({"$date": "2024-02-03T04:05:06Z"});
        assert_eq!(document_cell_text(&date), "2024-02-03T04:05:06Z");
        let old = json!({"$date": {"$numberLong": "-1000"}});
        assert_eq!(document_cell_text(&old), "Date(-1000)");
        assert_eq!(
            document_cell_text(&json!({"$numberDecimal": "19.99"})),
            "19.99"
        );
        assert_eq!(
            document_cell_text(&json!({"$numberLong": "9007199254740993"})),
            "9007199254740993"
        );
        // Nested values are shortened too; plain objects and text stay as they are.
        assert_eq!(
            document_cell_text(&json!({"by": id, "at": date, "n": 1, "s": "x"})),
            "{\"by\":ObjectId(\"65f0c0ffee0000000000abcd\"),\
             \"at\":ISODate(\"2024-02-03T04:05:06Z\"),\"n\":1,\"s\":\"x\"}"
        );
        assert_eq!(
            document_cell_text(&json!([{"$numberDecimal": "1.5"}, null])),
            "[1.5,null]"
        );
        assert_eq!(
            document_cell_text(&json!({"$oid": "a", "x": 1})),
            "{\"$oid\":\"a\",\"x\":1}"
        );
        assert_eq!(document_cell_text(&json!("plain")), "plain");
        assert_eq!(document_cell_text(&json!(null)), "NULL");
    }

    #[test]
    fn decimal_columns_of_documents_line_up_as_numbers() {
        let rows = vec![vec![json!({"$numberDecimal": "19.99"})], vec![json!(5)]];
        assert!(is_numeric_column(&rows, 0, true));
        assert!(!is_numeric_column(&rows, 0, false));
    }

    #[test]
    fn a_missing_field_is_absent_and_a_null_one_is_not() {
        let document = json!({"a": null});
        assert!(!is_absent(Some(&document), "a"));
        assert!(is_absent(Some(&document), "b"));
        assert!(!is_absent(None, "b"));
        assert!(!is_absent(Some(&json!(3)), "b"));
    }
}
