//! Schema browser: the tables, columns and other objects of the selected connection.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::Connection;
use tuplebase_core::db::{ColumnInfo, Db, ObjectInfo, ObjectKind, TableInfo};

use crate::completions::SchemaNames;
use crate::runtime::io;
use crate::tree_nav::{self, TreeCollapse, TreeDown, TreeExpand, TreeOpen, TreeUp};

const TABLE_ICON: &[u8] = include_bytes!("../../../assets/icons/table-2.svg");
const ROW_HEIGHT: f32 = 30.;

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
    /// Everything of one kind besides tables, such as a schema's functions.
    Group {
        schema: String,
        kind: ObjectKind,
        count: usize,
        open: bool,
    },
    Object {
        name: String,
        detail: String,
    },
    Note(&'static str),
}

/// A row that does something. The arrow keys stop on these and pass the others.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    Schema(String),
    Table(TableKey),
    Group(String, ObjectKind),
}

impl Row {
    fn target(&self) -> Option<Target> {
        match self {
            Row::Schema { name, .. } => Some(Target::Schema(name.clone())),
            Row::Table { key, .. } => Some(Target::Table(key.clone())),
            Row::Group { schema, kind, .. } => Some(Target::Group(schema.clone(), *kind)),
            Row::Column { .. } | Row::Object { .. } | Row::Note(_) => None,
        }
    }
}

/// The groups the user has opened.
type OpenGroups = HashSet<(String, ObjectKind)>;

/// The visible lines, in order. A filter searches names across every schema, so matches
/// stay visible even in a collapsed schema or group.
fn flatten(
    tables: &[TableInfo],
    objects: &[ObjectInfo],
    filter: &str,
    collapsed: &HashSet<String>,
    open_groups: &OpenGroups,
    expanded: &HashMap<TableKey, Columns>,
) -> Vec<Row> {
    let filter = filter.trim().to_lowercase();
    let matches = |name: &str| name.to_lowercase().contains(&filter);
    let tables: Vec<&TableInfo> = tables.iter().filter(|t| matches(&t.name)).collect();
    let objects: Vec<&ObjectInfo> = objects.iter().filter(|o| matches(&o.name)).collect();
    let schemas: BTreeSet<&str> = tables
        .iter()
        .map(|t| t.schema.as_str())
        .chain(objects.iter().map(|o| o.schema.as_str()))
        .collect();
    let mut rows = Vec::new();
    for schema in schemas {
        let in_schema: Vec<&&TableInfo> = tables.iter().filter(|t| t.schema == schema).collect();
        let is_collapsed = filter.is_empty() && collapsed.contains(schema);
        rows.push(Row::Schema {
            name: schema.to_owned(),
            tables: in_schema.len(),
            collapsed: is_collapsed,
        });
        if is_collapsed {
            continue;
        }
        for table in in_schema {
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
        let mut groups: BTreeMap<ObjectKind, Vec<&ObjectInfo>> = BTreeMap::new();
        for object in objects.iter().filter(|o| o.schema == schema) {
            groups.entry(object.kind).or_default().push(object);
        }
        for (kind, list) in groups {
            let open = !filter.is_empty() || open_groups.contains(&(schema.to_owned(), kind));
            rows.push(Row::Group {
                schema: schema.to_owned(),
                kind,
                count: list.len(),
                open,
            });
            if open {
                rows.extend(list.iter().map(|o| Row::Object {
                    name: o.name.clone(),
                    detail: o.detail.clone(),
                }));
            }
        }
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
    names: SchemaNames,
    connection: Option<Connection>,
    pub(crate) load: Load,
    pub(crate) tables: Vec<TableInfo>,
    pub(crate) objects: Vec<ObjectInfo>,
    collapsed: HashSet<String>,
    open_groups: OpenGroups,
    pub(crate) expanded: HashMap<TableKey, Columns>,
    filter: Entity<InputState>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// The row arrow keys act on.
    cursor: Option<Target>,
    _filter_subscription: Subscription,
    _focus_subscriptions: [Subscription; 2],
}

impl EventEmitter<SchemaEvent> for SchemaPanel {}

impl SchemaPanel {
    pub fn new(
        db: Arc<Db>,
        names: SchemaNames,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter objects"));
        let _filter_subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        // The cursor only shows while the tree has the keys.
        let focus = cx.focus_handle();
        let _focus_subscriptions = [
            cx.on_focus(&focus, window, |_, _, cx| cx.notify()),
            cx.on_blur(&focus, window, |_, _, cx| cx.notify()),
        ];
        Self {
            db,
            names,
            connection: None,
            load: Load::Idle,
            tables: Vec::new(),
            objects: Vec::new(),
            collapsed: HashSet::new(),
            open_groups: HashSet::new(),
            expanded: HashMap::new(),
            filter,
            scroll: UniformListScrollHandle::new(),
            focus,
            cursor: None,
            _filter_subscription,
            _focus_subscriptions,
        }
    }

    /// Shows the tables of `connection`, or nothing for `None`.
    pub fn set_connection(&mut self, connection: Option<Connection>, cx: &mut Context<Self>) {
        self.connection = connection;
        self.tables.clear();
        self.objects.clear();
        self.names.borrow_mut().clear();
        self.collapsed.clear();
        self.open_groups.clear();
        self.cursor = None;
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
            let result = io(async move {
                let tables = db.list_tables(&conn).await?;
                // Autocomplete is a nicety: the table list still shows without it.
                let columns = db.list_column_names(&conn).await.unwrap_or_default();
                let objects = db.list_objects(&conn).await.unwrap_or_default();
                Ok((tables, columns, objects))
            })
            .await;
            this.update(cx, |this, cx| {
                if !this.is_showing(&name) {
                    return;
                }
                match result {
                    Ok((tables, columns, objects)) => {
                        let mut names: Vec<String> =
                            tables.iter().map(|t| t.name.clone()).collect();
                        names.extend(columns);
                        names.sort();
                        names.dedup();
                        *this.names.borrow_mut() = names;
                        this.tables = tables;
                        this.objects = objects;
                        this.load = Load::Ready;
                    }
                    Err(_) => {
                        this.tables.clear();
                        this.objects.clear();
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

    /// A filter shows every match whatever is closed, so nothing folds while one is set:
    /// the change would only show up, unasked, once the filter is cleared.
    fn is_filtering(&self, cx: &App) -> bool {
        !self.filter_text(cx).trim().is_empty()
    }

    fn toggle_schema(&mut self, schema: &str, cx: &mut Context<Self>) {
        if self.is_filtering(cx) {
            return;
        }
        if !self.collapsed.remove(schema) {
            self.collapsed.insert(schema.to_owned());
        }
        cx.notify();
    }

    fn toggle_group(&mut self, schema: String, kind: ObjectKind, cx: &mut Context<Self>) {
        if self.is_filtering(cx) {
            return;
        }
        let key = (schema, kind);
        if !self.open_groups.remove(&key) {
            self.open_groups.insert(key);
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

    fn filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).value().to_string()
    }

    /// The visible lines, top to bottom.
    fn rows(&self, cx: &App) -> Vec<Row> {
        flatten(
            &self.tables,
            &self.objects,
            &self.filter_text(cx),
            &self.collapsed,
            &self.open_groups,
            &self.expanded,
        )
    }

    #[cfg(test)]
    pub(crate) fn cursor_name(&self) -> Option<String> {
        Some(match self.cursor.as_ref()? {
            Target::Schema(name) => name.clone(),
            Target::Table(key) => key.table.clone(),
            Target::Group(_, kind) => kind.plural().to_owned(),
        })
    }

    /// The row under the cursor, if it is still showing.
    fn cursor_row(&self, cx: &App) -> Option<Row> {
        let cursor = self.cursor.as_ref()?;
        self.rows(cx)
            .into_iter()
            .find(|row| row.target().as_ref() == Some(cursor))
    }

    fn is_cursor(&self, row: &Row, window: &Window) -> bool {
        self.cursor.is_some() && row.target() == self.cursor && self.focus.is_focused(window)
    }

    /// A clicked row takes the cursor, and the tree takes the keys.
    fn take_cursor(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        self.cursor = Some(target);
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// Puts the cursor on `target` and brings it into view.
    fn set_cursor(&mut self, target: Target, cx: &mut Context<Self>) {
        let index = self
            .rows(cx)
            .iter()
            .position(|row| row.target().as_ref() == Some(&target));
        if let Some(index) = index {
            self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
        }
        self.cursor = Some(target);
        cx.notify();
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let mut stops: Vec<Target> = self.rows(cx).iter().filter_map(Row::target).collect();
        let current = self
            .cursor
            .as_ref()
            .and_then(|cursor| stops.iter().position(|stop| stop == cursor));
        if let Some(next) = tree_nav::step(stops.len(), current, delta) {
            self.set_cursor(stops.swap_remove(next), cx);
        }
    }

    /// Left arrow: closes what is open, otherwise jumps to the schema above.
    fn collapse_cursor(&mut self, cx: &mut Context<Self>) {
        let can_close = !self.is_filtering(cx);
        match self.cursor_row(cx) {
            Some(Row::Schema {
                name,
                collapsed: false,
                ..
            }) if can_close => self.toggle_schema(&name, cx),
            Some(Row::Group {
                schema,
                kind,
                open: true,
                ..
            }) if can_close => self.toggle_group(schema, kind, cx),
            Some(Row::Table {
                key,
                expanded: true,
                ..
            }) => self.toggle_table(key, cx),
            Some(Row::Table { key, .. }) => self.set_cursor(Target::Schema(key.schema), cx),
            Some(Row::Group { schema, .. }) => self.set_cursor(Target::Schema(schema), cx),
            _ => {}
        }
    }

    /// Right arrow: opens what is closed; on an open schema, moves into it.
    fn expand_cursor(&mut self, cx: &mut Context<Self>) {
        match self.cursor_row(cx) {
            Some(Row::Schema {
                name,
                collapsed: true,
                ..
            }) => self.toggle_schema(&name, cx),
            Some(Row::Schema { .. }) => self.move_cursor(1, cx),
            Some(Row::Table {
                key,
                expanded: false,
                ..
            }) => self.toggle_table(key, cx),
            Some(Row::Group {
                schema,
                kind,
                open: false,
                ..
            }) => self.toggle_group(schema, kind, cx),
            _ => {}
        }
    }

    /// Enter: shows a table's rows, and opens or closes anything else.
    fn open_cursor(&mut self, cx: &mut Context<Self>) {
        match self.cursor_row(cx) {
            Some(Row::Schema { name, .. }) => self.toggle_schema(&name, cx),
            Some(Row::Table { key, .. }) => self.preview(&key, cx),
            Some(Row::Group { schema, kind, .. }) => self.toggle_group(schema, kind, cx),
            _ => {}
        }
    }

    fn render_row(&self, row: &Row, is_cursor: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, hover, radius) = (theme.muted_foreground, theme.accent, theme.radius);
        let border = if is_cursor {
            theme.ring
        } else {
            transparent_black()
        };
        let line = || {
            h_flex()
                .h(px(ROW_HEIGHT))
                .gap_1p5()
                .pr_1()
                .border_1()
                .border_color(border)
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
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.take_cursor(Target::Schema(name.clone()), window, cx);
                    this.toggle_schema(&name, cx);
                }))
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
                            .flex_none()
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .child(
                                Button::new(SharedString::from(format!("preview-{id}")))
                                    .cursor_pointer()
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Play)
                                    .tooltip("Show the first 100 rows")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        let target = Target::Table(preview_key.clone());
                                        this.take_cursor(target, window, cx);
                                        this.preview(&preview_key, cx);
                                    })),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.take_cursor(Target::Table(key.clone()), window, cx);
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
            Row::Group {
                schema,
                kind,
                count,
                open,
            } => line()
                .id(SharedString::from(format!(
                    "group-{schema}-{}",
                    kind.plural()
                )))
                .test_support()
                .pl(px(14.))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(chevron(open))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(muted)
                        .child(kind.plural()),
                )
                .child(div().text_xs().text_color(muted).child(count.to_string()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.take_cursor(Target::Group(schema.clone(), kind), window, cx);
                    this.toggle_group(schema.clone(), kind, cx);
                }))
                .into_any_element(),
            Row::Object { name, detail } => line()
                .pl(px(34.))
                .child(div().flex_1().min_w_0().truncate().text_xs().child(name))
                .child(
                    div()
                        .max_w(px(120.))
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(detail),
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
        let filter = self.filter_text(cx);
        let rows = Rc::new(self.rows(cx));
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let message = match self.load {
            Load::Idle => Some(""),
            Load::Loading if self.tables.is_empty() => Some("Loading…"),
            Load::Failed => Some("Could not load this database."),
            _ if rows.is_empty() && filter.trim().is_empty() => Some("Nothing here yet."),
            _ if rows.is_empty() => Some("Nothing matches."),
            _ => None,
        };
        v_flex()
            .size_full()
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
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip("Reload")
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
                        cx.processor(move |this, range: Range<usize>, window, cx| {
                            rows[range]
                                .iter()
                                .map(|row| this.render_row(row, this.is_cursor(row, window), cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_focus(&self.focus)
                    .key_context(tree_nav::CONTEXT)
                    .on_action(cx.listener(|this, _: &TreeUp, _, cx| this.move_cursor(-1, cx)))
                    .on_action(cx.listener(|this, _: &TreeDown, _, cx| this.move_cursor(1, cx)))
                    .on_action(
                        cx.listener(|this, _: &TreeCollapse, _, cx| this.collapse_cursor(cx)),
                    )
                    .on_action(cx.listener(|this, _: &TreeExpand, _, cx| this.expand_cursor(cx)))
                    .on_action(cx.listener(|this, _: &TreeOpen, _, cx| this.open_cursor(cx)))
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
    use super::{
        ColumnInfo, Columns, HashMap, HashSet, ObjectInfo, ObjectKind, Row, TableInfo, TableKey,
        flatten,
    };

    fn plain_flatten(
        tables: &[TableInfo],
        filter: &str,
        collapsed: &HashSet<String>,
        expanded: &HashMap<TableKey, Columns>,
    ) -> Vec<Row> {
        flatten(tables, &[], filter, collapsed, &HashSet::new(), expanded)
    }

    fn object(schema: &str, kind: ObjectKind, name: &str) -> ObjectInfo {
        ObjectInfo {
            schema: schema.into(),
            kind,
            name: name.into(),
            detail: String::new(),
        }
    }

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
                Row::Group { kind, count, .. } => format!("{}({count})", kind.plural()),
                Row::Object { name, .. } => format!("  {name}"),
                Row::Note(text) => format!("  {text}"),
            })
            .collect()
    }

    #[test]
    fn tables_are_grouped_by_schema() {
        let rows = plain_flatten(&sample(), "", &HashSet::new(), &HashMap::new());
        assert_eq!(
            names(&rows),
            ["public(2)", "orders", "users", "reports(1)", "daily"]
        );
    }

    #[test]
    fn views_are_marked() {
        let rows = plain_flatten(&sample(), "daily", &HashSet::new(), &HashMap::new());
        assert!(matches!(rows[1], Row::Table { is_view: true, .. }));
    }

    #[test]
    fn collapsed_schema_hides_its_tables() {
        let collapsed = HashSet::from(["public".to_string()]);
        let rows = plain_flatten(&sample(), "", &collapsed, &HashMap::new());
        assert_eq!(names(&rows), ["public(2)", "reports(1)", "daily"]);
    }

    #[test]
    fn filter_ignores_case_and_shows_matches_in_collapsed_schemas() {
        let collapsed = HashSet::from(["public".to_string()]);
        let rows = plain_flatten(&sample(), " USE ", &collapsed, &HashMap::new());
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
        let rows = plain_flatten(&sample(), "", &HashSet::new(), &expanded);
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
    fn groups_follow_the_tables_and_start_closed() {
        let objects = [
            object("public", ObjectKind::Sequence, "ids"),
            object("public", ObjectKind::Function, "now2"),
        ];
        let rows = flatten(
            &sample(),
            &objects,
            "",
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
        );
        assert_eq!(
            names(&rows),
            [
                "public(2)",
                "orders",
                "users",
                "Functions(1)",
                "Sequences(1)",
                "reports(1)",
                "daily"
            ]
        );
    }

    #[test]
    fn an_open_group_lists_its_objects() {
        let objects = [object("public", ObjectKind::Function, "now2")];
        let open = HashSet::from([("public".to_string(), ObjectKind::Function)]);
        let rows = flatten(&[], &objects, "", &HashSet::new(), &open, &HashMap::new());
        assert_eq!(names(&rows), ["public(0)", "Functions(1)", "  now2"]);
    }

    #[test]
    fn the_filter_finds_objects_inside_closed_groups() {
        let objects = [
            object("public", ObjectKind::Function, "now2"),
            object("public", ObjectKind::Function, "other"),
        ];
        let rows = flatten(
            &sample(),
            &objects,
            "NOW",
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::new(),
        );
        assert_eq!(names(&rows), ["public(0)", "Functions(1)", "  now2"]);
    }

    #[test]
    fn a_collapsed_schema_hides_its_groups() {
        let objects = [object("public", ObjectKind::Function, "now2")];
        let collapsed = HashSet::from(["public".to_string()]);
        let rows = flatten(
            &sample(),
            &objects,
            "",
            &collapsed,
            &HashSet::new(),
            &HashMap::new(),
        );
        assert_eq!(names(&rows), ["public(2)", "reports(1)", "daily"]);
    }

    #[test]
    fn no_tables_means_no_rows() {
        assert!(plain_flatten(&[], "", &HashSet::new(), &HashMap::new()).is_empty());
    }
}
