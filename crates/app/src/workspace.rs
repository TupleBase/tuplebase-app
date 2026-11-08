//! Main window: connection sidebar, SQL editor, results table.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{
    Adapter, Config, Connection, FolderNode, folder_name, parent_of, rebase_folder,
};
use tuplebase_core::db::{Db, ExecOptions};
use tuplebase_core::state::SessionState;
use tuplebase_core::{secrets, updates};

use crate::completions::{SchemaNames, SqlCompletions};
use crate::conn_form::{ConnForm, ConnFormEvent};
use crate::logos;
use crate::results::ResultsDelegate;
use crate::runtime::{blocking, io};
use crate::schema::{SchemaEvent, SchemaPanel};

actions!(
    tuplebase,
    [
        RunQuery,
        TreeUp,
        TreeDown,
        TreeCollapse,
        TreeExpand,
        TreeOpen
    ]
);

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
        KeyBinding::new("up", TreeUp, Some(TREE_CONTEXT)),
        KeyBinding::new("down", TreeDown, Some(TREE_CONTEXT)),
        KeyBinding::new("left", TreeCollapse, Some(TREE_CONTEXT)),
        KeyBinding::new("right", TreeExpand, Some(TREE_CONTEXT)),
        KeyBinding::new("enter", TreeOpen, Some(TREE_CONTEXT)),
    ]);
}

const EDIT_ICON: &[u8] = include_bytes!("../../../assets/icons/square-pen.svg");
const DELETE_ICON: &[u8] = include_bytes!("../../../assets/icons/trash.svg");
const NEW_FOLDER_ICON: &[u8] = include_bytes!("../../../assets/icons/folder-plus.svg");
const HISTORY_ICON: &[u8] = include_bytes!("../../../assets/icons/clock.svg");
const RENAME_ICON: &[u8] = include_bytes!("../../../assets/icons/pencil.svg");

const HISTORY_MENU_SIZE: usize = 20;
const TREE_CONTEXT: &str = "ConnectionTree";
const SIDEBAR_WIDTH: f32 = 300.;
/// Sidebar indent per folder level.
const TREE_INDENT: f32 = 16.;
/// Empty space at the top of the window, where the traffic lights sit on macOS.
const WINDOW_TOP_STRIP: f32 = 40.;

/// Rows shown per query in the app; agents get their own cap in the MCP server.
const MAX_ROWS: usize = 1000;

enum Status {
    Idle,
    Busy(SharedString),
    Done(SharedString),
    Failed(SharedString),
}

/// One visible line of the connection tree.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TreeRow {
    Folder(String),
    Connection(String),
}

/// Inline name field in the sidebar, for creating or renaming a folder.
struct FolderEdit {
    /// Where a new folder goes; unused when renaming.
    parent: String,
    /// The folder being renamed; `None` when creating one.
    renaming: Option<String>,
    input: Entity<InputState>,
}

/// A connection being dragged to another folder; also draws itself under the pointer.
#[derive(Clone)]
struct DraggedConnection {
    name: String,
    adapter: Adapter,
    color: String,
}

/// A folder being dragged into another one.
#[derive(Clone)]
struct DraggedFolder {
    path: String,
}

impl Render for DraggedFolder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .text_sm()
            .child(folder_name(&self.path).to_owned())
    }
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
            .child(logos::icon(self.adapter, &self.color, 16., cx))
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
    session: SessionState,
    /// SQL to show once the next frame has a window to apply it with.
    pending_editor_text: Option<String>,
    _editor_subscription: Subscription,
    _session_save: Option<Task<()>>,
    schema: Entity<SchemaPanel>,
    _schema_subscription: Subscription,
    tree_focus: FocusHandle,
    tree_scroll: ScrollHandle,
    /// The row arrow keys act on.
    cursor: Option<TreeRow>,
    tree_focused: bool,
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
        let mut this = Self::with_config(config, status, window, cx);
        this.session = SessionState::load();
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
        parent: String,
        renaming: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.collapsed.remove(&parent);
        let value = renaming
            .as_deref()
            .map(folder_name)
            .unwrap_or_default()
            .to_owned();
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
        self.folder_edit = Some(FolderEdit {
            parent,
            renaming,
            input,
        });
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
        let unchanged = edit
            .renaming
            .as_deref()
            .is_some_and(|old| folder_name(old) == name);
        if name.is_empty() || unchanged {
            return;
        }
        let result = match &edit.renaming {
            Some(old) => self
                .config
                .rename_folder(old, &name)
                .map(|new| Some((old.clone(), new))),
            None => self.config.add_folder(&edit.parent, &name).map(|_| None),
        };
        match result {
            Ok(renamed) => {
                if let Some((old, new)) = renamed {
                    self.rebase_collapsed(&old, &new);
                }
                self.save_config(cx);
            }
            Err(e) => self.status = Status::Failed(format!("{e:#}").into()),
        }
    }

    /// Keeps folders closed after they, or a folder above them, get a new path.
    fn rebase_collapsed(&mut self, old: &str, new: &str) {
        self.collapsed = self
            .collapsed
            .drain()
            .map(|path| rebase_folder(&path, old, new).unwrap_or(path))
            .collect();
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
        let is_inside = |path: &str| path != folder && rebase_folder(path, &folder, "").is_some();
        let has_contents = self.config.folders().iter().any(|f| is_inside(f))
            || self
                .config
                .connections
                .iter()
                .any(|c| c.folder == folder || is_inside(&c.folder));
        if !has_contents {
            self.remove_folder(&folder, cx);
            return;
        }
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let name = folder.clone();
            alert
                .title(format!("Delete folder “{}”?", folder_name(&folder)))
                .description("What's inside moves up one level. No connections are deleted.")
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

    fn move_folder(&mut self, path: &str, new_parent: &str, cx: &mut Context<Self>) {
        match self.config.move_folder(path, new_parent) {
            Ok(None) => return,
            Ok(Some(new_path)) => {
                self.rebase_collapsed(path, &new_path);
                self.collapsed.remove(new_parent);
                self.save_config(cx);
            }
            Err(e) => self.status = Status::Failed(format!("{e:#}").into()),
        }
        cx.notify();
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
        let db = Arc::new(Db::new());
        let names = SchemaNames::default();
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value("SELECT 1;");
            state.lsp_mut().completion_provider = Some(Rc::new(SqlCompletions {
                names: names.clone(),
            }));
            state
        });
        let _editor_subscription = cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.remember_draft(cx);
            }
        });
        let schema = cx.new(|cx| SchemaPanel::new(db.clone(), names, window, cx));
        let _schema_subscription = cx.subscribe_in(
            &schema,
            window,
            |this, _, event: &SchemaEvent, window, cx| match event {
                SchemaEvent::Preview(sql) => {
                    this.editor
                        .update(cx, |editor, cx| editor.set_value(sql.clone(), window, cx));
                    this.remember_draft(cx);
                    this.run_query(cx);
                }
            },
        );
        Self {
            db,
            schema,
            _schema_subscription,
            config,
            selected: None,
            form: None,
            editor,
            session: SessionState::default(),
            pending_editor_text: None,
            _editor_subscription,
            _session_save: None,
            results: cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx)),
            status,
            tree_focus: cx.focus_handle(),
            tree_scroll: ScrollHandle::new(),
            cursor: None,
            tree_focused: false,
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
        if self.pending_editor_text.is_none() {
            self.remember_draft(cx);
        }
        self.pending_editor_text =
            Some(self.session.drafts.get(&name).cloned().unwrap_or_default());
        self.selected = Some(name);
        self.results.update(cx, |table, cx| {
            table.delegate_mut().clear();
            table.refresh(cx);
        });
        let Some(conn) = self.selected_connection() else {
            return;
        };
        let shown = conn.clone();
        self.schema
            .update(cx, |schema, cx| schema.set_connection(Some(shown), cx));
        self.status = Status::Busy(format!("Connecting to {}…", conn.name).into());
        cx.notify();
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let name = conn.name.clone();
            let result = io(async move { db.test(&conn).await }).await;
            this.update(cx, |this, cx| {
                // The user may have moved on to another connection by now.
                if this.selected.as_deref() != Some(name.as_str()) {
                    return;
                }
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
                ssh_password,
                previous,
            } => this.save_connection(
                form,
                connection.clone(),
                password.clone(),
                ssh_password.clone(),
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
        ssh_password: Option<String>,
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
                if let Some(old) = renamed {
                    secrets::rename(old, &connection.name)?;
                }
                if let Some(pw) = &password {
                    secrets::set_password(&connection.name, pw)?;
                }
                if let Some(secret) = &ssh_password {
                    secrets::set_ssh_password(&connection.name, secret)?;
                }
                Ok(previous)
            })
            .await;
            let forget = match &result {
                Ok(Some(previous)) => Some(previous.clone()),
                _ => None,
            };
            let renamed_from = forget.clone().filter(|old| *old != name);
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
                    if let Some(old) = &renamed_from {
                        this.session.rename_connection(old, &name);
                        this.selected = Some(name.clone());
                    }
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
        self.schema
            .update(cx, |schema, cx| schema.set_connection(None, cx));
        self.config.remove(&name);
        self.session.remove_connection(&name);
        self.schedule_session_save(cx);
        let config = self.config.clone();
        let db = self.db.clone();
        self.status = Status::Idle;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let forget_name = name.clone();
            let result = blocking(move || {
                config.save()?;
                secrets::delete_all(&name)
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

    /// Runs the selected text, or the whole editor when nothing is selected.
    fn run_query(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.selected_connection() else {
            return;
        };
        if self.form.is_some() || self.settings_open || matches!(self.status, Status::Busy(_)) {
            return;
        }
        let sql = {
            let editor = self.editor.read(cx);
            let selected = editor.selected_value();
            if selected.trim().is_empty() {
                editor.value().to_string()
            } else {
                selected.to_string()
            }
        };
        self.session.record(&conn.name, &sql);
        self.schedule_session_save(cx);
        self.status = Status::Busy("Running…".into());
        cx.notify();
        let db = self.db.clone();
        let opts = ExecOptions {
            read_only: false,
            max_rows: MAX_ROWS,
        };
        let name = conn.name.clone();
        cx.spawn(async move |this, cx| {
            let result = io(async move { db.execute_script(&conn, &sql, opts).await }).await;
            this.update(cx, |this, cx| {
                // Rows from a connection the user has left must not show under another.
                if this.selected.as_deref() != Some(name.as_str()) {
                    return;
                }
                this.status = match &result {
                    Ok(run) => Status::Done(summary(&run.result, run.statements).into()),
                    Err(e) => Status::Failed(format!("{e:#}").into()),
                };
                if let Ok(run) = &result {
                    // A statement that changed something may have changed the schema too.
                    if run.changed {
                        this.schema.update(cx, |schema, cx| schema.refresh(cx));
                    }
                    this.results.update(cx, |table, cx| {
                        table.delegate_mut().set(&run.result);
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

    /// Keeps what's in the editor for the selected connection.
    fn remember_draft(&mut self, cx: &mut Context<Self>) {
        if let Some(name) = self.selected.clone() {
            let sql = self.editor.read(cx).value().to_string();
            self.session.set_draft(&name, &sql);
            self.schedule_session_save(cx);
        }
    }

    /// Saves shortly after the last change, so typing doesn't write on every keystroke.
    fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        self._session_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let Ok(session) = this.read_with(cx, |this, _| this.session.clone()) else {
                return;
            };
            blocking(move || session.save()).await.ok();
        }));
    }

    fn show_history_query(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_value(sql, window, cx));
        self.remember_draft(cx);
        cx.notify();
    }

    /// Every row currently showing in the tree, top to bottom.
    fn visible_rows(&self) -> Vec<TreeRow> {
        fn walk(node: &FolderNode<'_>, collapsed: &HashSet<String>, rows: &mut Vec<TreeRow>) {
            for folder in &node.folders {
                rows.push(TreeRow::Folder(folder.path.clone()));
                if !collapsed.contains(&folder.path) {
                    walk(folder, collapsed, rows);
                }
            }
            for conn in &node.connections {
                rows.push(TreeRow::Connection(conn.name.clone()));
            }
        }
        let mut rows = Vec::new();
        walk(&self.config.tree(), &self.collapsed, &mut rows);
        rows
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .cursor
            .as_ref()
            .and_then(|cursor| rows.iter().position(|row| row == cursor));
        let last = rows.len() as isize - 1;
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, last),
            None if delta > 0 => 0,
            None => last,
        } as usize;
        self.tree_scroll.scroll_to_item(next);
        self.cursor = Some(rows[next].clone());
        cx.notify();
    }

    /// Left arrow: closes an open folder, otherwise jumps to the folder above.
    fn tree_collapse(&mut self, cx: &mut Context<Self>) {
        let parent = match self.cursor.clone() {
            Some(TreeRow::Folder(path)) if !self.collapsed.contains(&path) => {
                self.collapsed.insert(path);
                cx.notify();
                return;
            }
            Some(TreeRow::Folder(path)) => parent_of(&path).to_owned(),
            Some(TreeRow::Connection(name)) => self
                .config
                .find(&name)
                .map(|c| c.folder.clone())
                .unwrap_or_default(),
            None => return,
        };
        if !parent.is_empty() {
            self.cursor = Some(TreeRow::Folder(parent));
            cx.notify();
        }
    }

    /// Right arrow: opens a closed folder, otherwise moves into it or opens the connection.
    fn tree_expand(&mut self, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            Some(TreeRow::Folder(path)) if self.collapsed.remove(&path) => cx.notify(),
            Some(TreeRow::Folder(_)) => self.move_cursor(1, cx),
            Some(TreeRow::Connection(name)) => self.select(name, cx),
            None => {}
        }
    }

    fn tree_open(&mut self, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            Some(TreeRow::Folder(path)) => self.toggle_folder(path, cx),
            Some(TreeRow::Connection(name)) => self.select(name, cx),
            None => {}
        }
    }

    /// Replaces this app with a new version, or sends development builds to the download
    /// page so a release doesn't overwrite them.
    fn install_update(&mut self, version: &str, cx: &mut Context<Self>) {
        let installation = std::env::current_exe()
            .ok()
            .and_then(|exe| updates::detect_installation(&exe));
        let Some(installation) = installation else {
            cx.open_url(updates::DOWNLOAD_URL);
            return;
        };
        match updates::spawn_upgrade(&installation, version) {
            Ok(()) => {
                // The usual delayed save would not run after quitting.
                self.session.save().ok();
                cx.quit();
            }
            Err(e) => {
                self.status = Status::Failed(format!("{e:#}").into());
                cx.notify();
            }
        }
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
                                        this.begin_folder_edit(String::new(), None, window, cx);
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
                    .track_focus(&self.tree_focus)
                    .key_context(TREE_CONTEXT)
                    .track_scroll(&self.tree_scroll)
                    .on_action(cx.listener(|this, _: &TreeUp, _, cx| this.move_cursor(-1, cx)))
                    .on_action(cx.listener(|this, _: &TreeDown, _, cx| this.move_cursor(1, cx)))
                    .on_action(cx.listener(|this, _: &TreeCollapse, _, cx| this.tree_collapse(cx)))
                    .on_action(cx.listener(|this, _: &TreeExpand, _, cx| this.tree_expand(cx)))
                    .on_action(cx.listener(|this, _: &TreeOpen, _, cx| this.tree_open(cx)))
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .gap_px()
                    .overflow_y_scroll()
                    .on_drop(cx.listener(|this, dragged: &DraggedConnection, _, cx| {
                        this.move_connection(&dragged.name, "", cx);
                    }))
                    .on_drop(cx.listener(|this, dragged: &DraggedFolder, _, cx| {
                        this.move_folder(&dragged.path, "", cx);
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
                                .tooltip("Closes TupleBase, installs the update and reopens")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.install_update(&version, cx);
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
                            .gap_2()
                            .child(div().font_medium().child("Query history"))
                            .child(div().text_sm().text_color(muted).child(
                                "Your queries and unsaved SQL are kept on this computer only.",
                            ))
                            .child(
                                h_flex().child(
                                    Button::new("clear-history")
                                        .small()
                                        .label("Clear query history")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.session.history.clear();
                                            this.schedule_session_save(cx);
                                            cx.notify();
                                        })),
                                ),
                            ),
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

    fn is_cursor(&self, row: &TreeRow) -> bool {
        self.tree_focused && self.cursor.as_ref() == Some(row)
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tree = self.config.tree();
        let mut rows = Vec::new();
        self.render_new_folder_field("", 0, &mut rows, cx);
        self.render_folder_contents(&tree, 0, &mut rows, cx);
        rows
    }

    fn render_new_folder_field(
        &self,
        parent: &str,
        depth: usize,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let edit = self.folder_edit.as_ref();
        if let Some(edit) = edit.filter(|e| e.renaming.is_none() && e.parent == parent) {
            rows.push(self.render_folder_input(edit, depth, cx).into_any_element());
        }
    }

    /// Subfolders first, then the connections directly inside.
    fn render_folder_contents(
        &self,
        node: &FolderNode<'_>,
        depth: usize,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        for folder in &node.folders {
            let renaming = self
                .folder_edit
                .as_ref()
                .filter(|e| e.renaming.as_deref() == Some(folder.path.as_str()));
            rows.push(match renaming {
                Some(edit) => self.render_folder_input(edit, depth, cx).into_any_element(),
                None => self
                    .render_folder(&folder.path, folder.connection_count(), depth, cx)
                    .into_any_element(),
            });
            if !self.collapsed.contains(&folder.path) {
                self.render_new_folder_field(&folder.path, depth + 1, rows, cx);
                self.render_folder_contents(folder, depth + 1, rows, cx);
            }
        }
        for conn in &node.connections {
            rows.push(self.render_connection(conn, depth, cx).into_any_element());
        }
    }

    fn render_folder_input(
        &self,
        edit: &FolderEdit,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .when(depth == 0, |d| d.mt_2())
            .pl(px(4. + depth as f32 * TREE_INDENT))
            .pr_1()
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
        depth: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, hover, radius, ring) = (
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.radius,
            theme.ring,
        );
        let chevron = if self.collapsed.contains(folder) {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        let row_group = SharedString::from(format!("folder-row-{folder}"));
        let path = folder.to_owned();
        let action = |id: String, tooltip: &'static str| {
            Button::new(SharedString::from(id))
                .ghost()
                .xsmall()
                .tooltip(tooltip)
        };
        let (add_in, new_in, rename, remove, toggle, into_connection, into_folder) = (
            path.clone(),
            path.clone(),
            path.clone(),
            path.clone(),
            path.clone(),
            path.clone(),
            path.clone(),
        );
        h_flex()
            .id(SharedString::from(format!("folder-{folder}")))
            .test_support()
            .border_1()
            .border_color(if self.is_cursor(&TreeRow::Folder(folder.to_owned())) {
                ring
            } else {
                transparent_black()
            })
            .group(row_group.clone())
            .relative()
            .when(depth == 0, |d| d.mt_2())
            .gap_1p5()
            .pl(px(4. + depth as f32 * TREE_INDENT))
            .pr_1()
            .h(px(30.))
            .rounded(radius)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_drag(DraggedFolder { path: path.clone() }, |dragged, _, _, cx| {
                cx.new(|_| dragged.clone())
            })
            .drag_over::<DraggedConnection>(move |s, _, _, _| s.bg(hover))
            .drag_over::<DraggedFolder>(move |s, _, _, _| s.bg(hover))
            .on_drop(
                cx.listener(move |this, dragged: &DraggedConnection, _, cx| {
                    cx.stop_propagation();
                    this.move_connection(&dragged.name, &into_connection, cx);
                }),
            )
            .on_drop(cx.listener(move |this, dragged: &DraggedFolder, _, cx| {
                cx.stop_propagation();
                this.move_folder(&dragged.path, &into_folder, cx);
            }))
            .child(Icon::new(chevron).xsmall().text_color(muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_medium()
                    .child(folder_name(folder).to_owned()),
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
                                this.open_form(None, add_in.clone(), window, cx);
                            })),
                    )
                    .child(
                        action(format!("add-folder-in-{folder}"), "New folder inside")
                            .icon(Icon::default().data(NEW_FOLDER_ICON))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.begin_folder_edit(new_in.clone(), None, window, cx);
                            })),
                    )
                    .child(
                        action(format!("rename-{folder}"), "Rename folder")
                            .icon(Icon::default().data(RENAME_ICON))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.begin_folder_edit(
                                    String::new(),
                                    Some(rename.clone()),
                                    window,
                                    cx,
                                );
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
            .on_click(cx.listener(move |this, _, window, cx| {
                this.cursor = Some(TreeRow::Folder(toggle.clone()));
                this.tree_focus.focus(window, cx);
                this.toggle_folder(toggle.clone(), cx);
            }))
    }

    fn render_connection(
        &self,
        conn: &Connection,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, active, radius, ring) = (
            theme.muted_foreground,
            theme.sidebar_accent,
            theme.radius,
            theme.ring,
        );
        let selected = self.selected.as_deref() == Some(conn.name.as_str());
        let name = conn.name.clone();
        let folder = conn.folder.clone();
        h_flex()
            .id(SharedString::from(format!("conn-{}", conn.name)))
            .test_support()
            .border_1()
            .border_color(if self.is_cursor(&TreeRow::Connection(conn.name.clone())) {
                ring
            } else {
                transparent_black()
            })
            .h(px(32.))
            .gap_2()
            .pr_2()
            .pl(px(8. + depth as f32 * TREE_INDENT))
            .rounded(radius)
            .cursor_pointer()
            .when(selected, |d| d.bg(active))
            .hover(move |s| s.bg(active))
            .child(logos::icon(conn.adapter, &conn.color, 18., cx))
            .child(
                div()
                    .max_w(px(170.))
                    .truncate()
                    .text_sm()
                    .child(conn.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(conn.host.clone()),
            )
            .on_drag(
                DraggedConnection {
                    name: conn.name.clone(),
                    adapter: conn.adapter,
                    color: conn.color.clone(),
                },
                |dragged, _, _, cx| cx.new(|_| dragged.clone()),
            )
            .on_drop(
                cx.listener(move |this, dragged: &DraggedConnection, _, cx| {
                    cx.stop_propagation();
                    this.move_connection(&dragged.name, &folder, cx);
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.cursor = Some(TreeRow::Connection(name.clone()));
                this.tree_focus.focus(window, cx);
                this.select(name.clone(), cx);
            }))
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
        let folder_label = (!conn.folder.is_empty()).then(|| conn.folder.replace('/', " / "));
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(WINDOW_TOP_STRIP + 8.))
                    .flex_none()
                    .pl_3()
                    .pr_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .child(logos::icon(conn.adapter, &conn.color, 20., cx))
                    .child(div().font_semibold().child(conn.name.clone()))
                    .when_some(folder_label, |el, name| {
                        el.child(chip(name, muted, muted_bg))
                    })
                    .when(conn.readonly, |el| {
                        el.child(chip("Read-only".into(), muted, muted_bg))
                    })
                    .child(drag_area().flex_1().h_full())
                    .child({
                        let items: Vec<String> = self
                            .session
                            .history_for(&conn.name)
                            .take(HISTORY_MENU_SIZE)
                            .map(str::to_owned)
                            .collect();
                        let this = cx.entity().downgrade();
                        Button::new("history")
                            .ghost()
                            .small()
                            .icon(Icon::default().data(HISTORY_ICON))
                            .tooltip("Query history")
                            .dropdown_menu(move |mut menu, _, _| {
                                if items.is_empty() {
                                    menu = menu.label("No queries yet");
                                }
                                for sql in &items {
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
                            })
                    })
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
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.schema.clone())
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
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
                            .child(self.render_status(cx)),
                    ),
            )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tree_focused = self.tree_focus.is_focused(window);
        if let Some(text) = self.pending_editor_text.take() {
            self.editor
                .update(cx, |editor, cx| editor.set_value(text, window, cx));
        }
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

fn summary(r: &tuplebase_core::db::QueryResult, statements: usize) -> String {
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
    match flat.char_indices().nth(70) {
        Some((end, _)) => format!("{}…", &flat[..end]),
        None => flat,
    }
}

#[cfg(test)]
mod tests {
    // No globs from gpui_kit here: its `test` macro would shadow the built-in `#[test]`.
    use super::{Config, Connection, Status, TreeRow, Workspace, bind_keys};
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
            color: String::new(),
            tls: Default::default(),
            ssh: None,
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
        assert_eq!(super::summary(&result(1, None, false), 1), "1 row in 5 ms");
        assert_eq!(
            super::summary(&result(2, None, true), 1),
            "First 2 rows in 5 ms"
        );
        assert_eq!(
            super::summary(&result(0, Some(3), false), 1),
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
    fn ssh_fields_appear_and_require_a_fingerprint(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-ssh-host").is_none());
            window.click("use-ssh", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("conn-ssh-host").bounds().size.width > px(0.));
            window.click("conn-name", cx);
            window.input("tunneled", cx);
            window.click("conn-host", cx);
            window.input("db", cx);
            window.click("conn-user", cx);
            window.input("me", cx);
            // The longer form scrolls, so bring Save into view.
            window.scroll(
                "conn-form",
                gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-2000.))),
                cx,
            );
        });
        window_action(handle, cx, |window, cx| window.click("save", cx));
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert!(this.config.connections.is_empty());
            let form = this.form.as_ref().expect("form stays open").read(cx);
            assert!(form.error.is_some());
        });
    }

    #[test]
    fn summary_counts_statements_and_labels_stay_short() {
        let result = QueryResult {
            columns: Vec::new(),
            rows: vec![Vec::new(); 2],
            truncated: false,
            rows_affected: None,
            elapsed_ms: 5,
        };
        assert_eq!(super::summary(&result, 3), "3 statements. 2 rows in 5 ms");
        assert_eq!(super::history_label("select\n  1"), "select 1");
        assert_eq!(super::history_label(&"x".repeat(100)).chars().count(), 71);
    }

    #[gpui_kit::test]
    fn running_records_history_and_switching_keeps_drafts(cx: &mut TestAppContext) {
        let mut config = config_with_connection();
        let mut other = config.connections[0].clone();
        other.name = "other".into();
        config.upsert(other, None).unwrap();
        let (handle, workspace) = open(config, cx);
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 42", cx);
        });
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.status = Status::Idle;
                this.run_query(cx);
            });
        });
        window_action(handle, cx, |window, cx| window.click("conn-other", cx));
        window_action(handle, cx, |_, cx| {
            assert_eq!(workspace.read(cx).editor.read(cx).value().as_str(), "");
        });
        cx.update(|cx| {
            workspace.update(cx, |this, _| this.status = Status::Idle);
        });
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.editor.read(cx).value().as_str(), "select 42");
            assert_eq!(this.session.history_for("db").next(), Some("select 42"));
        });
    }

    #[gpui_kit::test]
    fn autocomplete_finishes_a_keyword(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.selected = Some("db".into());
                cx.notify();
            });
        });
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.press("secondary-a", cx);
            window.input("SELE", cx);
        });
        window_action(handle, cx, |window, cx| window.press("enter", cx));
        cx.update(|cx| {
            assert_eq!(
                workspace.read(cx).editor.read(cx).value().as_str(),
                "SELECT"
            );
        });
    }

    #[gpui_kit::test]
    fn arrow_keys_walk_and_fold_the_tree(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        let cursor = |cx: &mut TestAppContext| {
            workspace
                .read_with(cx, |this, _| this.cursor.clone())
                .unwrap()
        };
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.press("up", cx));
        assert_eq!(cursor(cx), TreeRow::Folder("Production".into()));

        window_action(handle, cx, |window, cx| window.press("left", cx));
        cx.update(|cx| assert!(workspace.read(cx).collapsed.contains("Production")));

        window_action(handle, cx, |window, cx| window.press("right", cx));
        cx.update(|cx| assert!(workspace.read(cx).collapsed.is_empty()));

        window_action(handle, cx, |window, cx| window.press("right", cx));
        assert_eq!(cursor(cx), TreeRow::Connection("db".into()));

        window_action(handle, cx, |window, cx| window.press("left", cx));
        assert_eq!(cursor(cx), TreeRow::Folder("Production".into()));
    }

    #[gpui_kit::test]
    fn a_connection_color_can_be_chosen_in_the_form(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| window.click("color-3", cx));
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert_eq!(this.form.as_ref().unwrap().read(cx).color, "#E2A336");
        });
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
    fn a_folder_can_be_created_inside_another(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        window_action(handle, cx, |window, cx| {
            window.hover("folder-Production", cx);
            window.click("add-folder-in-Production", cx);
        });
        window_action(handle, cx, |window, cx| {
            window.input("EU", cx);
            window.press("enter", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("folder-Production/EU").visible());
        });
        cx.update(|cx| {
            assert_eq!(
                workspace.read(cx).config.folders(),
                ["Production", "Production/EU"]
            );
        });
    }

    #[gpui_kit::test]
    fn dragging_a_folder_into_another_moves_it(cx: &mut TestAppContext) {
        let mut config = config_with_connection();
        config.add_folder("", "Archive").unwrap();
        let (handle, workspace) = open(config, cx);
        window_action(handle, cx, |window, cx| {
            window.drag_to("folder-Production", "folder-Archive", cx);
        });
        cx.update(|cx| {
            assert_eq!(
                workspace.read(cx).config.connections[0].folder,
                "Archive/Production"
            );
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
        config.add_folder("", "Old").unwrap();
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
        config.add_folder("", "Local").unwrap();
        let (handle, workspace) = open(config, cx);
        window_action(handle, cx, |window, cx| {
            window.drag_to("conn-db", "folder-Local", cx);
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).config.connections[0].folder, "Local"));
    }

    #[gpui_kit::test]
    fn previewing_a_table_runs_its_query(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                let conn = this.config.connections[0].clone();
                this.selected = Some(conn.name.clone());
                this.schema.update(cx, |schema, cx| {
                    schema.set_connection(Some(conn), cx);
                    schema.tables = vec![tuplebase_core::db::TableInfo {
                        schema: "public".into(),
                        name: "users".into(),
                        kind: "BASE TABLE".into(),
                    }];
                    schema.load = crate::schema::Load::Ready;
                    cx.notify();
                });
                cx.notify();
            });
        });
        window_action(handle, cx, |window, cx| {
            window.hover("table-public-users", cx);
            window.click("preview-table-public-users", cx);
        });
        cx.update(|cx| {
            let this = workspace.read(cx);
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
