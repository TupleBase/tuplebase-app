//! The Connections dialog: the folder tree of saved connections, to find one and open it
//! in a tab. A search and filter chips narrow the tree; the pane beside it shows the
//! selected connection and what can be done with it.
//!
//! It shows the workspace's config as it is at each frame and changes it through the
//! workspace; what it keeps itself is how the tree looks: the search, the filter, open
//! folders, the selected row and the folder name being typed.

use std::collections::HashSet;

use gpui_kit::base::TestTextExt as _;
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
use crate::connection_tab::{Light, light};
use crate::dialogs;
use crate::logos;
use crate::tree_nav::{self, TreeCollapse, TreeDown, TreeExpand, TreeOpen, TreeUp};

const UNPLUG_ICON: &[u8] = include_bytes!("../../../../assets/icons/unplug.svg");
const DELETE_ICON: &[u8] = include_bytes!("../../../../assets/icons/trash.svg");
const NEW_FOLDER_ICON: &[u8] = include_bytes!("../../../../assets/icons/folder-plus.svg");
const RENAME_ICON: &[u8] = include_bytes!("../../../../assets/icons/pencil.svg");

/// Indent per folder level.
const TREE_INDENT: f32 = 18.;
/// A connection's row in the tree.
const ROW_HEIGHT: f32 = 38.;
/// The pane that shows the selected connection.
const DETAILS_WIDTH: f32 = 300.;
/// How many connections "Recent" keeps.
const RECENT_COUNT: usize = 10;

/// Where the folder name field takes its own keys, ahead of the dialog's.
const FOLDER_NAME_CONTEXT: &str = "FolderName";
/// Where the search field gives the tree's keys to the tree.
const SEARCH_CONTEXT: &str = "ConnectionsSearch";

actions!(tuplebase, [CancelFolderEdit, CommitFolderEdit]);

/// Escape and Enter in the folder name field end the edit, and go no further: the
/// dialog would close on them. Up, Down and Enter in the search field move through the
/// tree and open the selected row, while the field keeps the other keys for typing.
/// Bound after `gpui_kit::init`, so they win over the fields' own bindings.
pub(crate) fn key_bindings() -> [KeyBinding; 5] {
    let folder = Some("FolderName > Input");
    let search = Some("ConnectionsSearch > Input");
    [
        KeyBinding::new("escape", CancelFolderEdit, folder),
        KeyBinding::new("enter", CommitFolderEdit, folder),
        KeyBinding::new("up", TreeUp, search),
        KeyBinding::new("down", TreeDown, search),
        KeyBinding::new("enter", TreeOpen, search),
    ]
}

/// The dialog's size in a window showing `viewport`: as large as it is designed, and
/// smaller in a small window.
pub(crate) fn dialog_size(viewport: gpui_kit::Size<Pixels>) -> gpui_kit::Size<Pixels> {
    size(
        px(900.).min(viewport.width * 0.75),
        px(620.).min(viewport.height * 0.82),
    )
}

/// Which connections the tree shows, as the chips above it choose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Filter {
    #[default]
    All,
    Connected,
    /// Those queries last ran on.
    Recent,
}

impl Filter {
    const ALL: [Filter; 3] = [Filter::All, Filter::Connected, Filter::Recent];

    fn id(self) -> &'static str {
        match self {
            Filter::All => "filter-all",
            Filter::Connected => "filter-connected",
            Filter::Recent => "filter-recent",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Connected => "Connected",
            Filter::Recent => "Recent",
        }
    }
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
            .child(logos::badge(self.adapter, 18.))
            .child(self.name.clone())
    }
}

/// What the tree shows of the workspace, read once per frame.
struct Shown {
    config: Config,
    connected: HashSet<String>,
    /// The connections that have a tab.
    open: HashSet<String>,
    /// The tabs still connecting.
    connecting: HashSet<String>,
    /// The connections queries last ran on.
    recent: HashSet<String>,
    /// The tab on screen.
    active: Option<String>,
    /// What the search field holds, trimmed.
    query: String,
    filter: Filter,
}

impl Shown {
    /// Whether the search or a chip hides some connections.
    fn filtering(&self) -> bool {
        !self.query.is_empty() || self.filter != Filter::All
    }

    fn keeps(&self, conn: &Connection) -> bool {
        let chosen = match self.filter {
            Filter::All => true,
            Filter::Connected => self.connected.contains(&conn.name),
            Filter::Recent => self.recent.contains(&conn.name),
        };
        chosen && conn.matches(&self.query)
    }

    /// The tree as it shows: everything, or only the folders that hold a connection the
    /// search and the chip keep.
    fn tree(&self) -> FolderNode<'_> {
        let tree = self.config.tree();
        if self.filtering() {
            tree.filtered(|conn| self.keeps(conn))
        } else {
            tree
        }
    }

    fn light(&self, name: &str) -> Light {
        light(
            self.connecting.contains(name),
            self.connected.contains(name),
        )
    }
}

pub(crate) struct ConnectionsDialog {
    workspace: WeakEntity<Workspace>,
    /// Holds the keys whenever the dialog is the one in front.
    focus: FocusHandle,
    tree_focus: FocusHandle,
    tree_scroll: ScrollHandle,
    /// The row arrow keys act on.
    pub(crate) cursor: Option<TreeRow>,
    pub(crate) folder_edit: Option<FolderEdit>,
    _folder_edit_subscription: Option<Subscription>,
    /// Folders closed in the tree. A search or a chip shows every folder open, and
    /// leaves these as they are for when it ends.
    pub(crate) collapsed: HashSet<String>,
    pub(crate) search: Entity<InputState>,
    _search_subscription: Subscription,
    pub(crate) filter: Filter,
}

impl ConnectionsDialog {
    pub(crate) fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search by name, folder or host"));
        let search_subscription =
            cx.subscribe_in(&search, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.select_first_match(cx);
                    cx.notify();
                }
            });
        Self {
            workspace,
            focus: cx.focus_handle(),
            tree_focus: cx.focus_handle(),
            tree_scroll: ScrollHandle::new(),
            cursor: None,
            folder_edit: None,
            _folder_edit_subscription: None,
            collapsed: HashSet::new(),
            search,
            _search_subscription: search_subscription,
            filter: Filter::All,
        }
    }

    /// Starts afresh each time the dialog opens: no search, every connection, the tab on
    /// screen (`active`, in `folder`) selected and shown, and the keys in the search
    /// field. Open folders stay.
    fn prepare(
        &mut self,
        active: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter = Filter::All;
        self.search.update(cx, |search, cx| {
            search.set_value("", window, cx);
            search.focus(window, cx);
        });
        if let Some((name, mut folder)) = active {
            while !folder.is_empty() {
                self.collapsed.remove(&folder);
                folder = parent_of(&folder).to_owned();
            }
            self.cursor = Some(TreeRow::Connection(name));
        }
        cx.notify();
    }

    /// Gives the keys back to the tree once a question asked from it has closed: closing
    /// returns them to the menu it was asked from, which is gone.
    pub(crate) fn take_keys_back(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let focus = this.read(cx).tree_focus.clone();
        window.defer(cx, move |window, cx| focus.focus(window, cx));
    }

    /// Whether the dialog is in front, with the keys, rather than under another one.
    fn in_front(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
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
            connecting: workspace
                .tabs
                .iter()
                .filter(|open| open.tab.read(cx).connecting)
                .map(|open| open.name.clone())
                .collect(),
            recent: workspace
                .session
                .recent_connections(RECENT_COUNT)
                .into_iter()
                .map(str::to_owned)
                .collect(),
            active: workspace.active.clone(),
            query: self.search.read(cx).value().trim().to_owned(),
            filter: self.filter,
        })
    }

    fn set_filter(&mut self, filter: Filter, window: &mut Window, cx: &mut Context<Self>) {
        self.filter = filter;
        self.keep_cursor_in_view(cx);
        // Ready to type a search into what the chip kept.
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
        cx.notify();
    }

    /// After typing in the search, selects the first connection it finds, for Enter to
    /// open. With the search emptied, the selection stays where it is if it shows.
    fn select_first_match(&mut self, cx: &App) {
        let searching = self.shown(cx).is_some_and(|shown| !shown.query.is_empty());
        if searching {
            self.cursor = None;
        }
        self.keep_cursor_in_view(cx);
    }

    /// Moves the selection to the first connection showing once a search or a chip hid
    /// the row it was on.
    fn keep_cursor_in_view(&mut self, cx: &App) {
        let Some(shown) = self.shown(cx) else {
            return;
        };
        let rows = self.visible_rows(&shown);
        if self.cursor.as_ref().is_some_and(|row| rows.contains(row)) {
            return;
        }
        self.cursor = rows
            .iter()
            .find(|row| matches!(row, TreeRow::Connection(_)))
            .or(rows.first())
            .cloned();
    }

    /// Whether the tree shows what is inside `folder`.
    fn is_open(&self, shown: &Shown, folder: &str) -> bool {
        shown.filtering() || !self.collapsed.contains(folder)
    }

    /// The folder the selection is in, or is: where a new connection goes.
    fn selected_folder(&self, cx: &App) -> String {
        match &self.cursor {
            Some(TreeRow::Folder(path)) => path.clone(),
            Some(TreeRow::Connection(name)) => self
                .config(cx)
                .find(name)
                .map(|c| c.folder.clone())
                .unwrap_or_default(),
            None => String::new(),
        }
    }

    /// Selects `row` and gives the keys to the tree, as a click on it does.
    fn select(&mut self, row: TreeRow, window: &mut Window, cx: &mut Context<Self>) {
        self.cursor = Some(row);
        self.tree_focus.focus(window, cx);
        cx.notify();
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
        if renaming.is_none() {
            // A new folder is empty, and a search or a chip would hide it.
            self.filter = Filter::All;
            self.search
                .update(cx, |search, cx| search.set_value("", window, cx));
        }
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
                .on_ok({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| this.remove_folder(&name, cx))
                            .ok();
                        true
                    }
                })
                .on_close(move |_, window, cx| {
                    if let Some(this) = this.upgrade() {
                        Self::take_keys_back(&this, window, cx);
                    }
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
    fn visible_rows(&self, shown: &Shown) -> Vec<TreeRow> {
        fn walk(
            this: &ConnectionsDialog,
            shown: &Shown,
            node: &FolderNode<'_>,
            rows: &mut Vec<TreeRow>,
        ) {
            for folder in &node.folders {
                rows.push(TreeRow::Folder(folder.path.clone()));
                if this.is_open(shown, &folder.path) {
                    walk(this, shown, folder, rows);
                }
            }
            for conn in &node.connections {
                rows.push(TreeRow::Connection(conn.name.clone()));
            }
        }
        let mut rows = Vec::new();
        walk(self, shown, &shown.tree(), &mut rows);
        rows
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(shown) = self.shown(cx) else {
            return;
        };
        let rows = self.visible_rows(&shown);
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

    /// Left arrow: closes an open folder, otherwise jumps to the folder above. While a
    /// search or a chip shows every folder open, it only jumps.
    fn tree_collapse(&mut self, cx: &mut Context<Self>) {
        let filtering = self.shown(cx).is_some_and(|shown| shown.filtering());
        let parent = match self.cursor.clone() {
            Some(TreeRow::Folder(path)) if !filtering && !self.collapsed.contains(&path) => {
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
        let filtering = self.shown(cx).is_some_and(|shown| shown.filtering());
        match self.cursor.clone() {
            Some(TreeRow::Folder(path)) if !filtering && self.collapsed.remove(&path) => {
                cx.notify()
            }
            Some(TreeRow::Folder(_)) => self.move_cursor(1, cx),
            Some(TreeRow::Connection(name)) => self.open_connection(name, window, cx),
            None => {}
        }
    }

    /// Enter: opens the selected connection, or opens or closes the selected folder.
    fn tree_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filtering = self.shown(cx).is_some_and(|shown| shown.filtering());
        match self.cursor.clone() {
            Some(TreeRow::Folder(_)) if filtering => {}
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

    /// Whether `row` is selected. The selection shows whether the tree or the search
    /// field has the keys, since both move it.
    fn is_cursor(&self, row: &TreeRow) -> bool {
        self.cursor.as_ref() == Some(row)
    }

    fn render_tree(&self, shown: &Shown, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tree = shown.tree();
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
                    .render_folder(shown, folder, depth, cx)
                    .into_any_element(),
            });
            if self.is_open(shown, &folder.path) {
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
        shown: &Shown,
        node: &FolderNode<'_>,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let folder = node.path.as_str();
        let theme = cx.theme();
        let (muted, hover, selected, radius, ring, success) = (
            theme.muted_foreground,
            theme.accent,
            theme.sidebar_accent,
            theme.radius,
            theme.ring,
            theme.success,
        );
        let chevron = if self.is_open(shown, folder) {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        let total = node.connection_count();
        let connected = node.count(|c| shown.connected.contains(&c.name));
        let is_cursor = self.is_cursor(&TreeRow::Folder(folder.to_owned()));
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
            .border_color(if is_cursor { ring } else { transparent_black() })
            .when(is_cursor, |d| d.bg(selected))
            .group(row_group.clone())
            .relative()
            .when(depth == 0, |d| d.mt_1())
            .gap_1p5()
            .pl(px(6. + depth as f32 * TREE_INDENT))
            .pr_2()
            .h(px(32.))
            .rounded(radius)
            .cursor_pointer()
            .text_color(muted)
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
                h_flex()
                    .id(SharedString::from(format!("folder-count-{folder}")))
                    .test_support()
                    .gap_1()
                    .text_xs()
                    .group_hover(row_group.clone(), |s| s.invisible())
                    .when(connected > 0, |d| {
                        d.child(status_dot(Light::Connected, success, muted))
                            .child(text(format!("{connected} / {total}")))
                    })
                    .when(connected == 0, |d| d.child(text(total.to_string()))),
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
                this.select(TreeRow::Folder(toggle.clone()), window, cx);
                // While a search or a chip shows every folder open, a click only selects.
                if !this.shown(cx).is_some_and(|shown| shown.filtering()) {
                    this.toggle_folder(toggle.clone(), cx);
                }
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
        let (muted, hover, selected_bg, radius, ring) = (
            theme.muted_foreground,
            theme.accent,
            theme.sidebar_accent,
            theme.radius,
            theme.ring,
        );
        let on_screen = shown.active.as_deref() == Some(conn.name.as_str());
        let selected = self.is_cursor(&TreeRow::Connection(conn.name.clone()));
        let light = shown.light(&conn.name);
        let lit = light != Light::Off;
        let connected = shown.connected.contains(&conn.name);
        let dot_color = light_color(light, cx);
        let row_group = SharedString::from(format!("conn-row-{}", conn.name));
        let name = conn.name.clone();
        let folder = conn.folder.clone();
        let closing = conn.name.clone();
        h_flex()
            .id(SharedString::from(format!("conn-{}", conn.name)))
            .test_support()
            .group(row_group.clone())
            .border_1()
            .border_color(if selected { ring } else { transparent_black() })
            .relative()
            .flex_none()
            .h(px(ROW_HEIGHT))
            .gap_2p5()
            .pr_3()
            .pl(px(10. + depth as f32 * TREE_INDENT))
            .rounded(radius)
            .cursor_pointer()
            .when(selected, |d| d.bg(selected_bg))
            .when(!selected, |d| d.hover(move |s| s.bg(hover)))
            .child(logos::badge(conn.adapter, 22.))
            .child(
                div()
                    .flex_none()
                    .max_w(px(220.))
                    .truncate()
                    .text_sm()
                    .when(on_screen, |d| d.font_semibold())
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
            .when(lit, |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!("conn-status-{}", conn.name)))
                        .test_support()
                        // Disconnect takes its place.
                        .when(connected, |d| {
                            d.group_hover(row_group.clone(), |s| s.invisible())
                        })
                        .child(status_dot(light, dot_color, muted)),
                )
            })
            .when(!lit, |row| row.child(status_dot(light, dot_color, muted)))
            .when(connected, |row| {
                row.child(
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
            // A click selects; a second one opens.
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.select(TreeRow::Connection(name.clone()), window, cx);
                if event.click_count() >= 2 {
                    this.open_connection(name.clone(), window, cx);
                }
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

impl ConnectionsDialog {
    fn render_header(&self, shown: &Shown, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, success) = (theme.muted_foreground, theme.success);
        let saved = shown.config.connections.len();
        let connected = shown
            .config
            .connections
            .iter()
            .filter(|c| shown.connected.contains(&c.name))
            .count();
        h_flex()
            .w_full()
            .flex_none()
            .gap_3()
            .pt(px(18.))
            .pb(px(14.))
            .pl_5()
            .pr_3()
            .child(
                div()
                    .id("connections-title")
                    .test_support()
                    .text_lg()
                    .font_semibold()
                    .child("Connections"),
            )
            .child(
                h_flex()
                    .id("connections-summary")
                    .test_support()
                    .flex_1()
                    .gap_1()
                    .text_sm()
                    .text_color(muted)
                    .child(text(format!("{saved} saved ·")))
                    .child(status_dot(Light::Connected, success, muted))
                    .child(text(format!("{connected} connected"))),
            )
            .child(div().ml_auto().child(dialogs::close_button()))
    }

    fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chips: Vec<_> = Filter::ALL
            .into_iter()
            .map(|filter| self.render_chip(filter, cx).into_any_element())
            .collect();
        h_flex()
            .flex_none()
            .gap_2p5()
            .px_5()
            .pb(px(14.))
            .child(
                div().flex_1().min_w_0().key_context(SEARCH_CONTEXT).child(
                    Input::new(&self.search)
                        .id("connections-search")
                        .prefix(Icon::new(IconName::Search).small()),
                ),
            )
            .child(h_flex().flex_none().gap_1p5().children(chips))
    }

    fn render_chip(&self, filter: Filter, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let chosen = self.filter == filter;
        let (selected, border, hover, muted, success) = (
            theme.sidebar_accent,
            theme.input,
            theme.accent,
            theme.muted_foreground,
            theme.success,
        );
        h_flex()
            .id(filter.id())
            .test_support()
            .gap_1()
            .h(px(30.))
            .px_3()
            .rounded_full()
            .border_1()
            .text_sm()
            .cursor_pointer()
            .map(|d| {
                if chosen {
                    d.bg(selected).border_color(selected)
                } else {
                    d.border_color(border).hover(move |s| s.bg(hover))
                }
            })
            .when(filter == Filter::Connected, |d| {
                d.child(status_dot(Light::Connected, success, muted))
            })
            .child(filter.label())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.set_filter(filter, window, cx);
            }))
    }

    fn render_details(&self, shown: &Shown, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, background) = (theme.muted_foreground, theme.sidebar);
        let pane = v_flex()
            .id("connection-details")
            .test_support()
            .flex_none()
            .w(px(DETAILS_WIDTH))
            .p_5()
            .gap(px(14.))
            .bg(background);
        let conn = match &self.cursor {
            Some(TreeRow::Connection(name)) => shown.config.find(name),
            _ => None,
        };
        let Some(conn) = conn else {
            return pane
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child("Select a connection to see it here.")
                .into_any_element();
        };
        let light = shown.light(&conn.name);
        let has_tab = shown.open.contains(&conn.name);
        let connected = shown.connected.contains(&conn.name);
        let status = match (light, has_tab) {
            (Light::Connecting, _) => "Connecting…",
            (Light::Connected, true) => "Connected, open in a tab",
            (Light::Connected, false) => "Connected",
            (Light::Off, true) => "Open in a tab, not connected",
            (Light::Off, false) => "Not connected",
        };
        let folder = if conn.folder.is_empty() {
            "No folder".to_owned()
        } else {
            conn.folder.split('/').collect::<Vec<_>>().join(" / ")
        };
        // Only what the config holds: nothing is looked up, and never a password.
        let (place_label, place) = if conn.adapter.is_file() {
            ("File", conn.database.clone())
        } else if conn.location() != conn.host {
            ("Host", format!("from {}", conn.location()))
        } else {
            ("Host", format!("{}:{}", conn.host, conn.port))
        };
        let user = if conn.user.is_empty() {
            "—".to_owned()
        } else {
            conn.user.clone()
        };
        let access = if conn.readonly {
            "Read-only"
        } else {
            "Read and write"
        };
        let dot = status_dot(light, light_color(light, cx), muted);
        let field = |label: &'static str, value: AnyElement| {
            h_flex()
                .gap_2p5()
                .items_start()
                .child(div().flex_none().w(px(80.)).text_color(muted).child(label))
                .child(div().flex_1().min_w_0().child(value))
        };
        let (opened, edited, closing, deleted) = (
            conn.name.clone(),
            conn.clone(),
            conn.name.clone(),
            conn.name.clone(),
        );
        pane.child(
            h_flex()
                .gap_3()
                .child(logos::badge(conn.adapter, 40.))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            div()
                                .id("details-name")
                                .test_support()
                                .truncate()
                                .text_base()
                                .font_semibold()
                                .child(text(conn.name.clone())),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(muted)
                                .child(text(folder)),
                        ),
                ),
        )
        .child(
            v_flex()
                .gap_2()
                .text_sm()
                .child(field(
                    "Status",
                    h_flex()
                        .id("details-status")
                        .test_support()
                        .gap_1p5()
                        .child(dot)
                        .child(text(status))
                        .into_any_element(),
                ))
                .child(field(
                    "Engine",
                    text(conn.adapter.label()).into_any_element(),
                ))
                .child(field(
                    place_label,
                    div()
                        .id("details-host")
                        .test_support()
                        .child(text(place))
                        .into_any_element(),
                ))
                .child(field("User", text(user).into_any_element()))
                .child(field("Access", text(access).into_any_element())),
        )
        .child(div().flex_1())
        .child(
            Button::new("details-open")
                .cursor_pointer()
                .primary()
                .w_full()
                .label(if has_tab {
                    "Go to tab ↵"
                } else {
                    "Connect ↵"
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_connection(opened.clone(), window, cx);
                })),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("details-edit")
                        .cursor_pointer()
                        .outline()
                        .flex_1()
                        .label("Edit")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let folder = edited.folder.clone();
                            this.open_form(Some(edited.clone()), folder, window, cx);
                        })),
                )
                // A connection with a tab can always be closed, connected or not.
                .when(connected || has_tab, |row| {
                    row.child(
                        Button::new("details-disconnect")
                            .cursor_pointer()
                            .outline()
                            .flex_1()
                            .label("Disconnect")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let closing = closing.clone();
                                this.with_workspace(window, cx, |workspace, _, cx| {
                                    workspace.close_tab(closing, cx)
                                });
                                // The button goes with the connection: the keys stay
                                // in the dialog.
                                this.tree_focus.focus(window, cx);
                            })),
                    )
                })
                .child(
                    Button::new("details-delete")
                        .cursor_pointer()
                        .outline()
                        .icon(Icon::default().data(DELETE_ICON))
                        .tooltip("Delete connection")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let deleted = deleted.clone();
                            this.with_workspace(window, cx, |workspace, window, cx| {
                                workspace.confirm_delete(deleted, window, cx)
                            });
                        })),
                ),
        )
        .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        h_flex()
            .flex_none()
            .gap_2p5()
            .px_5()
            .py_3()
            .border_t_1()
            .border_color(border)
            .child(
                Button::new("add-connection")
                    .cursor_pointer()
                    .outline()
                    .icon(IconName::Plus)
                    .label("New connection")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let folder = this.selected_folder(cx);
                        this.open_form(None, folder, window, cx);
                    })),
            )
            .child(
                Button::new("add-folder")
                    .cursor_pointer()
                    .outline()
                    .icon(Icon::default().data(NEW_FOLDER_ICON))
                    .label("New folder")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.begin_folder_edit(String::new(), None, window, cx);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .text_xs()
                    .text_color(muted)
                    .child("↑↓ move · ↵ open · drag to move between folders · Esc close"),
            )
    }
}

impl Render for ConnectionsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(shown) = self.shown(cx) else {
            return div().into_any_element();
        };
        let height = dialog_size(window.viewport_size()).height;
        let empty = shown.config.connections.is_empty() && shown.config.folders().is_empty();
        let rows = self.render_tree(&shown, cx);
        let no_match = shown.filtering() && !empty && shown.tree().connection_count() == 0;
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        v_flex()
            .id("connections-dialog")
            .test_support()
            .track_focus(&self.focus)
            .h(height)
            // Here, not on the tree, so that the keys the search field passes on reach them.
            .on_action(cx.listener(|this, _: &TreeUp, _, cx| this.move_cursor(-1, cx)))
            .on_action(cx.listener(|this, _: &TreeDown, _, cx| this.move_cursor(1, cx)))
            .on_action(cx.listener(|this, _: &TreeCollapse, _, cx| this.tree_collapse(cx)))
            .on_action(cx.listener(|this, _: &TreeExpand, window, cx| this.tree_expand(window, cx)))
            .on_action(cx.listener(|this, _: &TreeOpen, window, cx| this.tree_open(window, cx)))
            .child(self.render_header(&shown, cx))
            .child(self.render_search(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .id("connections")
                            .track_focus(&self.tree_focus)
                            // The folder name field sits inside the tree and passes on keys
                            // it has no use for; they must not reach the tree.
                            .when(self.folder_edit.is_none(), |el| {
                                el.key_context(tree_nav::CONTEXT)
                            })
                            .track_scroll(&self.tree_scroll)
                            .flex_1()
                            .min_w_0()
                            .px_3()
                            .py_2p5()
                            .gap_0p5()
                            .border_r_1()
                            .border_color(border)
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
                                        .child("No connections yet. Add one with New connection."),
                                )
                            })
                            .when(no_match, |el| {
                                el.child(
                                    div()
                                        .id("connections-no-match")
                                        .test_support()
                                        .p_4()
                                        .text_sm()
                                        .text_color(muted)
                                        .child("No connection matches."),
                                )
                            })
                            .children(rows),
                    )
                    .child(self.render_details(&shown, cx)),
            )
            .child(self.render_footer(cx))
            .into_any_element()
    }
}

/// Text the UI tests can read back.
fn text(text: impl Into<SharedString>) -> impl IntoElement {
    let text = text.into();
    div().child(text.clone()).test_text(text)
}

/// The color of a connection's light.
fn light_color(light: Light, cx: &App) -> Hsla {
    let theme = cx.theme();
    match light {
        Light::Connecting => theme.warning,
        Light::Connected => theme.success,
        Light::Off => theme.muted_foreground,
    }
}

/// A connection's light: a filled dot when lit, a ring when not.
fn status_dot(light: Light, color: Hsla, muted: Hsla) -> Div {
    let dot = div().flex_none().size(px(8.)).rounded_full();
    match light {
        Light::Off => dot.border_1().border_color(muted),
        Light::Connecting | Light::Connected => dot.bg(color),
    }
}

impl Workspace {
    /// Opens the Connections dialog, or closes it when it is open. Does nothing while
    /// another dialog is open, including one in front of it.
    pub(super) fn toggle_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_dialog = window.has_active_dialog(cx);
        if self.connections_open && has_dialog {
            if self.connections.read(cx).in_front(window, cx) {
                self.close_connections(window, cx);
            }
            return;
        }
        // Closed some way that didn't say so.
        self.connections_open = false;
        if has_dialog {
            return;
        }
        self.connections_open = true;
        let (dialog, this) = (self.connections.clone(), cx.entity().downgrade());
        window.open_dialog(cx, move |modal, window, _| {
            let this = this.clone();
            modal
                .w(dialog_size(window.viewport_size()).width)
                .centered()
                .p_0()
                .close_button(false)
                .on_close(move |_, _, cx| {
                    this.update(cx, |this, _| this.connections_open = false)
                        .ok();
                })
                .child(dialog.clone())
        });
        let active = self.active.clone().map(|name| {
            let folder = self.config.find(&name).map(|c| c.folder.clone());
            (name, folder.unwrap_or_default())
        });
        self.connections
            .update(cx, |dialog, cx| dialog.prepare(active, window, cx));
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

#[cfg(test)]
mod tests {
    // No globs from gpui_kit here: its `test` macro would shadow the built-in `#[test]`.
    use super::super::tests::{
        config_with_connection, config_with_two_connections, open, open_picker, open_tab,
        window_action,
    };
    use super::{ConnectionsDialog, Filter, TreeRow, dialog_size};
    use crate::workspace::Workspace;
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{ElementId, Entity, SharedString, TestAppContext, Window, px, size};
    use tuplebase_core::config::{Adapter, Config, Connection};

    /// Production holds `db` and `other`; Staging holds `shop`, a MySQL server elsewhere.
    fn config_with_folders() -> Config {
        let mut config = config_with_two_connections();
        let shop = Connection {
            name: "shop".into(),
            folder: "Staging".into(),
            adapter: Adapter::Mysql,
            host: "stage.internal".into(),
            port: 3306,
            user: "reader".into(),
            readonly: true,
            ..config.connections[0].clone()
        };
        config.upsert(shop, None).unwrap();
        config
    }

    fn picker<R>(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        read: impl FnOnce(&ConnectionsDialog) -> R,
    ) -> R {
        cx.update(|cx| read(workspace.read(cx).connections.read(cx)))
    }

    fn shows(window: &mut Window, id: &str) -> bool {
        window
            .try_find(SharedString::from(id.to_owned()))
            .is_some_and(|el| el.visible())
    }

    /// The text drawn inside the element `id`.
    fn text_in(window: &Window, id: &str) -> String {
        let id = ElementId::Name(id.to_owned().into());
        gpui_kit::base::test_support::texts(window)
            .iter()
            .filter(|text| text.path().contains(&id))
            .map(|text| text.text().to_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn search(handle: gpui_kit::AnyWindowHandle, text: &str, cx: &mut TestAppContext) {
        window_action(handle, cx, |window, cx| {
            window.click("connections-search", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.press("secondary-a", cx);
            window.press("backspace", cx);
            if !text.is_empty() {
                window.input(text, cx);
            }
        });
    }

    fn connect(workspace: &Entity<Workspace>, name: &str, cx: &mut TestAppContext) {
        workspace.update(cx, |this, cx| {
            this.connected.insert(name.into());
            this.show_connected_tabs(cx);
            cx.notify();
        });
    }

    #[test]
    fn the_dialog_is_as_designed_and_shrinks_with_the_window() {
        assert_eq!(
            dialog_size(size(px(1200.), px(760.))),
            size(px(900.), px(620.))
        );
        assert_eq!(
            dialog_size(size(px(2400.), px(1600.))),
            size(px(900.), px(620.))
        );
        assert_eq!(
            dialog_size(size(px(800.), px(500.))),
            size(px(600.), px(410.))
        );
    }

    #[gpui_kit::test]
    fn the_dialog_takes_its_size_from_the_window(cx: &mut TestAppContext) {
        let (handle, _) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            let expected = dialog_size(window.viewport_size());
            let bounds = window.find("connections-dialog").bounds();
            assert_eq!(bounds.size.height, expected.height);
            // Inside the dialog's border.
            assert!((expected.width - bounds.size.width).abs() <= px(2.));
        });
    }

    #[gpui_kit::test]
    fn the_header_counts_saved_and_connected(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_folders(), cx);
        connect(&workspace, "db", cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert_eq!(
                text_in(window, "connections-summary"),
                "3 saved · 1 connected"
            );
        });
    }

    #[gpui_kit::test]
    fn the_search_filters_and_opens_folders_and_clearing_it_folds_them_back(
        cx: &mut TestAppContext,
    ) {
        let (handle, workspace) = open(config_with_folders(), cx);
        open_picker(handle, cx);
        // Closed before searching.
        window_action(handle, cx, |window, cx| {
            window.click("folder-Production", cx)
        });
        window_action(handle, cx, |window, _| assert!(!shows(window, "conn-db")));

        search(handle, "db", cx);
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "conn-db"));
            assert!(!shows(window, "conn-other"));
            assert!(!shows(window, "folder-Staging"));
        });
        // The match is selected, ready for Enter.
        assert_eq!(
            picker(&workspace, cx, |p| p.cursor.clone()),
            Some(TreeRow::Connection("db".into()))
        );

        // By host and by folder.
        search(handle, "stage.internal", cx);
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "conn-shop"));
            assert!(!shows(window, "folder-Production"));
        });
        search(handle, "production", cx);
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "conn-db"));
            assert!(shows(window, "conn-other"));
            assert!(!shows(window, "conn-shop"));
        });

        search(handle, "nothing-like-it", cx);
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "connections-no-match"));
            assert!(!shows(window, "folder-Production"));
        });

        search(handle, "", cx);
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "folder-Production"));
            assert!(!shows(window, "conn-db"));
            assert!(shows(window, "conn-shop"));
        });
        assert!(picker(&workspace, cx, |p| p
            .collapsed
            .contains("Production")));
    }

    #[gpui_kit::test]
    fn each_chip_keeps_its_connections(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_folders(), cx);
        connect(&workspace, "db", cx);
        workspace.update(cx, |this, _| this.session.record("shop", "select 1"));
        open_picker(handle, cx);
        let visible = |cx: &mut TestAppContext| {
            window_action(handle, cx, |window, _| {
                ["db", "other", "shop"].map(|name| shows(window, &format!("conn-{name}")))
            })
        };
        assert_eq!(visible(cx), [true, true, true]);

        window_action(handle, cx, |window, cx| {
            window.click("filter-connected", cx)
        });
        assert_eq!(visible(cx), [true, false, false]);
        window_action(handle, cx, |window, _| {
            assert!(!shows(window, "folder-Staging"));
        });

        window_action(handle, cx, |window, cx| window.click("filter-recent", cx));
        assert_eq!(visible(cx), [false, false, true]);

        // A chip and a search narrow together.
        search(handle, "db", cx);
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "connections-no-match"));
        });
        search(handle, "", cx);

        window_action(handle, cx, |window, cx| window.click("filter-all", cx));
        assert_eq!(visible(cx), [true, true, true]);

        // Opening the dialog again starts from All.
        window_action(handle, cx, |window, cx| {
            window.click("filter-connected", cx)
        });
        open_picker(handle, cx);
        open_picker(handle, cx);
        assert_eq!(picker(&workspace, cx, |p| p.filter), Filter::All);
        assert_eq!(visible(cx), [true, true, true]);
    }

    #[gpui_kit::test]
    fn folders_count_their_connected_connections(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_folders(), cx);
        connect(&workspace, "db", cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert_eq!(text_in(window, "folder-count-Production"), "1 / 2");
            // None connected: just how many it holds.
            assert_eq!(text_in(window, "folder-count-Staging"), "1");
        });
    }

    #[gpui_kit::test]
    fn up_and_down_in_the_search_move_the_selection_and_enter_opens_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_folders(), cx);
        let cursor = |cx: &mut TestAppContext| picker(&workspace, cx, |p| p.cursor.clone());
        open_picker(handle, cx);
        search(handle, "production", cx);
        assert_eq!(cursor(cx), Some(TreeRow::Connection("db".into())));
        window_action(handle, cx, |window, cx| window.press("down", cx));
        assert_eq!(cursor(cx), Some(TreeRow::Connection("other".into())));
        window_action(handle, cx, |window, cx| window.press("up", cx));
        window_action(handle, cx, |window, cx| window.press("up", cx));
        assert_eq!(cursor(cx), Some(TreeRow::Folder("Production".into())));
        // The keys stay in the search field, to go on typing.
        window_action(handle, cx, |window, _| {
            assert_eq!(window.find("connections-search").focused(), Some(true));
        });

        window_action(handle, cx, |window, cx| window.press("down", cx));
        window_action(handle, cx, |window, cx| window.press("enter", cx));
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx));
        });
        assert_eq!(
            workspace.read_with(cx, |this, _| this.active.clone()),
            Some("db".into())
        );
    }

    #[gpui_kit::test]
    fn enter_with_a_folder_or_nothing_selected_keeps_the_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.press("enter", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.has_active_dialog(cx))
        });
        window_action(handle, cx, |window, cx| window.press("down", cx));
        window_action(handle, cx, |window, cx| window.press("enter", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.has_active_dialog(cx))
        });
        assert!(picker(&workspace, cx, |p| p
            .collapsed
            .contains("Production")));
        assert!(workspace.read_with(cx, |this, _| this.tabs.is_empty()));
    }

    #[gpui_kit::test]
    fn a_click_selects_and_the_details_show_it_without_secrets(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_folders(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert!(!shows(window, "details-open"));
        });
        window_action(handle, cx, |window, cx| window.click("conn-shop", cx));
        window_action(handle, cx, |window, cx| {
            // A click opens nothing.
            assert!(window.has_active_dialog(cx));
            assert_eq!(text_in(window, "details-name"), "shop");
            assert_eq!(text_in(window, "details-host"), "stage.internal:3306");
            assert_eq!(text_in(window, "details-status"), "Not connected");
            let details = text_in(window, "connection-details");
            for shown in ["MySQL", "reader", "Read-only", "Staging"] {
                assert!(details.contains(shown), "{shown} in {details}");
            }
            assert!(!details.to_lowercase().contains("password"));
        });
        assert!(workspace.read_with(cx, |this, _| this.tabs.is_empty()));
    }

    #[gpui_kit::test]
    fn the_details_connect_and_then_go_to_the_tab(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |window, cx| {
            assert_eq!(window.find("details-open").label(), Some("Connect ↵"));
            window.click("details-open", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx));
            assert!(window.find("tab-db").visible());
        });
        open_tab(handle, &workspace, "other", cx);

        // Opening the dialog selects the tab on screen.
        open_picker(handle, cx);
        assert_eq!(
            picker(&workspace, cx, |p| p.cursor.clone()),
            Some(TreeRow::Connection("other".into()))
        );
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |window, cx| {
            assert_eq!(window.find("details-open").label(), Some("Go to tab ↵"));
            window.click("details-open", cx);
        });
        workspace.read_with(cx, |this, _| {
            assert_eq!(this.active.as_deref(), Some("db"));
            assert_eq!(this.tabs.len(), 2);
        });
    }

    #[gpui_kit::test]
    fn edit_in_the_details_opens_the_form(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.click("details-edit", cx));
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx));
            let form = workspace.read(cx).form.clone().expect("the form opens");
            assert_eq!(form.read(cx).editing(), Some("db"));
        });
    }

    #[gpui_kit::test]
    fn disconnect_in_the_details_closes_the_tab_and_keeps_the_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_tab(handle, &workspace, "db", cx);
        connect(&workspace, "db", cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert_eq!(
                text_in(window, "details-status"),
                "Connected, open in a tab"
            );
        });
        window_action(handle, cx, |window, cx| {
            window.click("details-disconnect", cx)
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.has_active_dialog(cx));
            assert!(!shows(window, "details-disconnect"));
            assert!(!shows(window, "tab-db"));
            assert_eq!(text_in(window, "details-status"), "Not connected");
        });
        workspace.read_with(cx, |this, _| {
            assert!(this.tabs.is_empty());
            assert!(this.connected.is_empty());
        });
        // The dialog still has the keys: Escape closes it.
        window_action(handle, cx, |window, cx| window.press("escape", cx));
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx))
        });
    }

    #[gpui_kit::test]
    fn delete_in_the_details_asks_over_the_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.click("details-delete", cx));
        window_action(handle, cx, |window, cx| {
            assert!(shows(window, "ok"));
            assert!(shows(window, "connections-dialog"));
            window.click("ok", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(!shows(window, "ok"));
            assert!(!shows(window, "conn-db"));
            assert!(shows(window, "connections-dialog"));
        });
        assert!(workspace.read_with(cx, |this, _| this.config.connections.is_empty()));
    }

    #[gpui_kit::test]
    fn a_connecting_connection_says_so(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        // Nothing listens at the test connection's port: it stays connecting.
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| this.open_connection("db".into(), window, cx));
        });
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "conn-status-db"));
            assert_eq!(text_in(window, "details-status"), "Connecting…");
            // Its tab can be closed while it connects.
            assert!(shows(window, "details-disconnect"));
        });
    }

    #[gpui_kit::test]
    fn a_new_folder_ends_a_search_so_that_it_shows(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_folders(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.click("filter-connected", cx)
        });
        search(handle, "shop", cx);
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            window.input("Archive", cx);
            window.press("enter", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(shows(window, "folder-Archive"));
            assert!(shows(window, "conn-db"));
        });
        assert_eq!(picker(&workspace, cx, |p| p.filter), Filter::All);
    }

    #[gpui_kit::test]
    fn a_new_connection_goes_into_the_selected_folder(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_folders(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("conn-shop", cx));
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("the form opens");
            assert_eq!(form.read(cx).folder, "Staging");
        });
    }
}
