//! Main window: connection sidebar, SQL editor, results table.

use std::future::Future;
use std::sync::{Arc, LazyLock};

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{Config, Connection};
use tuplebase_core::db::{Db, ExecOptions};
use tuplebase_core::secrets;

use crate::conn_form::{ConnForm, ConnFormEvent};
use crate::results::ResultsDelegate;

actions!(tuplebase, [RunQuery]);

#[cfg(target_os = "macos")]
const RUN_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
const RUN_KEY: &str = "ctrl-enter";

/// Call after `gpui_kit::init`, so the `Input` binding overrides the editor's own
/// one, which would insert a newline.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(RUN_KEY, RunQuery, Some("Input")),
        KeyBinding::new(RUN_KEY, RunQuery, None),
    ]);
}

/// Rows shown per query in the app; agents get their own cap in the MCP server.
const MAX_ROWS: usize = 1000;

/// sqlx needs Tokio; GPUI has its own executor. All database and keychain I/O runs
/// here, and GPUI tasks await the results.
static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Runtime::new().expect("failed to start the Tokio runtime"));

fn io<T: Send + 'static>(
    fut: impl Future<Output = anyhow::Result<T>> + Send + 'static,
) -> impl Future<Output = anyhow::Result<T>> {
    let handle = RUNTIME.spawn(fut);
    async move { handle.await? }
}

fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> impl Future<Output = anyhow::Result<T>> {
    let handle = RUNTIME.spawn_blocking(f);
    async move { handle.await? }
}

enum Status {
    Idle,
    Busy(SharedString),
    Done(SharedString),
    Failed(SharedString),
}

pub struct Workspace {
    db: Arc<Db>,
    config: Config,
    selected: Option<String>,
    form: Option<Entity<ConnForm>>,
    editor: Entity<EditorState>,
    results: Entity<TableState<ResultsDelegate>>,
    status: Status,
    _form_subscription: Option<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Small local file read once before the first frame.
        let (config, status) = match Config::load() {
            Ok(config) => (config, Status::Idle),
            Err(e) => (Config::default(), Status::Failed(format!("{e:#}").into())),
        };
        Self::with_config(config, status, window, cx)
    }

    fn with_config(
        config: Config,
        status: Status,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            db: Arc::new(Db::new()),
            config,
            selected: None,
            form: None,
            editor: cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("sql")
                    .line_number(true)
                    .default_value("SELECT 1;")
            }),
            results: cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx)),
            status,
            _form_subscription: None,
        }
    }

    fn selected_connection(&self) -> Option<Connection> {
        let name = self.selected.as_deref()?;
        self.config.find(name).cloned()
    }

    fn select(&mut self, name: String, cx: &mut Context<Self>) {
        self.form = None;
        self.selected = Some(name);
        self.results.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        let Some(conn) = self.selected_connection() else {
            return;
        };
        self.status = Status::Busy(format!("Connecting to {}…", conn.name).into());
        cx.notify();
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let name = conn.name.clone();
            let result = io(async move { db.test(&conn).await }).await;
            this.update(cx, |this, cx| {
                this.status = match result {
                    Ok(()) => Status::Done(format!("Connected to {name}").into()),
                    Err(e) => Status::Failed(format!("{e:#}").into()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_form(
        &mut self,
        existing: Option<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = cx.new(|cx| ConnForm::new(existing.as_ref(), window, cx));
        self._form_subscription = Some(cx.subscribe(&form, |this, form, event, cx| match event {
            ConnFormEvent::Save {
                connection,
                password,
                previous,
            } => this.save_connection(
                form,
                connection.clone(),
                password.clone(),
                previous.clone(),
                cx,
            ),
            ConnFormEvent::Cancel => {
                this.form = None;
                cx.notify();
            }
        }));
        self.form = Some(form);
        cx.notify();
    }

    fn save_connection(
        &mut self,
        form: Entity<ConnForm>,
        connection: Connection,
        password: Option<String>,
        previous: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut config = self.config.clone();
        if let Err(e) = config.upsert(connection.clone(), previous.as_deref()) {
            form.update(cx, |form, cx| form.set_error(format!("{e:#}"), cx));
            return;
        }
        let db = self.db.clone();
        let name = connection.name.clone();
        let saved = config.clone();
        cx.spawn(async move |this, cx| {
            let result = blocking(move || {
                saved.save()?;
                let renamed = previous.as_ref().filter(|p| **p != connection.name);
                match (&password, renamed) {
                    (Some(pw), _) => secrets::set_password(&connection.name, pw)?,
                    // Renamed without retyping the password: move the stored one.
                    (None, Some(old)) => {
                        if let Some(pw) = secrets::get_password(old)? {
                            secrets::set_password(&connection.name, &pw)?;
                        }
                    }
                    (None, None) => {}
                }
                if let Some(old) = renamed {
                    secrets::delete_password(old)?;
                }
                Ok(previous)
            })
            .await;
            let forget = match &result {
                Ok(Some(previous)) => Some(previous.clone()),
                _ => None,
            };
            if let Some(previous) = forget {
                io(async move {
                    db.forget(&previous).await;
                    Ok(())
                })
                .await
                .ok();
            }
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.config = config;
                    this.select(name, cx);
                }
                Err(e) => form.update(cx, |form, cx| form.set_error(format!("{e:#}"), cx)),
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.selected_connection() else {
            return;
        };
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .title(format!("Delete “{}”?", conn.name))
                .description("Its saved password is removed from the keychain.")
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.delete_selected(cx)).ok();
                    true
                })
        });
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.selected.take() else {
            return;
        };
        self.config.remove(&name);
        let config = self.config.clone();
        let db = self.db.clone();
        self.status = Status::Idle;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let forget_name = name.clone();
            let result = blocking(move || {
                config.save()?;
                secrets::delete_password(&name)
            })
            .await;
            io(async move {
                db.forget(&forget_name).await;
                Ok(())
            })
            .await
            .ok();
            if let Err(e) = result {
                this.update(cx, |this, cx| {
                    this.status = Status::Failed(format!("{e:#}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn run_query(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.selected_connection() else {
            return;
        };
        if self.form.is_some() || matches!(self.status, Status::Busy(_)) {
            return;
        }
        let sql = self.editor.read(cx).value().to_string();
        self.status = Status::Busy("Running…".into());
        cx.notify();
        let db = self.db.clone();
        let opts = ExecOptions {
            read_only: false,
            max_rows: MAX_ROWS,
        };
        cx.spawn(async move |this, cx| {
            let result = io(async move { db.execute(&conn, &sql, opts).await }).await;
            this.update(cx, |this, cx| {
                this.status = match &result {
                    Ok(r) => Status::Done(summary(r).into()),
                    Err(e) => Status::Failed(format!("{e:#}").into()),
                };
                if let Ok(r) = &result {
                    this.results.update(cx, |table, cx| {
                        table.delegate_mut().set(r);
                        table.refresh(cx);
                        cx.notify();
                    });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (accent, muted, border, bg) = (
            theme.accent,
            theme.muted_foreground,
            theme.border,
            theme.sidebar,
        );
        v_flex()
            .w(px(240.))
            .h_full()
            .flex_none()
            .bg(bg)
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .justify_between()
                    .child(div().font_semibold().child("Connections"))
                    .child(
                        Button::new("add-connection")
                            .ghost()
                            .small()
                            .icon(IconName::Plus)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_form(None, window, cx)),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("connections")
                    .px_2()
                    .gap_1()
                    .overflow_y_scroll()
                    .children(self.config.connections.iter().map(|c| {
                        let name = c.name.clone();
                        let selected = self.selected.as_deref() == Some(c.name.as_str());
                        div()
                            .id(SharedString::from(format!("conn-{}", c.name)))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(accent))
                            .hover(move |s| s.bg(accent))
                            .child(div().child(c.name.clone()))
                            .child(div().text_xs().text_color(muted).child(format!(
                                "{} · {}:{}",
                                c.adapter.label(),
                                c.host,
                                c.port
                            )))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.select(name.clone(), cx)),
                            )
                    })),
            )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (text, color) = match &self.status {
            Status::Idle => (SharedString::default(), theme.muted_foreground),
            Status::Busy(s) | Status::Done(s) => (s.clone(), theme.muted_foreground),
            Status::Failed(s) => (s.clone(), theme.danger),
        };
        div().px_3().py_1().text_sm().text_color(color).child(text)
    }

    fn render_query_view(&self, conn: &Connection, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted) = (theme.border, theme.muted_foreground);
        let busy = matches!(self.status, Status::Busy(_));
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(div().font_semibold().child(conn.name.clone()))
                    .child(div().text_sm().text_color(muted).child(format!(
                        "{}{}",
                        conn.adapter.label(),
                        if conn.readonly { " · read-only" } else { "" }
                    )))
                    .child(div().flex_1())
                    .child(
                        Button::new("edit")
                            .small()
                            .label("Edit")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let existing = this.selected_connection();
                                this.open_form(existing, window, cx);
                            })),
                    )
                    .child(Button::new("delete").small().label("Delete").on_click(
                        cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                    ))
                    .child(
                        Button::new("run")
                            .small()
                            .primary()
                            .label("Run")
                            .loading(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.run_query(cx))),
                    ),
            )
            .child(
                div()
                    .id("sql-editor")
                    .test_support()
                    .h(px(200.))
                    .border_b_1()
                    .border_color(border)
                    .child(Editor::new(&self.editor).h_full()),
            )
            .child(self.render_status(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.results).stripe(true)),
            )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(self.render_status(cx))
            .child(div().text_color(cx.theme().muted_foreground).child(
                if self.config.connections.is_empty() {
                    "Add a connection to get started."
                } else {
                    "Select a connection."
                },
            ))
            .child(
                Button::new("empty-add")
                    .primary()
                    .label("Add connection")
                    .on_click(cx.listener(|this, _, window, cx| this.open_form(None, window, cx))),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = if let Some(form) = self.form.clone() {
            div().size_full().child(form).into_any_element()
        } else if let Some(conn) = self.selected_connection() {
            self.render_query_view(&conn, cx).into_any_element()
        } else {
            self.render_empty(cx).into_any_element()
        };
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run_query(cx)))
            .child(self.render_sidebar(cx))
            .child(div().flex_1().h_full().min_w_0().child(main))
    }
}

fn summary(r: &tuplebase_core::db::QueryResult) -> String {
    match r.rows_affected {
        Some(n) => format!("{n} row(s) affected · {} ms", r.elapsed_ms),
        None => format!(
            "{} row(s){} · {} ms",
            r.rows.len(),
            if r.truncated { " (truncated)" } else { "" },
            r.elapsed_ms
        ),
    }
}

#[cfg(test)]
mod tests {
    // No globs from gpui_kit here: its `test` macro would shadow the built-in `#[test]`.
    use super::{Config, Connection, Status, Workspace, bind_keys};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use serde_json::json;
    use tuplebase_core::config::Adapter;
    use tuplebase_core::db::QueryResult;

    fn open(config: Config, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            bind_keys(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1200.), px(780.)),
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Workspace::with_config(config, Status::Idle, window, cx))
            })
            .expect("open test window")
        })
    }

    fn config_with_connection() -> Config {
        let mut config = Config::default();
        let conn = Connection {
            name: "db".into(),
            adapter: Adapter::Postgres,
            // Nothing listens here; these tests never need a real server.
            host: "127.0.0.1".into(),
            port: 1,
            database: "app".into(),
            user: "dev".into(),
            readonly: false,
        };
        config.upsert(conn, None).unwrap();
        config
    }

    #[gpui_kit::test]
    fn form_opens_validates_and_cancels(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("empty-add").visible());
            window.click("empty-add", cx);
            assert!(window.find("save").visible());
            // Blank name is rejected and the form stays open.
            window.click("save", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let this = workspace.read(cx);
            let form = this.form.as_ref().expect("form stays open").read(cx);
            assert!(form.error.is_some());
            assert!(this.config.connections.is_empty());
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("cancel", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("save").is_none());
            assert!(window.find("empty-add").visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn query_view_renders_results(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.selected = Some("db".into());
                this.results.update(cx, |table, cx| {
                    table.delegate_mut().set(&QueryResult {
                        columns: vec!["id".into(), "name".into()],
                        rows: vec![vec![json!(1), json!("Ada")], vec![json!(2), json!(null)]],
                        truncated: false,
                        rows_affected: None,
                        elapsed_ms: 1,
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
        let (handle, workspace) = open(config_with_connection(), cx);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.selected = Some("db".into());
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("sql-editor", cx);
            window.press("secondary-enter", cx);
        })
        .unwrap();
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert!(matches!(this.status, Status::Busy(_)));
            assert_eq!(this.editor.read(cx).value().as_str(), "SELECT 1;");
        });
    }
}
