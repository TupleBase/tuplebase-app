//! Table delegate that renders a [`QueryResult`].

use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;
use serde_json::Value;
use tuplebase_core::db::QueryResult;

#[derive(Default)]
pub struct ResultsDelegate {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    ran: bool,
}

impl ResultsDelegate {
    pub fn set(&mut self, result: &QueryResult) {
        self.ran = true;
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
                Column::new(SharedString::from(format!("c{i}")), name.clone())
                    .width(column_width(name, &self.rows, i))
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

    pub fn clear(&mut self) {
        self.ran = false;
        self.columns.clear();
        self.rows.clear();
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

fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        self.rows[row_ix][col_ix].clone()
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
    use super::column_width;

    #[test]
    fn column_width_fits_content_within_limits() {
        let rows = vec![vec!["a".repeat(30)]];
        let fitted = column_width("id", &rows, 0);
        assert!(fitted > 200. && fitted < 300.);
        assert_eq!(column_width("id", &[], 0), 80.);
        assert_eq!(column_width("id", &[vec!["a".repeat(500)]], 0), 420.);
    }
}
