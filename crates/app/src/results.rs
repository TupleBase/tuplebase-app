//! Table delegate that renders a [`QueryResult`].

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Size, h_flex};
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
    ran: bool,
    /// The cell a right click last landed on, for the row's context menu.
    menu_cell: Option<(usize, usize)>,
}

impl ResultsDelegate {
    pub fn set(&mut self, result: &QueryResult) {
        self.ran = true;
        self.values = result.rows.clone();
        self.rows = result
            .rows
            .iter()
            .map(|row| row.iter().map(cell_text).collect())
            .collect();
        self.columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                // No padding: `render_td` pads the cell itself, so that it covers the
                // whole cell and a right click anywhere in it is seen.
                Column::new(SharedString::from(format!("c{i}")), name.clone())
                    .width(column_width(name, &self.rows, i))
                    .p_0()
            })
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

    /// A row's fields as the database returned them, by column name.
    pub fn row(&self, row_ix: usize) -> Option<Vec<(String, Value)>> {
        let values = self.values.get(row_ix)?;
        let names = self.columns.iter().map(|column| column.name.to_string());
        Some(names.zip(values.iter().cloned()).collect())
    }

    pub fn clear(&mut self) {
        self.ran = false;
        self.menu_cell = None;
        self.columns.clear();
        self.rows.clear();
        self.values.clear();
    }
}

/// Wide enough for the longest value in the first rows, within sensible limits.
fn column_width(name: &str, rows: &[Vec<String>], index: usize) -> f32 {
    const CHAR_WIDTH: f32 = 7.4;
    const PADDING: f32 = 28.;
    let longest = rows
        .iter()
        .take(200)
        .map(|row| row[index].chars().count())
        .chain([name.chars().count()])
        .max()
        .unwrap_or(0);
    (longest as f32 * CHAR_WIDTH + PADDING).clamp(80., 420.)
}

/// A value as the table shows it and as "Copy value" copies it: text without quotes,
/// `NULL` for null, and compact JSON for the rest.
pub fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
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
fn padded_cell() -> Div {
    let padding = Size::Small.table_cell_padding();
    div()
        .size_full()
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
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        padded_cell().child(self.columns[col_ix].name.clone())
    }

    /// The cell's own padding, so that the right click handler covers all of it.
    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        padded_cell()
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
        let value = self
            .menu_cell
            .take()
            .filter(|(row, _)| *row == row_ix)
            .and_then(|(row, col)| self.values.get(row)?.get(col))
            .map(cell_text);
        let row = self.row(row_ix).map(|row| row_json(&row));
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
mod tests {
    // No glob: gpui_kit's `test` macro would shadow the built-in `#[test]`.
    use super::{column_width, row_json};
    use serde_json::json;

    #[test]
    fn column_width_fits_content_within_limits() {
        let rows = vec![vec!["a".repeat(30)]];
        let fitted = column_width("id", &rows, 0);
        assert!(fitted > 200. && fitted < 300.);
        assert_eq!(column_width("id", &[], 0), 80.);
        assert_eq!(column_width("id", &[vec!["a".repeat(500)]], 0), 420.);
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
}
