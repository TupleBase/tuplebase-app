//! Main window: connection sidebar, SQL editor, results table.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::LazyLock;

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{Adapter, Config, Connection};
use tuplebase_core::db::{Db, ExecOptions};
use tuplebase_core::{secrets, updates};

use crate::conn_form::{ConnForm, ConnFormEvent};
use crate::logos;
use crate::results::ResultsDelegate;
use crate::theme::folder_color;

actions!(tuplebase, [RunQuery]);

#[cfg(target_os = "macos")]
const RUN_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
const RUN_KEY: &str = "ctrl-enter";
#[cfg(target_os = "macos")]
const RUN_HINT: &str = "Run query (⌘↵)";
#[cfg(not(target_os = "macos"))]
const RUN_HINT: &str = "Run query (Ctrl+Enter)";

/// Call after `gpui_kit::init`, so the `Input` binding overrides the editor's own
/// one, which would insert a newline.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(RUN_KEY, RunQuery, Some("Input")),
        KeyBinding::new(RUN_KEY, RunQuery, None),
    ]);
}

const EDIT_ICON: &[u8] = include_bytes!("../../../assets/icons/square-pen.svg");
const DELETE_ICON: &[u8] = include_bytes!("../../../assets/icons/trash.svg");
const NEW_FOLDER_ICON: &[u8] = include_bytes!("../../../assets/icons/folder-plus.svg");
const RENAME_ICON: &[u8] = include_bytes!("../../../assets/icons/pencil.svg");

const SIDEBAR_WIDTH: f32 = 264.;
/// Empty space at the top of the window, where the traffic lights sit on macOS.
const WINDOW_TOP_STRIP: f32 = 40.;

/// Rows shown per query in the app; agents get their own cap in the MCP server.
const MAX_ROWS: usize = 1000;

/// sqlx needs Tokio; GPUI has its own executor. All database and keychain I/O runs
/// here, and GPUI tasks await the results.
#[cfg(not(test))]
static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Runtime::new().expect("failed to start the Tokio runtime"));

#[cfg(not(test))]
fn io<T: Send + 'static>(
    fut: impl Future<Output = anyhow::Result<T>> + Send + 'static,
) -> impl Future<Output = anyhow::Result<T>> {
    let handle = RUNTIME.spawn(fut);
    async move { handle.await? }
}

#[cfg(not(test))]
fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> impl Future<Output = anyhow::Result<T>> {
    let handle = RUNTIME.spawn_blocking(f);
    async move { handle.await? }
}

// UI tests stop at the "busy" state and never run I/O: GPUI's test scheduler panics
// when a Tokio thread wakes one of its tasks.
#[cfg(test)]
fn io<T>(fut: impl Future<Output = anyhow::Result<T>>) -> impl Future<Output = anyhow::Result<T>> {
    drop(fut);
    std::future::pending()
}

#[cfg(test)]
fn blocking<T>(f: impl FnOnce() -> anyhow::Result<T>) -> impl Future<Output = anyhow::Result<T>> {
    drop(f);
    std::future::pending()
}

enum Status {
    Idle,
    Busy(SharedString),
    Done(SharedString),
    Failed(SharedString),
}

/// Inline name field in the sidebar, for creating or renaming a folder.
struct FolderEdit {
    /// The folder being renamed; `None` when creating one.
    renaming: Option<String>,
    input: Entity<InputState>,
}

/// A connection being dragged to another folder; also draws itself under the pointer.
#[derive(Clone)]
struct DraggedConnection {
    name: String,
    adapter: Adapter,
}

impl Render for DraggedConnection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .text_sm()
            .child(logos::tile(self.adapter, 20.))
            .child(self.name.clone())
    }
}

pub struct Workspace {
    db: Arc<Db>,
    config: Config,
    selected: Option<String>,
    form: Option<Entity<ConnForm>>,
    editor: Entity<EditorState>,
    results: Entity<TableState<ResultsDelegate>>,
    status: Status,
    settings_open: bool,
    folder_edit: Option<FolderEdit>,
    _folder_edit_subscription: Option<Subscription>,
    /// Folders closed in the sidebar.
    collapsed: HashSet<String>,
    /// Newer version found by the update check.
    available_update: Option<String>,
    _form_subscription: Option<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Small local file read once before the first frame.
        let (config, status) = match Config::load() {
            Ok(config) => (config, Status::Idle),
            Err(e) => (Config::default(), Status::Failed(format!("{e:#}").into())),
        };
        cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        })
        .detach();
        let this = Self::with_config(config, status, window, cx);
        if this.config.check_for_updates {
            this.check_for_updates(cx);
        }
        this
    }

    fn check_for_updates(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            // Failures stay silent: being offline is not worth an error.
            let latest = blocking(|| updates::check(env!("CARGO_PKG_VERSION")))
                .await
                .ok()
                .flatten();
            this.update(cx, |this, cx| {
                this.available_update = latest;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_check_for_updates(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.config.check_for_updates = enabled;
        self.available_update = None;
        if enabled {
            self.check_for_updates(cx);
        }
        cx.notify();
        self.save_config(cx);
    }

    fn save_config(&self, cx: &mut Context<Self>) {
        let config = self.config.clone();
        cx.spawn(async move |this, cx| {
            if let Err(e) = blocking(move || config.save()).await {
                this.update(cx, |this, cx| {
                    this.status = Status::Failed(format!("{e:#}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn begin_folder_edit(
        &mut self,
        renaming: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = renaming.clone().unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Folder name")
                .default_value(value)
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self._folder_edit_subscription =
            Some(
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        this.commit_folder_edit(cx);
                    }
                }),
            );
        self.folder_edit = Some(FolderEdit { renaming, input });
        cx.notify();
    }

    fn cancel_folder_edit(&mut self, cx: &mut Context<Self>) {
        self.folder_edit = None;
        self._folder_edit_subscription = None;
        cx.notify();
    }

    fn commit_folder_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.folder_edit.take() else {
            return;
        };
        cx.notify();
        let name = edit.input.read(cx).value().trim().to_string();
        if name.is_empty() || edit.renaming.as_deref() == Some(name.as_str()) {
            return;
        }
        let result = match &edit.renaming {
            Some(old) => self.config.rename_folder(old, &name),
            None => self.config.add_folder(&name),
        };
        match result {
            Ok(()) => {
                if edit.renaming.is_some_and(|old| self.collapsed.remove(&old)) {
                    self.collapsed.insert(name);
                }
                self.save_config(cx);
            }
            Err(e) => self.status = Status::Failed(format!("{e:#}").into()),
        }
    }

    fn remove_folder(&mut self, folder: &str, cx: &mut Context<Self>) {
        self.config.remove_folder(folder);
        self.collapsed.remove(folder);
        self.save_config(cx);
        cx.notify();
    }

    fn confirm_remove_folder(
        &mut self,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_connections = self.config.connections.iter().any(|c| c.folder == folder);
        if !has_connections {
            self.remove_folder(&folder, cx);
            return;
        }
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let name = folder.clone();
            alert
                .title(format!("Delete folder “{folder}”?"))
                .description("Its connections move out of the folder. None of them are deleted.")
                .confirm()
                .ok_text("Delete folder")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.remove_folder(&name, cx))
                        .ok();
                    true
                })
        });
    }

    fn move_connection(&mut self, name: &str, folder: &str, cx: &mut Context<Self>) {
        if self.config.move_connection(name, folder) {
            self.collapsed.remove(folder);
            self.save_config(cx);
            cx.notify();
        }
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
            settings_open: false,
            folder_edit: None,
            _folder_edit_subscription: None,
            collapsed: HashSet::new(),
            available_update: None,
            _form_subscription: None,
        }
    }

    fn selected_connection(&self) -> Option<Connection> {
        let name = self.selected.as_deref()?;
        self.config.find(name).cloned()
    }

    fn select(&mut self, name: String, cx: &mut Context<Self>) {
        self.form = None;
        self.settings_open = false;
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

    /// Opens the connection form; a new connection is saved into `folder`.
    fn open_form(
        &mut self,
        existing: Option<Connection>,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = cx.new(|cx| ConnForm::new(existing.as_ref(), folder, window, cx));
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
        self.settings_open = false;
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
        let folder = connection.folder.clone();
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
                    this.collapsed.remove(&folder);
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
        if self.form.is_some() || self.settings_open || matches!(self.status, Status::Busy(_)) {
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

    fn toggle_folder(&mut self, folder: String, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&folder) {
            self.collapsed.insert(folder);
        }
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, bg) = (theme.sidebar_border, theme.sidebar);
        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(bg)
            .border_r_1()
            .border_color(border)
            .when(cfg!(target_os = "macos"), |el| {
                el.child(drag_area().w_full().h(px(WINDOW_TOP_STRIP)).flex_none())
            })
            .child(
                h_flex()
                    .pl_3()
                    .pr_2()
                    .pb_1()
                    .justify_between()
                    .child(div().text_sm().font_semibold().child("Connections"))
                    .child(
                        h_flex()
                            .child(
                                Button::new("add-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::default().data(NEW_FOLDER_ICON))
                                    .tooltip("New folder")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.begin_folder_edit(None, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("add-connection")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("New connection")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_form(None, String::new(), window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("connections")
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .gap_px()
                    .overflow_y_scroll()
                    .on_drop(cx.listener(|this, dragged: &DraggedConnection, _, cx| {
                        this.move_connection(&dragged.name, "", cx);
                    }))
                    .children(self.render_tree(cx)),
            )
            .child(
                v_flex()
                    .p_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(border)
                    .when_some(self.available_update.clone(), |el, version| {
                        el.child(
                            Button::new("download-update")
                                .primary()
                                .small()
                                .label(format!("Update to {version}"))
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.open_url(updates::DOWNLOAD_URL);
                                })),
                        )
                    })
                    .child(
                        h_flex().child(
                            Button::new("open-settings")
                                .ghost()
                                .small()
                                .icon(IconName::Settings)
                                .label("Settings")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.form = None;
                                    this.settings_open = true;
                                    cx.notify();
                                })),
                        ),
                    ),
            )
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        div().id("settings").size_full().overflow_y_scroll().child(
            v_flex().w_full().items_center().child(
                v_flex()
                    .w(px(520.))
                    .pt(px(56.))
                    .pb_8()
                    .gap_6()
                    .child(div().text_xl().font_semibold().child("Settings"))
                    .child(
                        v_flex()
                            .gap_2()
                            .child(div().font_medium().child("Updates"))
                            .child(
                                Checkbox::new("check-updates")
                                    .label("Check for updates at startup")
                                    .checked(self.config.check_for_updates)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.set_check_for_updates(*checked, cx);
                                    })),
                            )
                            .child(div().text_sm().text_color(muted).child(
                                "Asks GitHub whether a newer version exists. \
                                 Sends no connection details or queries.",
                            )),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().font_medium().child("About"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted)
                                    .child(concat!("TupleBase ", env!("CARGO_PKG_VERSION"))),
                            ),
                    )
                    .child(
                        h_flex().child(
                            Button::new("close-settings")
                                .primary()
                                .label("Done")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings_open = false;
                                    cx.notify();
                                })),
                        ),
                    ),
            ),
        )
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = Vec::new();
        let editing = self.folder_edit.as_ref();
        if let Some(edit) = editing.filter(|e| e.renaming.is_none()) {
            rows.push(self.render_folder_input(edit, cx).into_any_element());
        }
        for (folder, members) in self.config.grouped() {
            let named = !folder.is_empty();
            if named {
                match editing.filter(|e| e.renaming.as_deref() == Some(folder.as_str())) {
                    Some(edit) => rows.push(self.render_folder_input(edit, cx).into_any_element()),
                    None => rows.push(
                        self.render_folder(&folder, members.len(), cx)
                            .into_any_element(),
                    ),
                }
            }
            if !self.collapsed.contains(&folder) {
                for conn in members {
                    rows.push(self.render_connection(conn, named, cx).into_any_element());
                }
            }
        }
        rows
    }

    fn render_folder_input(&self, edit: &FolderEdit, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .mt_2()
            .px_1()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.commit_folder_edit(cx)))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.cancel_folder_edit(cx);
                }
            }))
            .child(Input::new(&edit.input).id("folder-name").small())
    }

    fn render_folder(
        &self,
        folder: &str,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, hover, radius) = (theme.muted_foreground, theme.sidebar_accent, theme.radius);
        let chevron = if self.collapsed.contains(folder) {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        let row_group = SharedString::from(format!("folder-row-{folder}"));
        let name = folder.to_owned();
        let action = |id: String, tooltip: &'static str| {
            Button::new(SharedString::from(id))
                .ghost()
                .xsmall()
                .tooltip(tooltip)
        };
        let (add_to, rename, remove, toggle, drop_into) = (
            name.clone(),
            name.clone(),
            name.clone(),
            name.clone(),
            name.clone(),
        );
        h_flex()
            .id(SharedString::from(format!("folder-{folder}")))
            .test_support()
            .group(row_group.clone())
            .relative()
            .mt_2()
            .gap_1p5()
            .px_1()
            .h(px(24.))
            .rounded(radius)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .drag_over::<DraggedConnection>(move |s, _, _, _| s.bg(hover))
            .on_drop(
                cx.listener(move |this, dragged: &DraggedConnection, _, cx| {
                    cx.stop_propagation();
                    this.move_connection(&dragged.name, &drop_into, cx);
                }),
            )
            .child(Icon::new(chevron).xsmall().text_color(muted))
            .child(
                div()
                    .w(px(3.))
                    .h(px(12.))
                    .rounded_full()
                    .bg(folder_color(folder)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_medium()
                    .child(name),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .pr_1()
                    .group_hover(row_group.clone(), |s| s.invisible())
                    .child(count.to_string()),
            )
            .child(
                h_flex()
                    .absolute()
                    .right_0()
                    .invisible()
                    .group_hover(row_group, |s| s.visible())
                    .child(
                        action(format!("add-in-{folder}"), "New connection in this folder")
                            .icon(IconName::Plus)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_form(None, add_to.clone(), window, cx);
                            })),
                    )
                    .child(
                        action(format!("rename-{folder}"), "Rename folder")
                            .icon(Icon::default().data(RENAME_ICON))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.begin_folder_edit(Some(rename.clone()), window, cx);
                            })),
                    )
                    .child(
                        action(format!("remove-{folder}"), "Delete folder")
                            .icon(Icon::default().data(DELETE_ICON))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.confirm_remove_folder(remove.clone(), window, cx);
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_folder(toggle.clone(), cx)))
    }

    fn render_connection(
        &self,
        conn: &Connection,
        in_folder: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, active, radius) = (theme.muted_foreground, theme.sidebar_accent, theme.radius);
        let selected = self.selected.as_deref() == Some(conn.name.as_str());
        let name = conn.name.clone();
        let folder = conn.folder.clone();
        h_flex()
            .id(SharedString::from(format!("conn-{}", conn.name)))
            .test_support()
            .gap_2()
            .pr_2()
            .py_1()
            .pl(px(if in_folder { 22. } else { 8. }))
            .rounded(radius)
            .cursor_pointer()
            .when(selected, |d| d.bg(active))
            .hover(move |s| s.bg(active))
            .child(logos::tile(conn.adapter, 24.))
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(div().text_sm().truncate().child(conn.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(muted)
                            .child(format!("{}:{}", conn.host, conn.port)),
                    ),
            )
            .on_drag(
                DraggedConnection {
                    name: conn.name.clone(),
                    adapter: conn.adapter,
                },
                |dragged, _, _, cx| cx.new(|_| dragged.clone()),
            )
            .on_drop(
                cx.listener(move |this, dragged: &DraggedConnection, _, cx| {
                    cx.stop_propagation();
                    this.move_connection(&dragged.name, &folder, cx);
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.select(name.clone(), cx)))
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (text, color) = match &self.status {
            Status::Idle => (SharedString::default(), theme.muted_foreground),
            Status::Busy(s) | Status::Done(s) => (s.clone(), theme.muted_foreground),
            Status::Failed(s) => (s.clone(), theme.danger),
        };
        div()
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

    fn render_query_view(&self, conn: &Connection, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, radius, muted, muted_bg) = (
            theme.border,
            theme.radius,
            theme.muted_foreground,
            theme.muted,
        );
        let busy = matches!(self.status, Status::Busy(_));
        let placeholder = self.results.read(cx).delegate().placeholder();
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
        let environment =
            (!conn.folder.is_empty()).then(|| (conn.folder.clone(), folder_color(&conn.folder)));
        v_flex()
            .size_full()
            .when_some(environment.clone(), |el, (_, color)| {
                el.child(div().w_full().h(px(3.)).flex_none().bg(color))
            })
            .child(
                h_flex()
                    .h(px(WINDOW_TOP_STRIP + 8.))
                    .flex_none()
                    .pl_3()
                    .pr_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(logos::tile(conn.adapter, 26.))
                    .child(div().font_semibold().child(conn.name.clone()))
                    .when_some(environment, |el, (name, color)| {
                        el.child(chip(name, color, color.opacity(0.14)))
                    })
                    .when(conn.readonly, |el| {
                        el.child(chip("Read-only".into(), muted, muted_bg))
                    })
                    .child(drag_area().flex_1().h_full())
                    .child(
                        Button::new("edit")
                            .ghost()
                            .small()
                            .icon(Icon::default().data(EDIT_ICON))
                            .tooltip("Edit connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let existing = this.selected_connection();
                                let folder = existing
                                    .as_ref()
                                    .map(|c| c.folder.clone())
                                    .unwrap_or_default();
                                this.open_form(existing, folder, window, cx);
                            })),
                    )
                    .child(
                        Button::new("delete")
                            .ghost()
                            .small()
                            .icon(Icon::default().data(DELETE_ICON))
                            .tooltip("Delete connection")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("run")
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
                div()
                    .id("sql-editor")
                    .test_support()
                    .h(px(220.))
                    .flex_none()
                    .border_b_1()
                    .border_color(border)
                    .child(Editor::new(&self.editor).appearance(false).h_full()),
            )
            .child(
                div().flex_1().min_h_0().child(match placeholder {
                    Some(text) => h_flex()
                        .size_full()
                        .justify_center()
                        .text_sm()
                        .text_color(muted)
                        .child(text)
                        .into_any_element(),
                    None => DataTable::new(&self.results)
                        .small()
                        .bordered(false)
                        .into_any_element(),
                }),
            )
            .child(self.render_status(cx))
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let failed = matches!(self.status, Status::Failed(_));
        v_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(logos::brand(72.))
                    .child(div().text_color(cx.theme().muted_foreground).child(
                        if self.config.connections.is_empty() {
                            "Add a connection to start querying."
                        } else {
                            "Choose a connection from the sidebar."
                        },
                    ))
                    .child(
                        Button::new("empty-add")
                            .primary()
                            .label("Add connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_form(None, String::new(), window, cx)
                            })),
                    ),
            )
            .when(failed, |el| el.child(self.render_status(cx)))
    }
}

/// Empty region that moves the window, since there is no title bar. It must stay empty:
/// a button inside it would start a window move instead of a click.
fn drag_area() -> Div {
    div()
        .window_control_area(WindowControlArea::Drag)
        .on_mouse_down(MouseButton::Left, |event, window, _| {
            if event.click_count == 2 {
                window.titlebar_double_click();
            } else {
                window.start_window_move();
            }
        })
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_toolbar =
            self.form.is_none() && !self.settings_open && self.selected_connection().is_some();
        let main = if let Some(form) = self.form.clone() {
            div().size_full().child(form).into_any_element()
        } else if self.settings_open {
            self.render_settings(cx).into_any_element()
        } else if let Some(conn) = self.selected_connection() {
            self.render_query_view(&conn, cx).into_any_element()
        } else {
            self.render_empty(cx).into_any_element()
        };
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .when(cfg!(not(target_os = "macos")), |el| {
                el.child(TitleBar::new())
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run_query(cx)))
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(main)
                            .when(!has_toolbar, |el| {
                                el.child(
                                    drag_area()
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .w_full()
                                        .h(px(WINDOW_TOP_STRIP)),
                                )
                            }),
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

fn summary(r: &tuplebase_core::db::QueryResult) -> String {
    let ms = r.elapsed_ms;
    match r.rows_affected {
        Some(n) => format!("{} changed in {ms} ms", rows_text(n as usize)),
        None if r.truncated => format!("First {} in {ms} ms", rows_text(r.rows.len())),
        None => format!("{} in {ms} ms", rows_text(r.rows.len())),
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
                    size(px(1317.), px(877.)),
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
            folder: "Production".into(),
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

    #[test]
    fn summary_reads_naturally() {
        let result = |rows: usize, affected, truncated| QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new(); rows],
            truncated,
            rows_affected: affected,
            elapsed_ms: 5,
        };
        assert_eq!(super::summary(&result(1, None, false)), "1 row in 5 ms");
        assert_eq!(
            super::summary(&result(2, None, true)),
            "First 2 rows in 5 ms"
        );
        assert_eq!(
            super::summary(&result(0, Some(3), false)),
            "3 rows changed in 5 ms"
        );
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
    fn clicking_a_connection_selects_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            window.click("conn-db", cx);
        })
        .unwrap();
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert_eq!(this.selected.as_deref(), Some("db"));
            assert!(matches!(this.status, Status::Busy(_)));
        });
    }

    #[gpui_kit::test]
    fn update_button_appears_only_when_a_version_is_available(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("download-update").is_none());
            assert!(window.find("open-settings").visible());
        })
        .unwrap();
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.available_update = Some("9.9.9".into());
                cx.notify();
            });
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("download-update").visible());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn clicking_a_folder_collapses_and_expands_it(cx: &mut TestAppContext) {
        let (handle, _) = open(config_with_connection(), cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("conn-db").visible());
            window.click("folder-Production", cx);
            window.render_frame(cx);
            assert!(window.try_find("conn-db").is_none());
            window.click("folder-Production", cx);
            window.render_frame(cx);
            assert!(window.find("conn-db").visible());
        })
        .unwrap();
    }

    fn window_action<R>(
        handle: AnyWindowHandle,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> R,
    ) -> R {
        let result = cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            f(window, cx)
        });
        cx.run_until_parked();
        result.unwrap()
    }

    #[gpui_kit::test]
    fn settings_panel_opens_and_closes(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("check-updates").is_none());
            window.click("open-settings", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("check-updates").visible());
            window.click("check-updates", cx);
        });
        cx.update(|cx| assert!(!workspace.read(cx).config.check_for_updates));
        window_action(handle, cx, |window, cx| window.click("close-settings", cx));
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("check-updates").is_none());
        });
    }

    #[gpui_kit::test]
    fn new_folder_is_created_from_the_sidebar(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            window.input("Local", cx);
            window.press("enter", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("folder-Local").visible());
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).config.folders(), ["Local"]));
    }

    #[gpui_kit::test]
    fn escape_cancels_creating_a_folder(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            window.input("Nope", cx);
            window.press("escape", cx);
        });
        cx.update(|cx| assert!(workspace.read(cx).config.folders().is_empty()));
    }

    #[gpui_kit::test]
    fn clicking_elsewhere_closes_the_folder_field(cx: &mut TestAppContext) {
        let (handle, _) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.find("folder-name").visible());
            window.click("open-settings", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("folder-name").is_none());
        });
    }

    #[gpui_kit::test]
    fn renaming_a_folder_moves_its_connections(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        window_action(handle, cx, |window, cx| {
            window.hover("folder-Production", cx);
            window.click("rename-Production", cx);
        });
        window_action(handle, cx, |window, cx| {
            window.press("secondary-a", cx);
            window.input("Live", cx);
            window.press("enter", cx);
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).config.connections[0].folder, "Live"));
    }

    #[gpui_kit::test]
    fn plus_on_a_folder_targets_that_folder_without_collapsing_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        window_action(handle, cx, |window, cx| {
            window.hover("folder-Production", cx);
            window.click("add-in-Production", cx);
        });
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert!(this.collapsed.is_empty());
            let form = this.form.as_ref().expect("form opens").read(cx);
            assert_eq!(form.folder, "Production");
        });
    }

    #[gpui_kit::test]
    fn deleting_an_empty_folder_needs_no_confirmation(cx: &mut TestAppContext) {
        let mut config = Config::default();
        config.add_folder("Old").unwrap();
        let (handle, workspace) = open(config, cx);
        window_action(handle, cx, |window, cx| {
            window.hover("folder-Old", cx);
            window.click("remove-Old", cx);
        });
        cx.update(|cx| assert!(workspace.read(cx).config.folders().is_empty()));
    }

    #[gpui_kit::test]
    fn dragging_a_connection_onto_a_folder_moves_it(cx: &mut TestAppContext) {
        let mut config = config_with_connection();
        config.add_folder("Local").unwrap();
        let (handle, workspace) = open(config, cx);
        window_action(handle, cx, |window, cx| {
            window.drag_to("conn-db", "folder-Local", cx);
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).config.connections[0].folder, "Local"));
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
