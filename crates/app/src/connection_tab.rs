//! One open connection: its SQL editor, schema browser, results and status line.
//!
//! The workspace keeps one tab per open connection and writes what outlives the window
//! (drafts, history, panel sizes) from the events a tab sends it.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::resizable::{
    ResizablePanel, ResizableState, h_resizable, resizable_panel, v_resizable,
};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::Connection;
use tuplebase_core::db::{Db, ExecOptions, ROLLED_BACK_NOTE, ScriptResult};
use tuplebase_core::safety;

use crate::completions::{self, SchemaNames, SqlCompletions};
use crate::results::ResultsDelegate;
use crate::row_detail::{RowDetail, RowDetailEvent};
use crate::run_button::{RunButton, RunStatement};
use crate::runtime::io;
use crate::schema::{SchemaEvent, SchemaPanel};
use crate::workspace::{Panel, RUN_KEY};

#[cfg(target_os = "macos")]
const RUN_HINT: &str = "Run the selection or the statement at the cursor (⌘↵). Run all: ⌘⇧↵";
#[cfg(not(target_os = "macos"))]
const RUN_HINT: &str =
    "Run the selection or the statement at the cursor (Ctrl+Enter). Run all: Ctrl+Shift+Enter";
#[cfg(target_os = "macos")]
const NO_STATEMENT: &str = "No statement at the cursor. Select some SQL, or run all with ⌘⇧↵";
#[cfg(not(target_os = "macos"))]
const NO_STATEMENT: &str =
    "No statement at the cursor. Select some SQL, or run all with Ctrl+Shift+Enter";

const EDIT_ICON: &[u8] = include_bytes!("../../../assets/icons/square-pen.svg");
const DELETE_ICON: &[u8] = include_bytes!("../../../assets/icons/trash.svg");
const HISTORY_ICON: &[u8] = include_bytes!("../../../assets/icons/clock.svg");

/// Queries the history menu lists.
pub(crate) const HISTORY_MENU_SIZE: usize = 20;
/// The history menu's width, in ems, and the longest label that fits it.
const HISTORY_MENU_EMS: f32 = 44.;
const HISTORY_LABEL_CHARS: usize = 56;

/// Rows shown per query in the app; agents get their own cap in the MCP server.
const MAX_ROWS: usize = 1000;

/// What the status line says.
pub(crate) enum Status {
    Idle,
    Busy(SharedString),
    Done(SharedString),
    Failed(SharedString),
}

/// The colour of a tab's dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Light {
    Connecting,
    Connected,
    /// Not connected yet, or connecting failed.
    Off,
}

pub(crate) fn light(connecting: bool, connected: bool) -> Light {
    if connecting {
        Light::Connecting
    } else if connected {
        Light::Connected
    } else {
        Light::Off
    }
}

/// What a tab asks of the workspace.
pub(crate) enum TabEvent {
    /// The editor now holds this SQL.
    DraftChanged(String),
    /// This SQL was run: it goes in the connection's history.
    Ran(String),
    /// Panels were dragged to these sizes, by the name each is saved under.
    Resized(Vec<(&'static str, u32)>),
    Edit,
    Delete,
    /// Connecting is done, with why it failed.
    Connected(Result<(), SharedString>),
    /// The connection may have closed: look again at which pools are open.
    ConnectionChanged,
}

pub(crate) struct ConnectionTab {
    db: Arc<Db>,
    conn: Connection,
    pub(crate) editor: Entity<EditorState>,
    run_button: Entity<RunButton>,
    pub(crate) results: Entity<TableState<ResultsDelegate>>,
    pub(crate) row_detail: Entity<RowDetail>,
    pub(crate) schema: Entity<SchemaPanel>,
    pub(crate) status: Status,
    /// Set from asking for a pool until the answer.
    pub(crate) connecting: bool,
    /// Whether the connection has an open pool.
    connected: bool,
    /// The query run whose result is still to show; an earlier one's is dropped.
    pub(crate) run: u64,
    /// The newest queries run on this connection, newest first.
    history: Vec<String>,
    /// The sizes panels were dragged to, by name.
    panels: BTreeMap<String, u32>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TabEvent> for ConnectionTab {}

impl ConnectionTab {
    pub(crate) fn new(
        db: Arc<Db>,
        conn: Connection,
        draft: String,
        history: Vec<String>,
        panels: BTreeMap<String, u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let names = SchemaNames::default();
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(draft);
            state.lsp_mut().completion_provider = Some(Rc::new(SqlCompletions::new(names.clone())));
            state
        });
        let run_button = cx.new(|cx| {
            let mut button = RunButton::new(editor.clone(), cx);
            button.set_adapter(conn.adapter, cx);
            button
        });
        let schema = cx.new(|cx| SchemaPanel::new(db.clone(), names, window, cx));
        let shown = conn.clone();
        schema.update(cx, |schema, cx| schema.set_connection(Some(shown), cx));
        let results = cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx));
        let row_detail = cx.new(|cx| RowDetail::new(window, cx));
        let _subscriptions = vec![
            cx.subscribe(&run_button, |this, _, RunStatement(sql), cx| {
                this.run_sql(sql.clone(), cx);
            }),
            cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.remember_draft(cx);
                    // A definition still loading must not replace what was typed.
                    this.schema.update(cx, |schema, _| schema.forget_lookup());
                }
            }),
            cx.subscribe_in(&schema, window, Self::on_schema_event),
            cx.subscribe_in(
                &results,
                window,
                |this, results, event: &TableEvent, window, cx| {
                    if let TableEvent::SelectRow(row_ix) = event
                        && let Some(row) = results.read(cx).delegate().row(*row_ix)
                    {
                        this.row_detail
                            .update(cx, |detail, cx| detail.show(row, window, cx));
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &row_detail,
                |this, _, event: &RowDetailEvent, cx| match event {
                    RowDetailEvent::Close => this.close_row_detail(cx),
                },
            ),
        ];
        Self {
            db,
            conn,
            editor,
            run_button,
            results,
            row_detail,
            schema,
            status: Status::Idle,
            connecting: false,
            connected: false,
            run: 0,
            history,
            panels,
            _subscriptions,
        }
    }

    fn on_schema_event(
        &mut self,
        _: &Entity<SchemaPanel>,
        event: &SchemaEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Ignored while a query runs: it would replace the SQL and not run.
            SchemaEvent::Preview(_) if !self.can_run() => {}
            SchemaEvent::Preview(sql) => {
                self.editor
                    .update(cx, |editor, cx| editor.set_value(sql.clone(), window, cx));
                self.remember_draft(cx);
                self.run_sql(sql.clone(), cx);
            }
            // Not run: the user reads it, fills it in, and presses Run.
            SchemaEvent::Insert(sql) => {
                self.editor
                    .update(cx, |editor, cx| editor.set_value(sql.clone(), window, cx));
                self.remember_draft(cx);
                // An earlier error is about SQL the editor no longer shows.
                if matches!(self.status, Status::Failed(_)) {
                    self.status = Status::Idle;
                    cx.notify();
                }
            }
            // A running query's status stays until its result arrives.
            SchemaEvent::Failed(message) => {
                if !matches!(self.status, Status::Busy(_)) {
                    self.status = Status::Failed(message.clone().into());
                    cx.notify();
                }
            }
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.conn.name
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Takes the connection's settings as they now are in the config, such as a new
    /// folder or name. What is open stays as it is.
    pub(crate) fn set_connection(&mut self, conn: Connection, cx: &mut Context<Self>) {
        let adapter = conn.adapter;
        self.run_button
            .update(cx, |button, cx| button.set_adapter(adapter, cx));
        self.conn = conn;
        cx.notify();
    }

    /// Starts again with `conn`, such as after its settings were edited: the rows and
    /// tables shown so far go, and it connects anew.
    pub(crate) fn reconnect(&mut self, conn: Connection, cx: &mut Context<Self>) {
        self.set_connection(conn.clone(), cx);
        self.close_row_detail(cx);
        self.results.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        self.schema
            .update(cx, |schema, cx| schema.set_connection(Some(conn), cx));
        self.connect(cx);
    }

    /// Opens a pool, or checks the one open; the workspace shows the answer.
    pub(crate) fn connect(&mut self, cx: &mut Context<Self>) {
        self.status = Status::Busy(format!("Connecting to {}…", self.conn.name).into());
        self.connecting = true;
        cx.notify();
        let (db, conn) = (self.db.clone(), self.conn.clone());
        cx.spawn(async move |this, cx| {
            let result = io(async move { db.check(&conn).await }).await;
            this.update(cx, |this, cx| {
                this.connecting = false;
                cx.emit(TabEvent::Connected(
                    result.map_err(|e| format!("{e:#}").into()),
                ));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    pub(crate) fn set_connected(&mut self, connected: bool, cx: &mut Context<Self>) {
        if self.connected != connected {
            self.connected = connected;
            cx.notify();
        }
    }

    pub(crate) fn light(&self) -> Light {
        light(self.connecting, self.connected)
    }

    pub(crate) fn set_history(&mut self, history: Vec<String>, cx: &mut Context<Self>) {
        self.history = history;
        cx.notify();
    }

    /// Sizes dragged in another tab, so that every tab lays out alike.
    pub(crate) fn set_panel_sizes(
        &mut self,
        panels: BTreeMap<String, u32>,
        cx: &mut Context<Self>,
    ) {
        if self.panels != panels {
            self.panels = panels;
            cx.notify();
        }
    }

    /// Runs the selected text, or else the statement at the cursor, split as a script
    /// would be.
    pub(crate) fn run_query(&mut self, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let selected = editor.selected_value();
        let sql = if selected.trim().is_empty() {
            let text = editor.value();
            safety::statement_at(self.conn.adapter, &text, editor.cursor())
                .map(|statement| text[statement].to_owned())
        } else {
            Some(selected.to_string())
        };
        match sql {
            Some(sql) => self.run_sql(sql, cx),
            None if self.can_run() => {
                self.status = Status::Done(NO_STATEMENT.into());
                cx.notify();
            }
            None => {}
        }
    }

    /// Runs everything in the editor.
    pub(crate) fn run_all(&mut self, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().to_string();
        self.run_sql(sql, cx);
    }

    /// Whether a query may start: not while one runs.
    fn can_run(&self) -> bool {
        !matches!(self.status, Status::Busy(_))
    }

    /// Runs `sql` as a script and has it recorded in the history.
    pub(crate) fn run_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        if !self.can_run() {
            return;
        }
        cx.emit(TabEvent::Ran(sql.clone()));
        self.status = Status::Busy("Running…".into());
        self.run += 1;
        let run = self.run;
        cx.notify();
        let db = self.db.clone();
        let conn = self.conn.clone();
        let opts = ExecOptions {
            read_only: false,
            max_rows: MAX_ROWS,
        };
        cx.spawn(async move |this, cx| {
            let checked = conn.clone();
            let result = io(async move { db.execute_script(&conn, &sql, opts).await }).await;
            // A tab closed meanwhile is gone, and its result with it.
            this.update(cx, |this, cx| this.finish_query(run, checked, result, cx))
                .ok();
        })
        .detach();
    }

    /// Shows the result of query run `run` on `conn`.
    pub(crate) fn finish_query(
        &mut self,
        run: u64,
        conn: Connection,
        result: anyhow::Result<ScriptResult>,
        cx: &mut Context<Self>,
    ) {
        if result.is_err() {
            self.check_still_connected(conn, cx);
        }
        // An earlier run that finishes late must not replace a later one's.
        if self.run != run {
            return;
        }
        self.status = match &result {
            Ok(run) => {
                let done = summary(&run.result, run.statements);
                // What the run changed in its open transaction was not kept: say so.
                Status::Done(if run.rolled_back {
                    format!("{done} · {ROLLED_BACK_NOTE}").into()
                } else {
                    done.into()
                })
            }
            Err(e) => Status::Failed(format!("{e:#}").into()),
        };
        if let Ok(run) = &result {
            // A statement that changed something may have changed the schema too.
            if run.changed {
                self.schema.update(cx, |schema, cx| schema.refresh(cx));
            }
            self.close_row_detail(cx);
            self.results.update(cx, |table, cx| {
                let font_size = cx.theme().font_size;
                table.delegate_mut().set(&run.result, font_size);
                table.refresh(cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// A failed query can mean the server has gone: if so, stop showing it as connected.
    fn check_still_connected(&self, conn: Connection, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            io(async move {
                db.check(&conn).await.ok();
                Ok(())
            })
            .await
            .ok();
            this.update(cx, |_, cx| cx.emit(TabEvent::ConnectionChanged))
                .ok();
        })
        .detach();
    }

    /// Has what's in the editor kept for this connection.
    fn remember_draft(&mut self, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().to_string();
        cx.emit(TabEvent::DraftChanged(sql));
    }

    /// Closes the row detail panel and drops the selection that opened it.
    pub(crate) fn close_row_detail(&mut self, cx: &mut Context<Self>) {
        self.row_detail.update(cx, |detail, cx| detail.clear(cx));
        self.results
            .update(cx, |table, cx| table.clear_selection(cx));
        cx.notify();
    }

    fn show_history_query(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_value(sql, window, cx));
        self.remember_draft(cx);
        cx.notify();
    }

    fn sized_panel(&self, panel: Panel) -> ResizablePanel {
        panel.sized(&self.panels)
    }

    /// Reports the sizes these panels of one group were dragged to.
    fn remember_panel_sizes(
        &self,
        panels: &'static [Panel],
        cx: &mut Context<Self>,
    ) -> impl Fn(&Entity<ResizableState>, &mut Window, &mut App) + 'static {
        let this = cx.entity().downgrade();
        move |state, _, cx| {
            let sizes = Panel::dragged_sizes(panels, state, cx);
            this.update(cx, |this, cx| {
                for (key, size) in &sizes {
                    this.panels.insert((*key).into(), *size);
                }
                cx.emit(TabEvent::Resized(sizes));
            })
            .ok();
        }
    }
}

/// The status line under the editor, also shown on the empty screen after a failure.
pub(crate) fn render_status(status: &Status, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let (text, color) = match status {
        Status::Idle => (SharedString::default(), theme.muted_foreground),
        Status::Busy(s) | Status::Done(s) => (s.clone(), theme.muted_foreground),
        Status::Failed(s) => (s.clone(), theme.danger),
    };
    div()
        .id("status-line")
        .test_support()
        .flex_none()
        .min_h(px(28.))
        .px_3()
        .py_1()
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(color)
        .child(text)
}

impl Render for ConnectionTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let conn = &self.conn;
        let theme = cx.theme();
        let (border, radius, muted, muted_bg) = (
            theme.border,
            theme.radius,
            theme.muted_foreground,
            theme.muted,
        );
        let busy = matches!(self.status, Status::Busy(_));
        let placeholder = self.results.read(cx).delegate().placeholder();
        let ran = self.results.read(cx).delegate().ran();
        let chip = |text: String, fg: Hsla, bg: Hsla| {
            div()
                .flex_none()
                .px_1p5()
                .rounded(radius)
                .text_xs()
                .font_medium()
                .text_color(fg)
                .bg(bg)
                .child(text)
        };
        let folder_label = (!conn.folder.is_empty()).then(|| conn.folder.replace('/', " / "));
        let editor_and_results = v_resizable("editor-panels")
            .on_resize(self.remember_panel_sizes(&[Panel::Editor], cx))
            .child(
                self.sized_panel(Panel::Editor).child(
                    completions::handle_keys(div().id("sql-editor"), &self.editor, window)
                        .test_support()
                        .size_full()
                        .child(Editor::new(&self.editor).appearance(false).h_full())
                        .child(self.run_button.clone()),
                ),
            )
            .child(
                resizable_panel().child(
                    h_resizable("results-panels")
                        .on_resize(self.remember_panel_sizes(&[Panel::RowDetail], cx))
                        .child(
                            resizable_panel().child(
                                div().size_full().min_w_0().child(match placeholder {
                                    Some(text) => v_flex()
                                        .size_full()
                                        .justify_center()
                                        .items_center()
                                        .gap_2()
                                        .text_sm()
                                        .text_color(muted)
                                        .child(text)
                                        .when(!ran, |el| {
                                            el.children(Keystroke::parse(RUN_KEY).ok().map(|key| {
                                                h_flex()
                                                    .gap_1p5()
                                                    .text_xs()
                                                    .child("Press")
                                                    .child(gpui_kit::component::kbd::Kbd::new(key))
                                                    .child("to run")
                                            }))
                                        })
                                        .into_any_element(),
                                    None => DataTable::new(&self.results)
                                        .small()
                                        .bordered(false)
                                        .into_any_element(),
                                }),
                            ),
                        )
                        .when(self.row_detail.read(cx).is_open(), |el| {
                            el.child(
                                self.sized_panel(Panel::RowDetail)
                                    .child(self.row_detail.clone()),
                            )
                        }),
                ),
            );
        let history = self.history.clone();
        let this = cx.entity().downgrade();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(40.))
                    .flex_none()
                    .pl_3()
                    .pr_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        h_flex()
                            .id("breadcrumb")
                            .test_support()
                            .min_w_0()
                            .gap_1p5()
                            .text_sm()
                            .when_some(folder_label, |el, folders| {
                                el.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(muted)
                                        .child(format!("{folders} /")),
                                )
                            })
                            .child(div().flex_none().font_semibold().child(conn.name.clone())),
                    )
                    .when(conn.readonly, |el| {
                        el.child(chip("Read-only".into(), muted, muted_bg))
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("history")
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .icon(Icon::default().data(HISTORY_ICON))
                            .tooltip("Query history")
                            .dropdown_menu(move |mut menu, _, cx| {
                                // Wide enough for a label at any text size; the menu
                                // doesn't cut a label that is longer than it is.
                                menu = menu.max_w(cx.theme().font_size * HISTORY_MENU_EMS);
                                if history.is_empty() {
                                    menu = menu.label("No queries yet");
                                }
                                for sql in &history {
                                    let (this, query) = (this.clone(), sql.clone());
                                    menu =
                                        menu.item(PopupMenuItem::new(history_label(sql)).on_click(
                                            move |_, window, cx| {
                                                let query = query.clone();
                                                this.update(cx, |this, cx| {
                                                    this.show_history_query(query, window, cx);
                                                })
                                                .ok();
                                            },
                                        ));
                                }
                                menu
                            }),
                    )
                    .child(
                        Button::new("edit")
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .icon(Icon::default().data(EDIT_ICON))
                            .tooltip("Edit connection")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(TabEvent::Edit))),
                    )
                    .child(
                        Button::new("delete")
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .icon(Icon::default().data(DELETE_ICON))
                            .tooltip("Delete connection")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(TabEvent::Delete))),
                    )
                    .child(
                        Button::new("run")
                            .cursor_pointer()
                            .primary()
                            .small()
                            .icon(IconName::Play)
                            .label("Run")
                            .tooltip(RUN_HINT)
                            .loading(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.run_query(cx))),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("query-panels")
                        .on_resize(self.remember_panel_sizes(&[Panel::Schema], cx))
                        .child(self.sized_panel(Panel::Schema).child(self.schema.clone()))
                        .child(
                            resizable_panel().child(
                                v_flex()
                                    .size_full()
                                    .min_w_0()
                                    .child(div().flex_1().min_h_0().child(editor_and_results))
                                    .child(render_status(&self.status, cx)),
                            ),
                        ),
                ),
            )
    }
}

fn rows_text(count: usize) -> String {
    if count == 1 {
        "1 row".to_owned()
    } else {
        format!("{count} rows")
    }
}

pub(crate) fn summary(r: &tuplebase_core::db::QueryResult, statements: usize) -> String {
    let ms = r.elapsed_ms;
    let outcome = match r.rows_affected {
        Some(n) => format!("{} changed in {ms} ms", rows_text(n as usize)),
        None if r.truncated => format!("First {} in {ms} ms", rows_text(r.rows.len())),
        None => format!("{} in {ms} ms", rows_text(r.rows.len())),
    };
    if statements > 1 {
        format!("{statements} statements. {outcome}")
    } else {
        outcome
    }
}

/// One line of a query, short enough for a menu.
fn history_label(sql: &str) -> String {
    let flat = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(HISTORY_LABEL_CHARS) {
        Some((end, _)) => format!("{}…", &flat[..end]),
        None => flat,
    }
}

#[cfg(test)]
mod tests {
    // No globs from gpui_kit here: its `test` macro would shadow the built-in `#[test]`.
    use super::{ConnectionTab, HISTORY_LABEL_CHARS, Status, history_label, summary};
    use crate::workspace::Workspace;
    use crate::workspace::tests::{config_with_connection, open, open_tab, window_action};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, point, px};
    use serde_json::json;
    use std::rc::Rc;
    use tuplebase_core::db::{ObjectInfo, ObjectKind, QueryResult, ROLLED_BACK_NOTE};

    /// The test connection's tab, open and done connecting, with "SELECT 1;" in its
    /// editor.
    fn open_db(
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>, Entity<ConnectionTab>) {
        let (handle, workspace) = open(config_with_connection(), cx);
        let tab = open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            let editor = tab.read(cx).editor.clone();
            editor.update(cx, |editor, cx| editor.set_value("SELECT 1;", window, cx));
        });
        (handle, workspace, tab)
    }

    const SCRIPT: &str = "select 1;\nselect 2;\nselect 3;";

    #[gpui_kit::test]
    fn choosing_a_result_row_shows_its_fields_until_closed(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.results.update(cx, |table, cx| {
                    table.delegate_mut().show(&QueryResult {
                        columns: vec!["id".into(), "name".into()],
                        rows: vec![vec![json!(1), json!("Ada")], vec![json!(2), json!(null)]],
                        truncated: false,
                        rows_affected: None,
                        elapsed_ms: 1,
                        note: None,
                    });
                    table.refresh(cx);
                });
                cx.notify();
            });
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("row-detail").is_none());
            tab.update(cx, |this, cx| {
                this.results
                    .update(cx, |table, cx| table.set_selected_row(0, cx));
            });
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("row-detail").visible());
            assert_eq!(
                tab.read(cx).row_detail.read(cx).shown(cx),
                "{\n  id: 1\n  name: \"Ada\"\n}"
            );
            // Its text can be selected and copied like any editor's.
            let text = tab.read(cx).row_detail.read(cx).text().clone();
            text.update(cx, |state, cx| state.focus(window, cx));
            window.dispatch_action(Box::new(gpui_kit::component::input::SelectAll), cx);
            window.dispatch_action(Box::new(gpui_kit::component::input::Copy), cx);
        });
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("{\n  id: 1\n  name: \"Ada\"\n}".to_owned())
        );
        window_action(handle, cx, |window, cx| {
            window.click("row-detail-close", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("row-detail").is_none());
            assert_eq!(tab.read(cx).results.read(cx).selected_row(), None);
        });
    }

    #[gpui_kit::test]
    fn result_cells_and_rows_can_be_copied(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.results.update(cx, |table, cx| {
                    table.delegate_mut().show(&QueryResult {
                        columns: vec!["id".into(), "name".into()],
                        rows: vec![
                            vec![json!(1), json!("say \"hi\"")],
                            vec![json!(2), json!(null)],
                        ],
                        truncated: false,
                        rows_affected: None,
                        elapsed_ms: 1,
                        note: None,
                    });
                    table.refresh(cx);
                });
                cx.notify();
            });
        });
        let copied = |cx: &mut TestAppContext| {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default()
        };
        let row_0 = "{\n  \"id\": 1,\n  \"name\": \"say \\\"hi\\\"\"\n}";

        // Right click a cell: "Copy value", then "Copy row as JSON".
        window_action(handle, cx, |window, cx| {
            window.right_click("table-cell-0:1", cx)
        });
        window_action(handle, cx, |window, cx| window.click(0usize, cx));
        assert_eq!(copied(cx), "say \"hi\"");
        window_action(handle, cx, |window, cx| {
            window.right_click("table-cell-1:1", cx)
        });
        window_action(handle, cx, |window, cx| window.click(0usize, cx));
        assert_eq!(copied(cx), "NULL");
        window_action(handle, cx, |window, cx| {
            window.right_click("table-cell-0:0", cx)
        });
        window_action(handle, cx, |window, cx| window.click(1usize, cx));
        assert_eq!(copied(cx), row_0);

        // The row detail panel copies the whole row.
        window_action(handle, cx, |_, cx| {
            tab.update(cx, |this, cx| {
                this.results
                    .update(cx, |table, cx| table.set_selected_row(0, cx));
            });
        });
        window_action(handle, cx, |window, cx| window.click("row-detail-copy", cx));
        assert_eq!(copied(cx), row_0);
    }

    #[gpui_kit::test]
    fn a_short_result_shows_no_blank_rows(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                let result = QueryResult {
                    columns: vec!["id".into()],
                    rows: vec![vec![json!(1)], vec![json!(2)]],
                    truncated: false,
                    rows_affected: None,
                    elapsed_ms: 1,
                    note: None,
                };
                this.results.update(cx, |table, cx| {
                    table.delegate_mut().show(&result);
                    table.refresh(cx);
                });
                cx.notify();
            });
        });
        window_action(handle, cx, |_, _| {});
        window_action(handle, cx, |window, _| {
            assert!(window.try_find(("row", 1usize)).is_some());
            assert!(window.try_find(("row", 2usize)).is_none());
        });
    }

    #[test]
    fn summary_reads_naturally() {
        let result = |rows: usize, affected, truncated| QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new(); rows],
            truncated,
            rows_affected: affected,
            elapsed_ms: 5,
            note: None,
        };
        assert_eq!(summary(&result(1, None, false), 1), "1 row in 5 ms");
        assert_eq!(summary(&result(2, None, true), 1), "First 2 rows in 5 ms");
        assert_eq!(
            summary(&result(0, Some(3), false), 1),
            "3 rows changed in 5 ms"
        );
    }

    #[test]
    fn summary_counts_statements_and_labels_stay_short() {
        let result = QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new(); 2],
            truncated: false,
            rows_affected: None,
            elapsed_ms: 5,
            note: None,
        };
        assert_eq!(summary(&result, 3), "3 statements. 2 rows in 5 ms");
        assert_eq!(history_label("select\n  1"), "select 1");
        assert_eq!(
            history_label(&"x".repeat(100)).chars().count(),
            HISTORY_LABEL_CHARS + 1
        );
    }

    /// Opens the editor of a connection, holding `text`.
    fn open_editor(
        text: &str,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>, Entity<ConnectionTab>) {
        let (handle, workspace, tab) = open_db(cx);
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.press("secondary-a", cx);
            window.input(text, cx);
        });
        (handle, workspace, tab)
    }

    fn menu_open(tab: &Entity<ConnectionTab>, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            let editor = tab.read(cx).editor.read(cx);
            editor.completion_menu_state().open
        })
    }

    #[gpui_kit::test]
    fn tab_takes_the_suggestion(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_editor("SELE", cx);
        assert!(menu_open(&tab, cx));
        window_action(handle, cx, |window, cx| window.press("tab", cx));
        assert_eq!(editor_text(&tab, cx), "SELECT");
        assert!(!menu_open(&tab, cx));
        // With no menu, Tab indents as before.
        window_action(handle, cx, |window, cx| window.press("tab", cx));
        let text = editor_text(&tab, cx);
        assert!(
            text.starts_with("SELECT") && text.len() > "SELECT".len(),
            "{text:?}"
        );
    }

    #[gpui_kit::test]
    fn enter_starts_a_new_line_even_with_the_menu_open(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_editor("SELE", cx);
        assert!(menu_open(&tab, cx));
        window_action(handle, cx, |window, cx| window.press("enter", cx));
        assert_eq!(editor_text(&tab, cx), "SELE\n");
        assert!(!menu_open(&tab, cx));
    }

    #[gpui_kit::test]
    fn the_menu_waits_for_a_second_letter_or_ctrl_space(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_editor("S", cx);
        assert!(!menu_open(&tab, cx));
        window_action(handle, cx, |window, cx| window.press("ctrl-space", cx));
        assert!(menu_open(&tab, cx));
        window_action(handle, cx, |window, cx| window.press("escape", cx));
        assert!(!menu_open(&tab, cx));
        assert_eq!(editor_text(&tab, cx), "S");
    }

    /// Puts the cursor at the first `at` in the editor, or selects it.
    fn place(
        handle: AnyWindowHandle,
        tab: &Entity<ConnectionTab>,
        at: &str,
        select: bool,
        cx: &mut TestAppContext,
    ) {
        window_action(handle, cx, |_, cx| {
            let editor = tab.read(cx).editor.clone();
            editor.update(cx, |editor, cx| {
                let start = editor.value().find(at).expect("text to place at");
                let end = if select { start + at.len() } else { start };
                editor.set_selected_range(start..end, cx);
            });
        });
        // The button follows the editor's layout, which the next frame paints.
        window_action(handle, cx, |_, _| ());
    }

    fn last_run(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<String> {
        cx.update(|cx| {
            let session = workspace.read(cx).session();
            session.history_for("db").next().map(str::to_owned)
        })
    }

    #[gpui_kit::test]
    fn run_key_runs_the_statement_at_the_cursor(cx: &mut TestAppContext) {
        let (handle, workspace, tab) = open_editor(SCRIPT, cx);
        place(handle, &tab, "2", false, cx);
        window_action(handle, cx, |window, cx| window.press("secondary-enter", cx));
        assert_eq!(last_run(&workspace, cx).as_deref(), Some("select 2"));
        // The key runs; it never adds a line.
        assert_eq!(editor_text(&tab, cx), SCRIPT);
    }

    #[gpui_kit::test]
    fn run_key_runs_the_selection(cx: &mut TestAppContext) {
        let (handle, workspace, tab) = open_editor(SCRIPT, cx);
        place(handle, &tab, "2;\nselect 3", true, cx);
        window_action(handle, cx, |window, cx| window.press("secondary-enter", cx));
        assert_eq!(last_run(&workspace, cx).as_deref(), Some("2;\nselect 3"));
    }

    #[gpui_kit::test]
    fn run_all_key_runs_everything(cx: &mut TestAppContext) {
        let (handle, workspace, tab) = open_editor(SCRIPT, cx);
        place(handle, &tab, "2", false, cx);
        window_action(handle, cx, |window, cx| {
            window.press("secondary-shift-enter", cx)
        });
        assert_eq!(last_run(&workspace, cx).as_deref(), Some(SCRIPT));
    }

    #[gpui_kit::test]
    fn run_key_with_no_statement_at_the_cursor_says_so(cx: &mut TestAppContext) {
        let (handle, workspace, tab) = open_editor("select 1;\n\n", cx);
        window_action(handle, cx, |window, cx| window.press("secondary-enter", cx));
        assert_eq!(last_run(&workspace, cx), None);
        cx.update(|cx| {
            let status = &tab.read(cx).status;
            assert!(matches!(status, Status::Done(s) if s.starts_with("No statement")));
        });
    }

    #[gpui_kit::test]
    fn the_run_button_runs_its_statement(cx: &mut TestAppContext) {
        let (handle, workspace, tab) = open_editor(SCRIPT, cx);
        place(handle, &tab, "3", false, cx);
        window_action(handle, cx, |window, cx| window.click("run-statement", cx));
        assert_eq!(last_run(&workspace, cx).as_deref(), Some("select 3"));
    }

    #[gpui_kit::test]
    fn the_run_button_shows_only_at_a_statement(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_editor("select 1;\n\n-- the end", cx);
        place(handle, &tab, "1", false, cx);
        window_action(handle, cx, |window, _| {
            assert!(window.find("run-statement").visible());
        });
        place(handle, &tab, "the end", false, cx);
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("run-statement").is_none());
        });
    }

    #[gpui_kit::test]
    fn panels_open_at_their_saved_size(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        workspace.update(cx, |this, _| {
            this.session_mut().panels.insert("editor".into(), 400);
        });
        open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, _| {
            assert_eq!(window.find("sql-editor").bounds().size.height, px(400.));
        });
    }

    #[gpui_kit::test]
    fn dragging_below_the_editor_resizes_it(cx: &mut TestAppContext) {
        let (handle, workspace, _) = open_db(cx);
        let editor = window_action(handle, cx, |window, _| window.find("sql-editor").bounds());
        let edge = point(editor.center().x, editor.bottom());
        window_action(handle, cx, |window, cx| {
            window.drag(edge, edge + point(px(0.), px(60.)), cx);
        });
        window_action(handle, cx, |window, _| {
            let height = window.find("sql-editor").bounds().size.height;
            assert_eq!(height, editor.size.height + px(60.));
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).session().panels["editor"], 280));
    }

    #[gpui_kit::test]
    fn a_filter_keeps_schemas_from_folding_unseen(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_tables(&tab, &["users"], cx);
        window_action(handle, cx, |window, cx| {
            window.click("schema-filter", cx);
            window.input("us", cx);
        });
        window_action(handle, cx, |window, cx| window.click("schema-public", cx));
        window_action(handle, cx, |window, cx| {
            window.click("schema-filter", cx);
            window.press("backspace", cx);
            window.press("backspace", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("table-public-users").visible());
        });
    }

    /// Opens the test connection with these tables of `public` in the schema panel.
    fn show_tables(tab: &Entity<ConnectionTab>, tables: &[&str], cx: &mut TestAppContext) {
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                let conn = this.connection().clone();
                this.schema.update(cx, |schema, cx| {
                    schema.set_connection(Some(conn), cx);
                    schema.tables = tables
                        .iter()
                        .map(|name| tuplebase_core::db::TableInfo {
                            schema: "public".into(),
                            name: (*name).into(),
                            kind: "BASE TABLE".into(),
                        })
                        .collect();
                    schema.load = crate::schema::Load::Ready;
                    cx.notify();
                });
                cx.notify();
            });
        });
    }

    #[gpui_kit::test]
    fn arrow_keys_walk_the_schema_tree_once_it_is_clicked(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_tables(&tab, &["orders", "users"], cx);
        let schema_cursor = |cx: &mut TestAppContext| {
            tab.read_with(cx, |this, cx| this.schema.read(cx).cursor_name())
        };
        let press = |key: &str, cx: &mut TestAppContext| {
            window_action(handle, cx, |window, cx| window.press(key, cx));
        };

        window_action(handle, cx, |window, cx| window.click("schema-public", cx));
        assert_eq!(schema_cursor(cx).as_deref(), Some("public"));
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("table-public-orders").is_none());
        });

        press("right", cx);
        press("down", cx);
        press("down", cx);
        press("down", cx);
        assert_eq!(schema_cursor(cx).as_deref(), Some("users"));
        press("up", cx);
        assert_eq!(schema_cursor(cx).as_deref(), Some("orders"));

        press("right", cx);
        cx.update(|cx| assert_eq!(tab.read(cx).schema.read(cx).expanded.len(), 1));
        press("left", cx);
        cx.update(|cx| assert!(tab.read(cx).schema.read(cx).expanded.is_empty()));
        press("left", cx);
        assert_eq!(schema_cursor(cx).as_deref(), Some("public"));
    }

    #[gpui_kit::test]
    fn enter_on_a_table_shows_its_rows(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_tables(&tab, &["users"], cx);
        window_action(handle, cx, |window, cx| {
            window.click("table-public-users", cx);
            window.press("left", cx);
            window.press("enter", cx);
        });
        cx.update(|cx| {
            let this = tab.read(cx);
            assert_eq!(
                this.editor.read(cx).value().as_str(),
                r#"SELECT * FROM "public"."users" LIMIT 100"#
            );
        });
    }

    /// Shows these objects of `public` in the schema panel. Their lookups answer at once,
    /// naming what was asked for, except for an object named `broken`.
    fn show_objects(
        tab: &Entity<ConnectionTab>,
        objects: &[(ObjectKind, &str)],
        cx: &mut TestAppContext,
    ) {
        show_tables(tab, &[], cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.schema.update(cx, |schema, cx| {
                    schema.objects = objects
                        .iter()
                        .map(|(kind, name)| ObjectInfo {
                            schema: "public".into(),
                            kind: *kind,
                            name: (*name).into(),
                            detail: String::new(),
                        })
                        .collect();
                    schema.fetch = Rc::new(|_, object, lookup| {
                        Box::pin(async move {
                            if object.name == "broken" {
                                anyhow::bail!("no privilege");
                            }
                            Ok(format!("{lookup:?} of {}", object.name))
                        })
                    });
                    cx.notify();
                });
            });
        });
    }

    fn editor_text(tab: &Entity<ConnectionTab>, cx: &mut TestAppContext) -> String {
        cx.update(|cx| tab.read(cx).editor.read(cx).value().to_string())
    }

    #[gpui_kit::test]
    fn clicking_a_procedure_puts_its_definition_in_the_editor_without_running_it(
        cx: &mut TestAppContext,
    ) {
        let (handle, _, tab) = open_db(cx);
        show_objects(&tab, &[(ObjectKind::Procedure, "start_job")], cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Procedures", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("object-public-Procedures-start_job", cx)
        });
        assert_eq!(editor_text(&tab, cx), "Definition of start_job");
        cx.update(|cx| assert!(matches!(tab.read(cx).status, Status::Idle)));
    }

    #[gpui_kit::test]
    fn right_clicking_a_procedure_inserts_a_call(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_objects(&tab, &[(ObjectKind::Procedure, "start_job")], cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Procedures", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.right_click("object-public-Procedures-start_job", cx)
        });
        window_action(handle, cx, |window, cx| window.click(1usize, cx));
        assert_eq!(editor_text(&tab, cx), "Call of start_job");
        cx.update(|cx| assert!(matches!(tab.read(cx).status, Status::Idle)));
    }

    #[gpui_kit::test]
    fn enter_on_a_function_shows_its_definition(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_objects(&tab, &[(ObjectKind::Function, "total")], cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Functions", cx);
            window.press("down", cx);
            window.press("enter", cx);
        });
        assert_eq!(editor_text(&tab, cx), "Definition of total");
        let cursor = |cx: &mut TestAppContext| {
            tab.read_with(cx, |this, cx| this.schema.read(cx).cursor_name())
        };
        assert_eq!(cursor(cx).as_deref(), Some("total"));
        window_action(handle, cx, |window, cx| window.press("left", cx));
        assert_eq!(cursor(cx).as_deref(), Some("Functions"));
    }

    #[gpui_kit::test]
    fn a_definition_that_cannot_be_read_shows_on_the_status_line(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_objects(&tab, &[(ObjectKind::Trigger, "broken")], cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Triggers", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("object-public-Triggers-broken", cx)
        });
        assert_eq!(editor_text(&tab, cx), "SELECT 1;");
        cx.update(|cx| match &tab.read(cx).status {
            Status::Failed(message) => {
                assert!(message.contains("broken"), "{message}");
                assert!(message.contains("no privilege"), "{message}");
            }
            _ => panic!("the error must show on the status line"),
        });

        // A later definition that loads clears the error.
        show_objects(&tab, &[(ObjectKind::Trigger, "fine")], cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Triggers", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("object-public-Triggers-fine", cx)
        });
        assert_eq!(editor_text(&tab, cx), "Definition of fine");
        cx.update(|cx| assert!(matches!(tab.read(cx).status, Status::Idle)));
    }

    #[gpui_kit::test]
    fn objects_without_a_source_are_not_clickable(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_objects(&tab, &[(ObjectKind::Sequence, "ids")], cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Sequences", cx)
        });
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("object-public-Sequences-ids").is_none());
        });
    }

    #[gpui_kit::test]
    fn a_view_shows_its_definition_from_its_row(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_objects(&tab, &[], cx);
        show_tables(&tab, &["daily"], cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.schema.update(cx, |schema, cx| {
                    schema.tables[0].kind = "VIEW".into();
                    cx.notify();
                });
            });
        });
        window_action(handle, cx, |window, cx| {
            window.hover("table-public-daily", cx);
            window.click("definition-table-public-daily", cx);
        });
        assert_eq!(editor_text(&tab, cx), "Definition of daily");
        cx.update(|cx| assert!(tab.read(cx).schema.read(cx).expanded.is_empty()));
    }

    #[gpui_kit::test]
    fn previewing_a_table_runs_its_query(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_tables(&tab, &["users"], cx);
        window_action(handle, cx, |window, cx| {
            window.hover("table-public-users", cx);
            window.click("preview-table-public-users", cx);
        });
        cx.update(|cx| {
            let this = tab.read(cx);
            assert!(this.schema.read(cx).expanded.is_empty());
            assert_eq!(
                this.editor.read(cx).value().as_str(),
                r#"SELECT * FROM "public"."users" LIMIT 100"#
            );
            assert!(matches!(this.status, Status::Busy(_)));
        });
    }

    #[gpui_kit::test]
    fn query_view_renders_results(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.results.update(cx, |table, cx| {
                    table.delegate_mut().show(&QueryResult {
                        columns: vec!["id".into(), "name".into()],
                        rows: vec![vec![json!(1), json!("Ada")], vec![json!(2), json!(null)]],
                        truncated: false,
                        rows_affected: None,
                        elapsed_ms: 1,
                        note: None,
                    });
                    table.refresh(cx);
                });
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("run").visible());
            assert!(window.find("edit").visible());
            assert!(window.find("sql-editor").visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn shortcut_runs_query_without_editing_the_sql(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("sql-editor", cx);
            window.press("secondary-enter", cx);
        })
        .unwrap();
        cx.update(|cx| {
            let this = tab.read(cx);
            assert!(matches!(this.status, Status::Busy(_)));
            assert_eq!(this.editor.read(cx).value().as_str(), "SELECT 1;");
        });
    }

    #[gpui_kit::test]
    fn the_status_line_says_when_an_open_transaction_was_rolled_back(cx: &mut TestAppContext) {
        let (_, _, tab) = open_db(cx);
        let run = |rolled_back| tuplebase_core::db::ScriptResult {
            result: QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                truncated: false,
                rows_affected: Some(1),
                elapsed_ms: 1,
                note: None,
            },
            statements: 2,
            changed: true,
            rolled_back,
        };
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                let conn = this.connection().clone();
                for rolled_back in [true, false] {
                    this.run_query(cx);
                    this.finish_query(this.run, conn.clone(), Ok(run(rolled_back)), cx);
                    let Status::Done(text) = &this.status else {
                        panic!("not done");
                    };
                    assert!(text.starts_with(&summary(&run(false).result, 2)), "{text}");
                    assert_eq!(text.contains(ROLLED_BACK_NOTE), rolled_back, "{text}");
                }
            });
        });
    }

    #[gpui_kit::test]
    fn an_earlier_query_finishing_late_does_not_replace_a_later_one(cx: &mut TestAppContext) {
        let (_, _, tab) = open_db(cx);
        let result = |rows: usize| tuplebase_core::db::ScriptResult {
            result: QueryResult {
                columns: vec!["n".into()],
                rows: (0..rows).map(|n| vec![json!(n)]).collect(),
                truncated: false,
                rows_affected: None,
                elapsed_ms: 1,
                note: None,
            },
            statements: 1,
            changed: false,
            rolled_back: false,
        };
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.run_query(cx);
                let earlier = this.run;
                // Something else, such as a save, may have changed the status meanwhile.
                this.status = Status::Idle;
                this.run_query(cx);
                let later = this.run;
                let conn = this.connection().clone();
                this.finish_query(later, conn.clone(), Ok(result(2)), cx);
                this.finish_query(earlier, conn, Ok(result(5)), cx);
                let expected = summary(&result(2).result, 1);
                assert!(matches!(&this.status, Status::Done(s) if s.as_ref() == expected));
            });
        });
    }

    #[gpui_kit::test]
    fn an_earlier_schema_load_does_not_replace_a_later_one(cx: &mut TestAppContext) {
        let (_, _, tab) = open_db(cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                let conn = this.connection().clone();
                this.schema.update(cx, |schema, cx| {
                    schema.set_connection(Some(conn), cx);
                    let earlier = schema.loading;
                    schema.refresh(cx);
                    let later = schema.loading;
                    let users = tuplebase_core::db::TableInfo {
                        schema: "public".into(),
                        name: "users".into(),
                        kind: "BASE TABLE".into(),
                    };
                    schema.finish_loading(later, Ok((vec![users], Vec::new(), Vec::new())), cx);
                    schema.finish_loading(earlier, Err(anyhow::anyhow!("timed out")), cx);
                    assert!(schema.load == crate::schema::Load::Ready);
                    assert_eq!(schema.tables.len(), 1);
                });
            });
        });
    }

    #[gpui_kit::test]
    fn previewing_while_a_query_runs_leaves_the_editor_alone(cx: &mut TestAppContext) {
        let (handle, workspace, tab) = open_db(cx);
        show_tables(&tab, &["users"], cx);
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.run_query(cx);
                assert!(matches!(this.status, Status::Busy(_)));
            });
        });
        window_action(handle, cx, |window, cx| {
            window.hover("table-public-users", cx);
            window.click("preview-table-public-users", cx);
        });
        assert_eq!(editor_text(&tab, cx), "SELECT 1;");
        cx.update(|cx| {
            let session = workspace.read(cx).session();
            assert_eq!(session.history_for("db").count(), 1);
            assert!(!session.drafts.contains_key("db"));
        });
    }

    /// Objects whose lookup takes a second when they are named "slow".
    fn show_slow_objects(
        tab: &Entity<ConnectionTab>,
        objects: &[(ObjectKind, &str)],
        cx: &mut TestAppContext,
    ) {
        show_objects(tab, objects, cx);
        let executor = cx.executor();
        cx.update(|cx| {
            tab.update(cx, |this, cx| {
                this.schema.update(cx, |schema, _| {
                    schema.fetch = Rc::new(move |_, object, lookup| {
                        let executor = executor.clone();
                        Box::pin(async move {
                            if object.name == "slow" {
                                executor.timer(std::time::Duration::from_secs(1)).await;
                            }
                            Ok(format!("{lookup:?} of {}", object.name))
                        })
                    });
                });
            });
        });
    }

    fn wait_for_slow_lookups(cx: &mut TestAppContext) {
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn a_slow_definition_does_not_replace_a_later_one(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        let objects = [
            (ObjectKind::Function, "slow"),
            (ObjectKind::Function, "fast"),
        ];
        show_slow_objects(&tab, &objects, cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Functions", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("object-public-Functions-slow", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("object-public-Functions-fast", cx)
        });
        assert_eq!(editor_text(&tab, cx), "Definition of fast");
        wait_for_slow_lookups(cx);
        assert_eq!(editor_text(&tab, cx), "Definition of fast");
    }

    #[gpui_kit::test]
    fn a_slow_definition_does_not_replace_what_was_typed_meanwhile(cx: &mut TestAppContext) {
        let (handle, _, tab) = open_db(cx);
        show_slow_objects(&tab, &[(ObjectKind::Function, "slow")], cx);
        window_action(handle, cx, |window, cx| {
            window.click("group-public-Functions", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("object-public-Functions-slow", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 42", cx);
        });
        let typed = editor_text(&tab, cx);
        assert!(typed.contains("select 42"), "{typed}");
        wait_for_slow_lookups(cx);
        assert_eq!(editor_text(&tab, cx), typed);
    }
}
