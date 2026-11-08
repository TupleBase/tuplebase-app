//! Schema browser: the tables and columns of the selected connection.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::Connection;
use tuplebase_core::db::{ColumnInfo, Db, TableInfo};

use crate::runtime::io;

const TABLE_ICON: &[u8] = include_bytes!("../../../assets/icons/table-2.svg");
const PANEL_WIDTH: f32 = 240.;
const ROW_HEIGHT: f32 = 26.;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TableKey {
    schema: String,
    table: String,
}

pub(crate) enum Columns {
    Loading,
    Loaded(Vec<ColumnInfo>),
    Failed,
}

/// One visible line of the tree.
#[derive(Clone, Debug, PartialEq)]
enum Row {
    Schema {
        name: String,
        tables: usize,
        collapsed: bool,
    },
    Table {
        key: TableKey,
        is_view: bool,
        expanded: bool,
    },
    Column {
        name: String,
        data_type: String,
    },
    Note(&'static str),
}

/// The visible lines, in order. A filter searches table names across every schema, so
/// matches stay visible even in a collapsed schema.
fn flatten(
    tables: &[TableInfo],
    filter: &str,
    collapsed: &HashSet<String>,
    expanded: &HashMap<TableKey, Columns>,
) -> Vec<Row> {
    let filter = filter.trim().to_lowercase();
    let matching: Vec<&TableInfo> = tables
        .iter()
        .filter(|t| t.name.to_lowercase().contains(&filter))
        .collect();
    let mut rows = Vec::new();
    let mut start = 0;
    while start < matching.len() {
        let schema = &matching[start].schema;
        let end = matching[start..]
            .iter()
            .position(|t| &t.schema != schema)
            .map_or(matching.len(), |offset| start + offset);
        let is_collapsed = filter.is_empty() && collapsed.contains(schema);
        rows.push(Row::Schema {
            name: schema.clone(),
            tables: end - start,
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            for table in &matching[start..end] {
                let key = TableKey {
                    schema: table.schema.clone(),
                    table: table.name.clone(),
                };
                let columns = expanded.get(&key);
                rows.push(Row::Table {
                    key,
                    is_view: table.kind.to_uppercase().contains("VIEW"),
                    expanded: columns.is_some(),
                });
                match columns {
                    None => {}
                    Some(Columns::Loading) => rows.push(Row::Note("Loading columns…")),
                    Some(Columns::Failed) => rows.push(Row::Note("Could not load columns.")),
                    Some(Columns::Loaded(list)) => {
                        rows.extend(list.iter().map(|c| Row::Column {
                            name: c.name.clone(),
                            data_type: c.data_type.clone(),
                        }));
                    }
                }
            }
        }
        start = end;
    }
    rows
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Load {
    Idle,
    Loading,
    Ready,
    Failed,
}

pub enum SchemaEvent {
    /// Show rows from a table using this SQL.
    Preview(String),
}

pub struct SchemaPanel {
    db: Arc<Db>,
    connection: Option<Connection>,
    pub(crate) load: Load,
    pub(crate) tables: Vec<TableInfo>,
    collapsed: HashSet<String>,
    pub(crate) expanded: HashMap<TableKey, Columns>,
    filter: Entity<InputState>,
    scroll: UniformListScrollHandle,
    _filter_subscription: Subscription,
}

impl EventEmitter<SchemaEvent> for SchemaPanel {}

impl SchemaPanel {
    pub fn new(db: Arc<Db>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables"));
        let _filter_subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            db,
            connection: None,
            load: Load::Idle,
            tables: Vec::new(),
            collapsed: HashSet::new(),
            expanded: HashMap::new(),
            filter,
            scroll: UniformListScrollHandle::new(),
            _filter_subscription,
        }
    }

    /// Shows the tables of `connection`, or nothing for `None`.
    pub fn set_connection(&mut self, connection: Option<Connection>, cx: &mut Context<Self>) {
        self.connection = connection;
        self.tables.clear();
        self.collapsed.clear();
        self.load = Load::Idle;
        self.refresh(cx);
    }

    /// Reloads the table list and forgets any loaded columns.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        cx.notify();
        let Some(conn) = self.connection.clone() else {
            return;
        };
        self.load = Load::Loading;
        let db = self.db.clone();
        let name = conn.name.clone();
        cx.spawn(async move |this, cx| {
            let result = io(async move { db.list_tables(&conn).await }).await;
            this.update(cx, |this, cx| {
                if !this.is_showing(&name) {
                    return;
                }
                match result {
                    Ok(tables) => {
                        this.tables = tables;
                        this.load = Load::Ready;
                    }
                    Err(_) => {
                        this.tables.clear();
                        this.load = Load::Failed;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Results can arrive after the user has moved to another connection.
    fn is_showing(&self, name: &str) -> bool {
        self.connection.as_ref().is_some_and(|c| c.name == name)
    }

    fn toggle_schema(&mut self, schema: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(schema) {
            self.collapsed.insert(schema.to_owned());
        }
        cx.notify();
    }

    fn toggle_table(&mut self, key: TableKey, cx: &mut Context<Self>) {
        cx.notify();
        if self.expanded.remove(&key).is_some() {
            return;
        }
        let Some(conn) = self.connection.clone() else {
            return;
        };
        self.expanded.insert(key.clone(), Columns::Loading);
        let db = self.db.clone();
        let name = conn.name.clone();
        cx.spawn(async move |this, cx| {
            let target = key.clone();
            let result = io(async move {
                db.describe_table(&conn, Some(&target.schema), &target.table)
                    .await
            })
            .await;
            this.update(cx, |this, cx| {
                if !this.is_showing(&name) || !this.expanded.contains_key(&key) {
                    return;
                }
                let columns = result.map_or(Columns::Failed, Columns::Loaded);
                this.expanded.insert(key, columns);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn preview(&mut self, key: &TableKey, cx: &mut Context<Self>) {
        if let Some(conn) = &self.connection {
            let sql = conn.adapter.preview_sql(&key.schema, &key.table);
            cx.emit(SchemaEvent::Preview(sql));
        }
    }

    fn render_row(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover, radius) = (theme.muted_foreground, theme.accent, theme.radius);
        let line = || {
            h_flex()
                .h(px(ROW_HEIGHT))
                .gap_1p5()
                .pr_1()
                .rounded(radius)
                .overflow_hidden()
        };
        let chevron = |open: bool| {
            let name = if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            };
            Icon::new(name).xsmall().text_color(muted)
        };
        match row.clone() {
            Row::Schema {
                name,
                tables,
                collapsed,
            } => line()
                .id(SharedString::from(format!("schema-{name}")))
                .test_support()
                .pl_1()
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(chevron(!collapsed))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .font_medium()
                        .child(name.clone()),
                )
                .child(div().text_xs().text_color(muted).child(tables.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_schema(&name, cx)))
                .into_any_element(),
            Row::Table {
                key,
                is_view,
                expanded,
            } => {
                let id = format!("table-{}-{}", key.schema, key.table);
                let group = SharedString::from(format!("row-{id}"));
                let preview_key = key.clone();
                line()
                    .id(SharedString::from(id.clone()))
                    .test_support()
                    .group(group.clone())
                    .relative()
                    .pl(px(14.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(chevron(expanded))
                    .child(Icon::default().data(TABLE_ICON).xsmall().text_color(muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(key.table.clone()),
                    )
                    .when(is_view, |el| {
                        el.child(div().text_xs().text_color(muted).child("view"))
                    })
                    .child(
                        h_flex()
                            .absolute()
                            .right_0()
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .child(
                                Button::new(SharedString::from(format!("preview-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Play)
                                    .tooltip("Show the first 100 rows")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.preview(&preview_key, cx);
                                    })),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_table(key.clone(), cx);
                    }))
                    .into_any_element()
            }
            Row::Column { name, data_type } => line()
                .pl(px(50.))
                .child(div().flex_1().min_w_0().truncate().text_xs().child(name))
                .child(
                    div()
                        .max_w(px(90.))
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(data_type),
                )
                .into_any_element(),
            Row::Note(text) => line()
                .pl(px(50.))
                .text_xs()
                .text_color(muted)
                .child(text)
                .into_any_element(),
        }
    }
}

impl Render for SchemaPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter.read(cx).value().to_string();
        let rows = Rc::new(flatten(
            &self.tables,
            &filter,
            &self.collapsed,
            &self.expanded,
        ));
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let message = match self.load {
            Load::Idle => Some(""),
            Load::Loading if self.tables.is_empty() => Some("Loading tables…"),
            Load::Failed => Some("Could not load tables."),
            _ if rows.is_empty() && filter.trim().is_empty() => Some("No tables found."),
            _ if rows.is_empty() => Some("No tables match."),
            _ => None,
        };
        v_flex()
            .w(px(PANEL_WIDTH))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(
                        Input::new(&self.filter)
                            .id("schema-filter")
                            .small()
                            .prefix(Icon::new(IconName::Search).xsmall().text_color(muted)),
                    )
                    .child(
                        Button::new("schema-refresh")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip("Reload tables")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(match message {
                Some(text) => div()
                    .px_3()
                    .text_sm()
                    .text_color(muted)
                    .child(text)
                    .into_any_element(),
                None => {
                    let count = rows.len();
                    uniform_list(
                        "schema-rows",
                        count,
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            rows[range]
                                .iter()
                                .map(|row| this.render_row(row, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .px_1()
                    .flex_1()
                    .track_scroll(&self.scroll)
                    .into_any_element()
                }
            })
    }
}

#[cfg(test)]
mod tests {
    // No glob: gpui_kit's `test` macro would shadow the built-in `#[test]`.
    use super::{ColumnInfo, Columns, HashMap, HashSet, Row, TableInfo, TableKey, flatten};

    fn table(schema: &str, name: &str, kind: &str) -> TableInfo {
        TableInfo {
            schema: schema.into(),
            name: name.into(),
            kind: kind.into(),
        }
    }

    fn sample() -> Vec<TableInfo> {
        vec![
            table("public", "orders", "BASE TABLE"),
            table("public", "users", "BASE TABLE"),
            table("reports", "daily", "VIEW"),
        ]
    }

    fn key(schema: &str, name: &str) -> TableKey {
        TableKey {
            schema: schema.into(),
            table: name.into(),
        }
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Schema { name, tables, .. } => format!("{name}({tables})"),
                Row::Table { key, .. } => key.table.clone(),
                Row::Column { name, .. } => format!("  {name}"),
                Row::Note(text) => format!("  {text}"),
            })
            .collect()
    }

    #[test]
    fn tables_are_grouped_by_schema() {
        let rows = flatten(&sample(), "", &HashSet::new(), &HashMap::new());
        assert_eq!(
            names(&rows),
            ["public(2)", "orders", "users", "reports(1)", "daily"]
        );
    }

    #[test]
    fn views_are_marked() {
        let rows = flatten(&sample(), "daily", &HashSet::new(), &HashMap::new());
        assert!(matches!(rows[1], Row::Table { is_view: true, .. }));
    }

    #[test]
    fn collapsed_schema_hides_its_tables() {
        let collapsed = HashSet::from(["public".to_string()]);
        let rows = flatten(&sample(), "", &collapsed, &HashMap::new());
        assert_eq!(names(&rows), ["public(2)", "reports(1)", "daily"]);
    }

    #[test]
    fn filter_ignores_case_and_shows_matches_in_collapsed_schemas() {
        let collapsed = HashSet::from(["public".to_string()]);
        let rows = flatten(&sample(), " USE ", &collapsed, &HashMap::new());
        assert_eq!(names(&rows), ["public(1)", "users"]);
    }

    #[test]
    fn expanded_table_lists_columns_or_status() {
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "integer".into(),
            nullable: false,
        }];
        let expanded = HashMap::from([
            (key("public", "orders"), Columns::Loaded(columns)),
            (key("public", "users"), Columns::Loading),
        ]);
        let rows = flatten(&sample(), "", &HashSet::new(), &expanded);
        assert_eq!(
            names(&rows),
            [
                "public(2)",
                "orders",
                "  id",
                "users",
                "  Loading columns…",
                "reports(1)",
                "daily"
            ]
        );
    }

    #[test]
    fn no_tables_means_no_rows() {
        assert!(flatten(&[], "", &HashSet::new(), &HashMap::new()).is_empty());
    }
}
