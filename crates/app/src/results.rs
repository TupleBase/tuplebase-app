//! Table delegate that renders a [`QueryResult`].

use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::*;
use serde_json::Value;
use tuplebase_core::db::QueryResult;

#[derive(Default)]
pub struct ResultsDelegate {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
}

impl ResultsDelegate {
    pub fn set(&mut self, result: &QueryResult) {
        self.columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, name)| {
                Column::new(SharedString::from(format!("c{i}")), name.clone()).width(160.)
            })
            .collect();
        self.rows = result
            .rows
            .iter()
            .map(|row| row.iter().map(cell_text).collect())
            .collect();
    }

    pub fn clear(&mut self) {
        self.columns.clear();
        self.rows.clear();
    }
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
}
