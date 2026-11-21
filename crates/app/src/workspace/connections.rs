//! The Connections dialog: the folder tree of saved connections, to open one in a tab.
//!
//! It shows the workspace's config as it is at each frame and changes it through the
//! workspace; what it keeps itself is how the tree looks: open folders, the cursor and
//! the folder name being typed.

use std::collections::HashSet;

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{
    Adapter, Config, Connection, FolderNode, folder_name, parent_of, rebase_folder,
};

use super::Workspace;
use crate::dialogs;
use crate::logos;
use crate::tree_nav::{self, TreeCollapse, TreeDown, TreeExpand, TreeOpen, TreeUp};

const UNPLUG_ICON: &[u8] = include_bytes!("../../../../assets/icons/unplug.svg");
const DELETE_ICON: &[u8] = include_bytes!("../../../../assets/icons/trash.svg");
const NEW_FOLDER_ICON: &[u8] = include_bytes!("../../../../assets/icons/folder-plus.svg");
const RENAME_ICON: &[u8] = include_bytes!("../../../../assets/icons/pencil.svg");

/// Indent per folder level.
const TREE_INDENT: f32 = 16.;
/// The dialog's size; it shrinks with a smaller window.
const WIDTH: f32 = 480.;
const MAX_HEIGHT: f32 = 560.;

/// Where the folder name field takes its own keys, ahead of the dialog's.
const FOLDER_NAME_CONTEXT: &str = "FolderName";

actions!(tuplebase, [CancelFolderEdit, CommitFolderEdit]);

/// Escape and Enter in the folder name field end the edit, and go no further: the
/// dialog would close on them. Bound after `gpui_kit::init`, so they win over the field's
/// own bindings.
pub(crate) fn key_bindings() -> [KeyBinding; 2] {
    let context = Some("FolderName > Input");
    [
        KeyBinding::new("escape", CancelFolderEdit, context),
        KeyBinding::new("enter", CommitFolderEdit, context),
    ]
}

/// One visible line of the connection tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TreeRow {
    Folder(String),
    Connection(String),
}

/// Inline name field in the tree, for creating or renaming a folder.
pub(crate) struct FolderEdit {
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
            .child(logos::icon(self.adapter, 16.))
            .child(self.name.clone())
    }
}

/// What the tree shows of the workspace, read once per frame.
struct Shown {
    config: Config,
    connected: HashSet<String>,
    /// The connections that have a tab.
    open: HashSet<String>,
    /// The tab on screen.
    active: Option<String>,
}

pub(crate) struct ConnectionsDialog {
    workspace: WeakEntity<Workspace>,
    tree_focus: FocusHandle,
    tree_scroll: ScrollHandle,
    /// The row arrow keys act on.
    pub(crate) cursor: Option<TreeRow>,
    tree_focused: bool,
    pub(crate) folder_edit: Option<FolderEdit>,
    _folder_edit_subscription: Option<Subscription>,
    /// Folders closed in the tree.
    pub(crate) collapsed: HashSet<String>,
}

impl ConnectionsDialog {
    pub(crate) fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        Self {
            workspace,
            tree_focus: cx.focus_handle(),
            tree_scroll: ScrollHandle::new(),
            cursor: None,
            tree_focused: false,
            folder_edit: None,
            _folder_edit_subscription: None,
            collapsed: HashSet::new(),
        }
    }

    /// Shows what is inside `folder`, such as after a connection was saved into it.
    pub(crate) fn reveal(&mut self, folder: &str) {
        self.collapsed.remove(folder);
    }

    fn shown(&self, cx: &App) -> Option<Shown> {
        let workspace = self.workspace.upgrade()?;
        let workspace = workspace.read(cx);
        Some(Shown {
            config: workspace.config.clone(),
            connected: workspace.connected.clone(),
            open: workspace
                .tabs
                .iter()
                .map(|open| open.name.clone())
                .collect(),
            active: workspace.active.clone(),
        })
    }

    fn config(&self, cx: &App) -> Config {
        self.workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).config.clone())
            .unwrap_or_default()
    }

    fn with_workspace(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        run: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>),
    ) {
        self.workspace
            .update(cx, |workspace, cx| run(workspace, window, cx))
            .ok();
    }

    /// Opens `name` in its tab, and leaves the dialog.
    fn open_connection(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.with_workspace(window, cx, |workspace, window, cx| {
            workspace.close_connections(window, cx);
            workspace.open_connection(name, window, cx);
            workspace.focus_active_editor(window, cx);
        });
    }

    /// Leaves the dialog for the connection form.
    fn open_form(
        &mut self,
        existing: Option<Connection>,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_workspace(window, cx, |workspace, window, cx| {
            workspace.close_connections(window, cx);
            workspace.open_form(existing, folder, window, cx);
        });
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
        let renamed = self.workspace.update(cx, |workspace, cx| {
            workspace.name_folder(&edit.parent, edit.renaming.as_deref(), &name, cx)
        });
        if let Ok(Some((old, new))) = renamed {
            self.rebase_collapsed(&old, &new);
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
        self.workspace
            .update(cx, |workspace, cx| workspace.remove_folder(folder, cx))
            .ok();
        self.collapsed.remove(folder);
        cx.notify();
    }

    fn confirm_remove_folder(
        &mut self,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let config = self.config(cx);
        let is_inside = |path: &str| path != folder && rebase_folder(path, &folder, "").is_some();
        let has_contents = config.folders().iter().any(|f| is_inside(f))
            || config
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
                .footer(dialogs::confirm_footer("Delete folder"))
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.remove_folder(&name, cx))
                        .ok();
                    true
                })
        });
    }

    fn move_folder(&mut self, path: &str, new_parent: &str, cx: &mut Context<Self>) {
        let moved = self.workspace.update(cx, |workspace, cx| {
            workspace.move_folder(path, new_parent, cx)
        });
        if let Ok(Some(new_path)) = moved {
            self.rebase_collapsed(path, &new_path);
            self.collapsed.remove(new_parent);
        }
        cx.notify();
    }

    fn move_connection(&mut self, name: &str, folder: &str, cx: &mut Context<Self>) {
        let moved = self.workspace.update(cx, |workspace, cx| {
            workspace.move_connection(name, folder, cx)
        });
        if matches!(moved, Ok(true)) {
            self.collapsed.remove(folder);
            cx.notify();
        }
    }

    /// Every row currently showing in the tree, top to bottom.
    fn visible_rows(&self, cx: &App) -> Vec<TreeRow> {
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
        walk(&self.config(cx).tree(), &self.collapsed, &mut rows);
        rows
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.visible_rows(cx);
        let current = self
            .cursor
            .as_ref()
            .and_then(|cursor| rows.iter().position(|row| row == cursor));
        let Some(next) = tree_nav::step(rows.len(), current, delta) else {
            return;
        };
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
                .config(cx)
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
    fn tree_expand(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            Some(TreeRow::Folder(path)) if self.collapsed.remove(&path) => cx.notify(),
            Some(TreeRow::Folder(_)) => self.move_cursor(1, cx),
            Some(TreeRow::Connection(name)) => self.open_connection(name, window, cx),
            None => {}
        }
    }

    fn tree_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.cursor.clone() {
            Some(TreeRow::Folder(path)) => self.toggle_folder(path, cx),
            Some(TreeRow::Connection(name)) => self.open_connection(name, window, cx),
            None => {}
        }
    }

    fn toggle_folder(&mut self, folder: String, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&folder) {
            self.collapsed.insert(folder);
        }
        cx.notify();
    }

    fn is_cursor(&self, row: &TreeRow) -> bool {
        self.tree_focused && self.cursor.as_ref() == Some(row)
    }

    fn render_tree(&self, shown: &Shown, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tree = shown.config.tree();
        let mut rows = Vec::new();
        self.render_new_folder_field("", 0, &mut rows, cx);
        self.render_folder_contents(shown, &tree, 0, &mut rows, cx);
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
        shown: &Shown,
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
                self.render_folder_contents(shown, folder, depth + 1, rows, cx);
            }
        }
        for conn in &node.connections {
            rows.push(
                self.render_connection(shown, conn, depth, cx)
                    .into_any_element(),
            );
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
            .key_context(FOLDER_NAME_CONTEXT)
            // The keys go back to the tree, so that the next Escape closes the dialog.
            .on_action(cx.listener(|this, _: &CancelFolderEdit, window, cx| {
                this.cancel_folder_edit(cx);
                this.tree_focus.focus(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommitFolderEdit, window, cx| {
                this.commit_folder_edit(cx);
                this.tree_focus.focus(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.commit_folder_edit(cx)))
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
                .cursor_pointer()
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
            .context_menu(self.folder_menu(folder, cx))
    }

    fn render_connection(
        &self,
        shown: &Shown,
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
        let selected = shown.active.as_deref() == Some(conn.name.as_str());
        let connected = shown.connected.contains(&conn.name);
        let success = cx.theme().success;
        let row_group = SharedString::from(format!("conn-row-{}", conn.name));
        let name = conn.name.clone();
        let folder = conn.folder.clone();
        let closing = conn.name.clone();
        h_flex()
            .id(SharedString::from(format!("conn-{}", conn.name)))
            .test_support()
            .group(row_group.clone())
            .border_1()
            .border_color(if self.is_cursor(&TreeRow::Connection(conn.name.clone())) {
                ring
            } else {
                transparent_black()
            })
            .relative()
            .h(px(32.))
            .gap_2()
            .pr_2()
            .pl(px(8. + depth as f32 * TREE_INDENT))
            .rounded(radius)
            .cursor_pointer()
            .when(selected, |d| d.bg(active))
            .hover(move |s| s.bg(active))
            .child(logos::icon(conn.adapter, 18.))
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
                    .child(conn.location()),
            )
            .when(connected, |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!("conn-status-{}", conn.name)))
                        .test_support()
                        .flex_none()
                        .size(px(8.))
                        .rounded_full()
                        .bg(success)
                        .group_hover(row_group.clone(), |s| s.invisible()),
                )
                .child(
                    h_flex()
                        .absolute()
                        .right_1()
                        .invisible()
                        .group_hover(row_group, |s| s.visible())
                        .child(
                            Button::new(SharedString::from(format!("disconnect-{}", conn.name)))
                                .cursor_pointer()
                                .ghost()
                                .xsmall()
                                .icon(Icon::default().data(UNPLUG_ICON))
                                .tooltip("Disconnect")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    let closing = closing.clone();
                                    this.with_workspace(window, cx, |workspace, _, cx| {
                                        workspace.close_tab(closing, cx)
                                    });
                                })),
                        ),
                )
            })
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
            .on_click(cx.listener(move |this, _, window, cx| {
                this.cursor = Some(TreeRow::Connection(name.clone()));
                this.open_connection(name.clone(), window, cx);
            }))
            .context_menu(self.connection_menu(shown, conn, cx))
    }

    fn connection_menu(
        &self,
        shown: &Shown,
        conn: &Connection,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static + use<>
    {
        let this = cx.entity().downgrade();
        let workspace = self.workspace.clone();
        let conn = conn.clone();
        // A connection with a tab can always be closed, connected or not.
        let connected = shown.connected.contains(&conn.name) || shown.open.contains(&conn.name);
        move |menu, _, _| {
            let (edited, name, closing) = (conn.clone(), conn.name.clone(), conn.name.clone());
            let menu = if connected {
                menu.item(
                    PopupMenuItem::new("Disconnect")
                        .on_click(menu_click(&workspace, move |workspace, _, cx| {
                            workspace.close_tab(closing.clone(), cx)
                        })),
                )
                .separator()
            } else {
                menu
            };
            menu.item(PopupMenuItem::new("Edit connection").on_click(menu_click(
                &this,
                move |this, window, cx| {
                    let folder = edited.folder.clone();
                    this.open_form(Some(edited.clone()), folder, window, cx)
                },
            )))
            .separator()
            .item(
                // "connection", so it's never mistaken for dropping the database.
                PopupMenuItem::new("Delete connection")
                    .on_click(menu_click(&workspace, move |workspace, window, cx| {
                        workspace.confirm_delete(name.clone(), window, cx)
                    })),
            )
        }
    }

    fn folder_menu(
        &self,
        folder: &str,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static + use<>
    {
        let this = cx.entity().downgrade();
        let folder = folder.to_owned();
        move |menu, _, _| {
            let (add_in, new_in, rename, remove) = (
                folder.clone(),
                folder.clone(),
                folder.clone(),
                folder.clone(),
            );
            menu.item(
                PopupMenuItem::new("New connection")
                    .on_click(menu_click(&this, move |this, window, cx| {
                        this.open_form(None, add_in.clone(), window, cx)
                    })),
            )
            .item(
                PopupMenuItem::new("New folder").on_click(menu_click(
                    &this,
                    move |this, window, cx| {
                        this.begin_folder_edit(new_in.clone(), None, window, cx)
                    },
                )),
            )
            .separator()
            .item(PopupMenuItem::new("Rename folder").on_click(menu_click(
                &this,
                move |this, window, cx| {
                    this.begin_folder_edit(String::new(), Some(rename.clone()), window, cx);
                },
            )))
            .item(
                PopupMenuItem::new("Delete folder")
                    .on_click(menu_click(&this, move |this, window, cx| {
                        this.confirm_remove_folder(remove.clone(), window, cx)
                    })),
            )
        }
    }
}

impl Render for ConnectionsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tree_focused = self.tree_focus.is_focused(window);
        let Some(shown) = self.shown(cx) else {
            return div().into_any_element();
        };
        let height = px(MAX_HEIGHT).min(window.viewport_size().height * 0.75);
        let empty = shown.config.connections.is_empty() && shown.config.folders().is_empty();
        let muted = cx.theme().muted_foreground;
        v_flex()
            .id("connections-dialog")
            .test_support()
            .h(height)
            .child(
                h_flex()
                    .flex_none()
                    .h(px(44.))
                    .pl_4()
                    // Clear of the close icon in the corner.
                    .pr(px(44.))
                    .gap_1()
                    .child(
                        div()
                            .id("connections-title")
                            .test_support()
                            .flex_1()
                            .font_semibold()
                            .child("Connections"),
                    )
                    .child(
                        Button::new("add-folder")
                            .cursor_pointer()
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
                            .cursor_pointer()
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("New connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_form(None, String::new(), window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("connections")
                    .track_focus(&self.tree_focus)
                    // The folder name field sits inside the tree and passes on keys it has
                    // no use for; they must not reach the tree.
                    .when(self.folder_edit.is_none(), |el| {
                        el.key_context(tree_nav::CONTEXT)
                    })
                    .track_scroll(&self.tree_scroll)
                    .on_action(cx.listener(|this, _: &TreeUp, _, cx| this.move_cursor(-1, cx)))
                    .on_action(cx.listener(|this, _: &TreeDown, _, cx| this.move_cursor(1, cx)))
                    .on_action(cx.listener(|this, _: &TreeCollapse, _, cx| this.tree_collapse(cx)))
                    .on_action(
                        cx.listener(|this, _: &TreeExpand, window, cx| {
                            this.tree_expand(window, cx)
                        }),
                    )
                    .on_action(
                        cx.listener(|this, _: &TreeOpen, window, cx| this.tree_open(window, cx)),
                    )
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .pb_2()
                    .gap_px()
                    .overflow_y_scroll()
                    .on_drop(cx.listener(|this, dragged: &DraggedConnection, _, cx| {
                        this.move_connection(&dragged.name, "", cx);
                    }))
                    .on_drop(cx.listener(|this, dragged: &DraggedFolder, _, cx| {
                        this.move_folder(&dragged.path, "", cx);
                    }))
                    .when(empty && self.folder_edit.is_none(), |el| {
                        el.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(muted)
                                .child("No connections yet. Add one with +."),
                        )
                    })
                    .children(self.render_tree(&shown, cx)),
            )
            .into_any_element()
    }
}

impl Workspace {
    /// Opens the Connections dialog, or closes it when it is open. Does nothing while
    /// another dialog is open.
    pub(super) fn toggle_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_dialog = window.has_active_dialog(cx);
        if self.connections_open && has_dialog {
            self.close_connections(window, cx);
            return;
        }
        // Closed some way that didn't say so.
        self.connections_open = false;
        if has_dialog {
            return;
        }
        self.connections_open = true;
        let (dialog, this) = (self.connections.clone(), cx.entity().downgrade());
        window.open_dialog(cx, move |modal, _, _| {
            let this = this.clone();
            modal
                .w(px(WIDTH))
                .p_0()
                .close_button(false)
                .on_close(move |_, _, cx| {
                    this.update(cx, |this, _| this.connections_open = false)
                        .ok();
                })
                .child(dialogs::with_close_button(dialog.clone()))
        });
        let focus = self.connections.read(cx).tree_focus.clone();
        focus.focus(window, cx);
        cx.notify();
    }

    /// Closes the Connections dialog, if it is open.
    pub(super) fn close_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.connections_open) && window.has_active_dialog(cx) {
            window.close_dialog(cx);
        }
    }
}

fn menu_click<T: 'static>(
    this: &WeakEntity<T>,
    run: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let this = this.clone();
    move |_, window, cx| {
        this.update(cx, |this, cx| run(this, window, cx)).ok();
    }
}
