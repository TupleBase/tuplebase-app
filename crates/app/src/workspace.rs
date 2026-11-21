//! Main window: a top bar of open connections, one tab each, above the open one.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use gpui_kit::component::button::*;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::AppMenuBar;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::resizable::{
    ResizablePanel, ResizableState, h_resizable, resizable_panel,
};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::agents::{self, AgentState, AgentStatus};
use tuplebase_core::config::{
    Appearance, Config, Connection, DEFAULT_FONT_SIZE, kept_unreadable_message,
};
use tuplebase_core::db::Db;
use tuplebase_core::secrets::{ConnectionSecrets, Secret};
use tuplebase_core::state::SessionState;
use tuplebase_core::{app_status, mcp_status, secrets, updates};

use crate::chat::{ChatEvent, ChatPanel};
use crate::completions;
use crate::conn_form::{ConnForm, ConnFormEvent};
use crate::connection_tab::{self, ConnectionTab, HISTORY_MENU_SIZE, Status, TabEvent};
use crate::dialogs;
use crate::logos;
use crate::runtime::{blocking, io};
use crate::saving::{Save, Saves};
use crate::theme;
use crate::tree_nav;

mod connections;
mod settings;
mod top_bar;

use connections::ConnectionsDialog;
use settings::SettingsPage;

actions!(
    tuplebase,
    [
        RunQuery,
        RunAll,
        OpenSettings,
        OpenConnections,
        CheckForUpdates,
        About,
        AddMcpToAgents,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize
    ]
);

#[cfg(target_os = "macos")]
pub(crate) const RUN_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
pub(crate) const RUN_KEY: &str = "ctrl-enter";
#[cfg(target_os = "macos")]
const SETTINGS_KEY: &str = "cmd-,";
#[cfg(not(target_os = "macos"))]
const SETTINGS_KEY: &str = "ctrl-,";
#[cfg(target_os = "macos")]
const RUN_ALL_KEY: &str = "cmd-shift-enter";
#[cfg(not(target_os = "macos"))]
const RUN_ALL_KEY: &str = "ctrl-shift-enter";

/// A key held with Cmd on macOS and Ctrl elsewhere.
#[cfg(target_os = "macos")]
macro_rules! secondary {
    ($key:literal) => {
        concat!("cmd-", $key)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! secondary {
    ($key:literal) => {
        concat!("ctrl-", $key)
    };
}

/// Opens and closes the Connections dialog.
pub(crate) const CONNECTIONS_KEY: &str = secondary!("k");

/// Call after `gpui_kit::init`, so the `Input` binding overrides the editor's own
/// one, which would insert a newline.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(RUN_KEY, RunQuery, Some("Input")),
        KeyBinding::new(RUN_KEY, RunQuery, None),
        KeyBinding::new(RUN_ALL_KEY, RunAll, Some("Input")),
        KeyBinding::new(RUN_ALL_KEY, RunAll, None),
        KeyBinding::new(SETTINGS_KEY, OpenSettings, None),
        KeyBinding::new(CONNECTIONS_KEY, OpenConnections, None),
        KeyBinding::new(secondary!("="), IncreaseFontSize, None),
        // The same key with Shift held.
        KeyBinding::new(secondary!("+"), IncreaseFontSize, None),
        KeyBinding::new(secondary!("-"), DecreaseFontSize, None),
        KeyBinding::new(secondary!("0"), ResetFontSize, None),
    ]);
    cx.bind_keys(tree_nav::key_bindings());
    cx.bind_keys(connections::key_bindings());
    cx.bind_keys(completions::key_bindings());
}

/// Menu items must work whatever has focus, so their actions are global and go to the
/// app's one workspace.
pub fn register_menu_actions(workspace: &Entity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    fn on<A: Action>(
        workspace: &Entity<Workspace>,
        window: AnyWindowHandle,
        cx: &mut App,
        run: fn(&mut Workspace, &mut Window, &mut Context<Workspace>),
    ) {
        let workspace = workspace.downgrade();
        cx.on_action(move |_: &A, cx| {
            let workspace = workspace.clone();
            // Dispatching holds the window, so it can only be updated afterwards.
            cx.defer(move |cx| {
                window
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |this, cx| run(this, window, cx)).ok();
                    })
                    .ok();
            });
        });
    }
    on::<OpenSettings>(workspace, window, cx, |this, window, cx| {
        this.open_settings(SettingsPage::General, window, cx);
    });
    on::<OpenConnections>(workspace, window, cx, |this, window, cx| {
        this.toggle_connections(window, cx);
    });
    on::<About>(workspace, window, cx, |this, window, cx| {
        this.open_about(window, cx);
    });
    on::<CheckForUpdates>(workspace, window, cx, |this, window, cx| {
        this.check_for_updates_now(window, cx);
    });
    on::<AddMcpToAgents>(workspace, window, cx, |this, window, cx| {
        this.add_mcp_to_agents(window, cx);
    });
    on::<IncreaseFontSize>(workspace, window, cx, |this, _, cx| {
        this.change_font_size(|config| config.resize_font(1), cx);
    });
    on::<DecreaseFontSize>(workspace, window, cx, |this, _, cx| {
        this.change_font_size(|config| config.resize_font(-1), cx);
    });
    on::<ResetFontSize>(workspace, window, cx, |this, _, cx| {
        this.change_font_size(|config| config.font_size = DEFAULT_FONT_SIZE, cx);
    });
}

const STATUS_POLL: Duration = Duration::from_secs(5);

/// The least room the chat leaves beside it. It fits the schema panel at its widest
/// next to an editor that is still usable.
const MAIN_MIN_WIDTH: f32 = 480.;

/// How far along a newer version is.
enum UpdateState {
    Available {
        version: String,
        /// Why the last attempt failed.
        error: Option<SharedString>,
    },
    Downloading {
        version: String,
        percent: Arc<AtomicU8>,
    },
    Ready {
        download: updates::Download,
        installation: updates::Installation,
    },
}

/// A panel whose edge can be dragged. Its size is kept between launches.
#[derive(Clone, Copy)]
pub(crate) enum Panel {
    Schema,
    Editor,
    Chat,
    RowDetail,
}

impl Panel {
    fn default_size(self) -> f32 {
        match self {
            Panel::Schema => 270.,
            Panel::Editor => 220.,
            Panel::Chat => 380.,
            Panel::RowDetail => 320.,
        }
    }

    fn size_range(self) -> Range<Pixels> {
        match self {
            Panel::Schema => px(180.)..px(380.),
            Panel::Editor => px(80.)..px(900.),
            Panel::Chat => px(280.)..px(720.),
            Panel::RowDetail => px(220.)..px(640.),
        }
    }

    /// The name its size is saved under.
    fn key(self) -> &'static str {
        match self {
            Panel::Schema => "schema",
            Panel::Editor => "editor",
            Panel::Chat => "chat",
            Panel::RowDetail => "row_detail",
        }
    }

    /// Where it sits in its group: the chat is second, after the main area, and the row
    /// detail second, after the results; the others come first.
    fn index(self) -> usize {
        match self {
            Panel::Chat | Panel::RowDetail => 1,
            Panel::Schema | Panel::Editor => 0,
        }
    }

    /// A panel that opens at the size it was last dragged to, as kept in `saved`, and
    /// keeps that size when the window resizes.
    pub(crate) fn sized(self, saved: &BTreeMap<String, u32>) -> ResizablePanel {
        let saved = saved.get(self.key());
        resizable_panel()
            .size(px(saved.map_or(self.default_size(), |size| *size as f32)))
            .size_range(self.size_range())
            .fixed()
    }

    /// The sizes these panels of one group were dragged to, by the name each is saved
    /// under. One that is not shown keeps the size it had, so it is left out.
    pub(crate) fn dragged_sizes(
        panels: &[Panel],
        state: &Entity<ResizableState>,
        cx: &App,
    ) -> Vec<(&'static str, u32)> {
        let sizes = state.read(cx).sizes();
        panels
            .iter()
            .filter_map(|panel| {
                let size = sizes.get(panel.index())?;
                Some((panel.key(), size.as_f32().round() as u32))
            })
            .collect()
    }
}

/// A connection the form is saving.
struct Saving {
    form: Entity<ConnForm>,
    connection: Connection,
    /// Its name before an edit.
    previous: Option<String>,
}

/// An open connection's tab.
struct OpenTab {
    /// The tab's connection, kept in step with its own on a rename.
    name: String,
    tab: Entity<ConnectionTab>,
    _subscription: Subscription,
}

pub struct Workspace {
    db: Arc<Db>,
    config: Config,
    /// One per open connection, in the order they were opened.
    tabs: Vec<OpenTab>,
    /// The name of the tab on screen.
    active: Option<String>,
    /// Scrolls the tab on screen into view when there are more tabs than fit.
    tab_scroll: ScrollHandle,
    form: Option<Entity<ConnForm>>,
    /// The status line of the empty screen, shown when no tab is open.
    status: Status,
    session: SessionState,
    _session_save: Option<Task<()>>,
    /// A change to the session waits for `_session_save`.
    session_unsaved: bool,
    session_saves: Saves<SessionState>,
    config_saves: Saves<Config>,
    chat: Entity<ChatPanel>,
    chat_open: bool,
    _chat_subscription: Subscription,
    app_menu: Entity<AppMenuBar>,
    /// MCP servers that agents have running.
    /// Connections with an open pool.
    connected: HashSet<String>,
    /// One entry per running MCP server: its agent's name, when known.
    mcp_servers: Vec<Option<String>>,
    /// `None` until Settings has looked for them.
    agents: Option<Vec<AgentStatus>>,
    /// The Connections dialog's view, kept while it is closed so that the folders it
    /// shows open stay open.
    connections: Entity<ConnectionsDialog>,
    /// Whether the Connections dialog is open.
    connections_open: bool,
    /// A newer version found by the update check, and how far along it is.
    update_state: Option<UpdateState>,
    _form_subscription: Option<Subscription>,
    /// Where saving moved a connections file this version can't read, until the status
    /// line has shown it past the connect that follows saving a connection.
    kept_note: Option<SharedString>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Small local file read once before the first frame.
        // A file that can't be used is left as it is, never saved over (see `core::config`).
        let (config, status) = match Config::load_or_keep() {
            Ok((config, None)) => (config, Status::Idle),
            Ok((config, Some(message))) => (config, Status::Failed(message.into())),
            Err(e) => {
                let config = Config {
                    unsaved_because: Some(format!("{e:#}")),
                    ..Config::default()
                };
                (config, Status::Failed(format!("{e:#}").into()))
            }
        };
        theme::set_appearance(config.appearance, Some(window), cx);
        cx.observe_window_appearance(window, |this, window, cx| {
            if this.config.appearance.is_system() {
                theme::set_appearance(Appearance::System, Some(window), cx);
            }
        })
        .detach();
        let mut this = Self::with_config(config, status, window, cx);
        this.session = SessionState::load();
        this.restore_tabs(window, cx);
        this.watch_mcp_servers(cx);
        this.watch_connections(cx);
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
                this.update_state = latest.map(|version| UpdateState::Available {
                    version,
                    error: None,
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Notices what changes on its own, such as an SSH tunnel dropping. Only asks the local
    /// pools, so it adds no traffic to the databases.
    fn watch_connections(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while this
                .update(cx, |this, cx| this.refresh_connected(cx))
                .is_ok()
            {
                cx.background_executor().timer(STATUS_POLL).await;
            }
        })
        .detach();
    }

    fn refresh_connected(&self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let open = io(async move {
                // One at a time, so an older list is never written over a newer one.
                static PUBLISHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
                let _publishing = PUBLISHING.lock().await;
                let open = db.open_connections().await;
                // Rewritten on every poll, changed or not: agents take a file that has
                // stopped changing to mean the app has quit.
                let publish = move || {
                    let _ = app_status::publish(&open);
                    open
                };
                Ok(tokio::task::spawn_blocking(publish).await?)
            })
            .await
            .unwrap_or_default();
            this.update(cx, |this, cx| {
                if this.connected != open {
                    this.connected = open;
                    this.show_connected_tabs(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn show_connected_tabs(&self, cx: &mut Context<Self>) {
        for open in &self.tabs {
            let connected = self.connected.contains(&open.name);
            open.tab
                .update(cx, |tab, cx| tab.set_connected(connected, cx));
        }
    }

    /// Closes a connection, and its tab with it: keeping the tab would let its next query
    /// quietly connect again.
    fn close_tab(&mut self, name: String, cx: &mut Context<Self>) {
        self.remove_tab(&name, cx);
        self.connected.remove(&name);
        cx.notify();
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            io(async move {
                db.disconnect(&name).await;
                Ok(())
            })
            .await
            .ok();
            this.update(cx, |this, cx| this.refresh_connected(cx)).ok();
        })
        .detach();
    }

    /// Takes `name`'s tab off the screen; the one to its right shows instead, else the
    /// one to its left.
    fn remove_tab(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(name) else {
            return;
        };
        self.tabs.remove(ix);
        if self.active.as_deref() == Some(name) {
            self.active = None;
            let next = if ix < self.tabs.len() {
                Some(ix)
            } else {
                ix.checked_sub(1)
            };
            if let Some(next) = next {
                self.put_on_screen(next, cx);
            }
        }
        self.remember_tabs(cx);
        cx.notify();
    }

    fn watch_mcp_servers(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let running = blocking(mcp_status::running_servers)
                    .await
                    .unwrap_or_default();
                let updated = this.update(cx, |this, cx| {
                    if this.mcp_servers != running {
                        this.mcp_servers = running;
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    return;
                }
                cx.background_executor().timer(STATUS_POLL).await;
            }
        })
        .detach();
    }

    fn refresh_agents(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let statuses = blocking(|| Ok(agents::statuses()))
                .await
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.agents = Some(statuses);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_mcp_to_agents(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let statuses = blocking(|| Ok(agents::add_to_all()))
                .await
                .unwrap_or_default();
            this.update_in(cx, |this, window, cx| {
                this.finish_adding_mcp(statuses, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Settings' button for one agent. The page shows the result on that agent's row.
    /// Only that row changes, so adds to two agents at once can't undo each other.
    fn add_mcp_to_agent(&self, name: &'static str, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let added = blocking(move || Ok(agents::add_to(name)))
                .await
                .unwrap_or_default();
            this.update(cx, |this, cx| this.update_agents(added, cx))
                .ok();
        })
        .detach();
    }

    /// Puts each status on its agent's row, leaving the other rows as they are.
    fn update_agents(&mut self, statuses: Vec<AgentStatus>, cx: &mut Context<Self>) {
        for status in statuses {
            let mut shown = self.agents.iter_mut().flatten();
            if let Some(shown) = shown.find(|shown| shown.name == status.name) {
                shown.state = status.state;
            }
        }
        cx.notify();
    }

    fn finish_adding_mcp(
        &mut self,
        statuses: Vec<AgentStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Settings lists each agent's result itself; the menu has nowhere else to say it.
        if !window.has_active_dialog(cx) {
            window.push_notification(mcp_added_note(&statuses), cx);
        }
        self.agents = Some(statuses);
        cx.notify();
    }

    /// The menu's check: unlike the one at startup, it always answers.
    fn check_for_updates_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(note) = self.update_in_progress() {
            window.push_notification(Notification::info(note), cx);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let result = blocking(|| updates::check(env!("CARGO_PKG_VERSION"))).await;
            this.update_in(cx, |this, window, cx| {
                this.finish_update_check(result, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// What the top bar is already showing about an update, if anything.
    fn update_in_progress(&self) -> Option<String> {
        Some(match self.update_state.as_ref()? {
            UpdateState::Available { version, .. } => {
                format!("TupleBase {version} is available. Update from the top bar.")
            }
            UpdateState::Downloading { version, .. } => {
                format!("Downloading TupleBase {version}…")
            }
            UpdateState::Ready { download, .. } => {
                format!(
                    "TupleBase {} is ready. Restart from the top bar.",
                    download.version
                )
            }
        })
    }

    fn finish_update_check(
        &mut self,
        result: anyhow::Result<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let note = match result {
            Ok(Some(version)) => {
                let note = format!("TupleBase {version} is available. Update from the top bar.");
                if self.update_state.is_none() {
                    self.update_state = Some(UpdateState::Available {
                        version,
                        error: None,
                    });
                }
                Notification::info(note)
            }
            Ok(None) => Notification::success(concat!(
                "TupleBase ",
                env!("CARGO_PKG_VERSION"),
                " is up to date."
            )),
            Err(e) => Notification::error(format!("Could not check for updates: {e:#}")),
        };
        window.push_notification(note, cx);
        cx.notify();
    }

    fn set_check_for_updates(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.config.check_for_updates = enabled;
        self.update_state = None;
        if enabled {
            self.check_for_updates(cx);
        }
        cx.notify();
        self.save_config(cx);
    }

    fn change_font_size(&mut self, change: impl FnOnce(&mut Config), cx: &mut Context<Self>) {
        let before = self.config.font_size;
        change(&mut self.config);
        if self.config.font_size != before {
            theme::set_font_size(self.config.font_size, cx);
            self.save_config(cx);
        }
    }

    /// Settings' choice of light or dark colors.
    fn set_appearance(
        &mut self,
        appearance: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.config.appearance != appearance {
            self.config.appearance = appearance;
            theme::set_appearance(appearance, Some(window), cx);
            self.save_config(cx);
            cx.notify();
        }
    }

    fn save_config(&mut self, cx: &mut Context<Self>) {
        let save = self.config_saves.save(self.config.clone());
        cx.spawn(async move |this, cx| {
            let saved = blocking(move || save.run(Config::save)).await;
            this.update(cx, |this, cx| this.show_saved(saved, cx)).ok();
        })
        .detach();
    }

    /// Shows in the status line why saving failed, or where it moved a connections file
    /// this version can't read; says nothing after an ordinary save.
    fn show_saved(&mut self, saved: anyhow::Result<Option<PathBuf>>, cx: &mut Context<Self>) {
        let status = match saved {
            Ok(None) => return,
            Ok(Some(kept)) => {
                let note = SharedString::from(kept_unreadable_message(&kept));
                self.kept_note = Some(note.clone());
                Status::Done(note)
            }
            Err(e) => Status::Failed(format!("{e:#}").into()),
        };
        self.report(status, cx);
    }

    /// Shows a message that belongs to no connection on the status line in view: the
    /// open tab's, or the empty screen's.
    fn report(&mut self, status: Status, cx: &mut Context<Self>) {
        match self.active_tab() {
            Some(tab) => tab.update(cx, |tab, cx| tab.set_status(status, cx)),
            None => self.status = status,
        }
        cx.notify();
    }

    /// The status once connecting to `name` is done; a note about a kept connections
    /// file stays in it rather than being replaced.
    fn connected_status(&mut self, name: &str, result: &Result<(), SharedString>) -> Status {
        let note = self.kept_note.take();
        let with_note = |text: String| match &note {
            Some(note) => format!("{text}. {note}"),
            None => text,
        };
        match result {
            Ok(()) => Status::Done(with_note(format!("Connected to {name}")).into()),
            Err(e) => Status::Failed(with_note(e.to_string()).into()),
        }
    }

    /// Creates a folder named `name` in `parent`, or gives the folder `renaming` that
    /// name. A rename gives back the folder's old and new path.
    fn name_folder(
        &mut self,
        parent: &str,
        renaming: Option<&str>,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Option<(String, String)> {
        let result = match renaming {
            Some(old) => self
                .config
                .rename_folder(old, name)
                .map(|new| Some((old.to_owned(), new))),
            None => self.config.add_folder(parent, name).map(|_| None),
        };
        match result {
            Ok(renamed) => {
                self.save_config(cx);
                self.sync_tabs(cx);
                cx.notify();
                renamed
            }
            Err(e) => {
                self.report(Status::Failed(format!("{e:#}").into()), cx);
                None
            }
        }
    }

    fn remove_folder(&mut self, folder: &str, cx: &mut Context<Self>) {
        self.config.remove_folder(folder);
        self.save_config(cx);
        self.sync_tabs(cx);
        cx.notify();
    }

    /// Moves the folder at `path` into `new_parent`; gives back its new path.
    fn move_folder(
        &mut self,
        path: &str,
        new_parent: &str,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let moved = match self.config.move_folder(path, new_parent) {
            Ok(None) => return None,
            Ok(Some(new_path)) => {
                self.save_config(cx);
                self.sync_tabs(cx);
                Some(new_path)
            }
            Err(e) => {
                self.report(Status::Failed(format!("{e:#}").into()), cx);
                None
            }
        };
        cx.notify();
        moved
    }

    /// Whether the connection moved.
    fn move_connection(&mut self, name: &str, folder: &str, cx: &mut Context<Self>) -> bool {
        let moved = self.config.move_connection(name, folder);
        if moved {
            self.save_config(cx);
            self.sync_tabs(cx);
            cx.notify();
        }
        moved
    }

    fn with_config(
        config: Config,
        status: Status,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        theme::set_font_size(config.font_size, cx);
        let db = Arc::new(Db::new());
        let workspace = cx.weak_entity();
        let chat = cx.new(|cx| ChatPanel::new(window, cx));
        let _chat_subscription =
            cx.subscribe(&chat, |this, _, event: &ChatEvent, cx| match event {
                ChatEvent::Close => {
                    this.chat_open = false;
                    cx.notify();
                }
            });
        // Quitting, or closing the window, must not lose a save still waiting.
        cx.on_app_quit(|this, _| this.flush_saves()).detach();
        cx.on_release(|this, cx| {
            let mut saved = Some(this.flush_saves());
            cx.on_app_quit(move |_| {
                let saved = saved.take();
                async move {
                    if let Some(saved) = saved {
                        saved.await;
                    }
                }
            })
            .detach();
        })
        .detach();
        Self {
            db,
            chat,
            chat_open: false,
            _chat_subscription,
            app_menu: AppMenuBar::new(cx),
            connected: HashSet::new(),
            mcp_servers: Vec::new(),
            agents: None,
            config,
            tabs: Vec::new(),
            active: None,
            tab_scroll: ScrollHandle::new(),
            form: None,
            session: SessionState::default(),
            _session_save: None,
            session_unsaved: false,
            session_saves: Saves::default(),
            config_saves: Saves::default(),
            status,
            connections: cx.new(|cx| ConnectionsDialog::new(workspace, window, cx)),
            connections_open: false,
            update_state: None,
            _form_subscription: None,
            kept_note: None,
        }
    }

    fn tab_index(&self, name: &str) -> Option<usize> {
        self.tabs.iter().position(|open| open.name == name)
    }

    fn tab(&self, name: &str) -> Option<&Entity<ConnectionTab>> {
        Some(&self.tabs[self.tab_index(name)?].tab)
    }

    /// The tab on screen.
    fn active_tab(&self) -> Option<&Entity<ConnectionTab>> {
        self.tab(self.active.as_deref()?)
    }

    /// Shows `name`'s tab.
    fn activate_tab(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab_index(name).is_some() {
            self.form = None;
            self.show_tab(name, cx);
            self.focus_active_editor(window, cx);
        }
    }

    /// Puts `name`'s tab on screen and remembers it.
    fn show_tab(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_index(name) {
            self.put_on_screen(ix, cx);
            self.remember_tabs(cx);
            cx.notify();
        }
    }

    /// Makes tab `ix` the one on screen, scrolled into view. A tab brought back at
    /// launch connects now, the first time it shows.
    fn put_on_screen(&mut self, ix: usize, cx: &mut Context<Self>) {
        let open = &self.tabs[ix];
        open.tab.update(cx, |tab, cx| tab.start(cx));
        self.active = Some(open.name.clone());
        self.tab_scroll.scroll_to_item(ix);
    }

    /// Has the open tabs and the one on screen saved, to bring them back at launch.
    fn remember_tabs(&mut self, cx: &mut Context<Self>) {
        let open: Vec<String> = self.tabs.iter().map(|open| open.name.clone()).collect();
        if self.session.open_tabs != open || self.session.active_tab != self.active {
            self.session.set_tabs(open, self.active.clone());
            self.schedule_session_save(cx);
        }
    }

    /// Brings back the tabs open when the app last closed, in their order, skipping
    /// connections deleted since. Only the one on screen connects: each of the others
    /// could ask for a password or a sign-in, so they wait until first shown.
    fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // What is remembered stays as it is until a tab changes: a config that failed
        // to load must not make the app forget its tabs.
        let names = self.session.open_tabs.clone();
        for name in names {
            self.add_tab(name, window, cx);
        }
        let active = self
            .session
            .active_tab
            .as_deref()
            .and_then(|name| self.tab_index(name))
            .or((!self.tabs.is_empty()).then_some(0));
        if let Some(ix) = active {
            self.put_on_screen(ix, cx);
            self.focus_active_editor(window, cx);
        }
    }

    /// Gives the keys to the SQL editor of the tab on screen, ready to type in.
    fn focus_active_editor(&self, window: &mut Window, cx: &mut App) {
        if let Some(tab) = self.active_tab() {
            let editor = tab.read(cx).editor.clone();
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
    }

    /// A click on a connection, or Enter on it in the tree: shows its tab, opening one
    /// first if needed. An open tab stays as it is, with its running query and its
    /// rows, unless connecting failed and this tries again.
    fn open_connection(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.form = None;
        if let Some(tab) = self.tab(&name).cloned() {
            let started = tab.read(cx).started;
            self.show_tab(&name, cx);
            tab.update(cx, |tab, cx| {
                if started && matches!(tab.status, Status::Failed(_)) {
                    tab.connect(cx);
                }
            });
            return;
        }
        if self.add_tab(name.clone(), window, cx) {
            self.show_tab(&name, cx);
        }
    }

    /// Adds a tab for `name` at the end, not yet connecting. False if `name` is gone
    /// from the config or already has a tab.
    fn add_tab(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.tab_index(&name).is_some() {
            return false;
        }
        let Some(conn) = self.config.find(&name).cloned() else {
            return false;
        };
        self.db.allow_connecting(&name);
        let draft = self.session.drafts.get(&name).cloned().unwrap_or_default();
        let (db, history, panels) = (
            self.db.clone(),
            self.history(&name),
            self.session.panels.clone(),
        );
        let tab = cx.new(|cx| ConnectionTab::new(db, conn, draft, history, panels, window, cx));
        let _subscription = cx.subscribe_in(&tab, window, Self::on_tab_event);
        let connected = self.connected.contains(&name);
        tab.update(cx, |tab, cx| tab.set_connected(connected, cx));
        self.tabs.push(OpenTab {
            name,
            tab,
            _subscription,
        });
        true
    }

    fn on_tab_event(
        &mut self,
        tab: &Entity<ConnectionTab>,
        event: &TabEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = tab.read(cx).name().to_owned();
        match event {
            TabEvent::DraftChanged(sql) => {
                self.session.set_draft(&name, sql);
                self.schedule_session_save(cx);
            }
            TabEvent::Ran(sql) => {
                self.session.record(&name, sql);
                self.schedule_session_save(cx);
                let history = self.history(&name);
                tab.update(cx, |tab, cx| tab.set_history(history, cx));
            }
            TabEvent::Resized(sizes) => {
                for (key, size) in sizes {
                    self.session.panels.insert((*key).into(), *size);
                }
                self.schedule_session_save(cx);
                let panels = self.session.panels.clone();
                for open in &self.tabs {
                    let panels = panels.clone();
                    open.tab
                        .update(cx, |tab, cx| tab.set_panel_sizes(panels, cx));
                }
            }
            TabEvent::Edit => {
                let conn = tab.read(cx).connection().clone();
                self.edit_connection(conn, window, cx);
            }
            TabEvent::Delete => self.confirm_delete(name, window, cx),
            TabEvent::Connected(result) => {
                self.refresh_connected(cx);
                let status = self.connected_status(&name, result);
                tab.update(cx, |tab, cx| tab.set_status(status, cx));
            }
            TabEvent::ConnectionChanged => self.refresh_connected(cx),
        }
    }

    /// The newest queries run on `name`, for its history menu.
    fn history(&self, name: &str) -> Vec<String> {
        self.session
            .history_for(name)
            .take(HISTORY_MENU_SIZE)
            .map(str::to_owned)
            .collect()
    }

    /// Gives each tab its connection as the config now has it, after a change such as a
    /// new folder, so a tab never runs on settings that are out of date.
    fn sync_tabs(&self, cx: &mut Context<Self>) {
        for open in &self.tabs {
            let Some(conn) = self.config.find(&open.name).cloned() else {
                continue;
            };
            open.tab.update(cx, |tab, cx| {
                if *tab.connection() != conn {
                    tab.set_connection(conn, cx);
                }
            });
        }
    }

    fn edit_connection(&mut self, conn: Connection, window: &mut Window, cx: &mut Context<Self>) {
        let folder = conn.folder.clone();
        self.open_form(Some(conn), folder, window, cx);
    }

    /// Opens the connection form; a new connection is saved into `folder`.
    fn open_form(
        &mut self,
        existing: Option<Connection>,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last_aws = self.session.aws_search.clone();
        let form = cx.new(|cx| ConnForm::new(existing.as_ref(), folder, last_aws, window, cx));
        self._form_subscription =
            Some(
                cx.subscribe_in(&form, window, |this, form, event, window, cx| match event {
                    ConnFormEvent::Save {
                        connection,
                        password,
                        ssh_password,
                        previous,
                    } => {
                        let saving = Saving {
                            form: form.clone(),
                            connection: connection.clone(),
                            previous: previous.clone(),
                        };
                        let passwords = (password.clone(), ssh_password.clone());
                        this.save_connection(saving, passwords, window, cx);
                    }
                    ConnFormEvent::Cancel => {
                        this.form = None;
                        cx.notify();
                    }
                    ConnFormEvent::SearchedAws(search) => {
                        this.session.aws_search = search.clone();
                        this.schedule_session_save(cx);
                    }
                }),
            );
        self.form = Some(form);
        cx.notify();
    }

    /// Saves the form's connection, with its password and SSH password when changed.
    fn save_connection(
        &mut self,
        saving: Saving,
        (password, ssh_password): (Option<String>, Option<String>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Saving {
            form,
            connection,
            previous,
        } = saving;
        let mut config = self.config.clone();
        if let Err(e) = config.upsert(connection.clone(), previous.as_deref()) {
            form.update(cx, |form, cx| form.set_error(format!("{e:#}"), cx));
            return;
        }
        let db = self.db.clone();
        let save = self.config_saves.save(config.clone());
        let (conn, prev) = (connection.clone(), previous.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = blocking(move || {
                let password = match &password {
                    // The credentials now come from AWS; don't keep an unused password.
                    _ if !connection.auth.is_password() => Secret::Delete,
                    Some(pw) => Secret::Set(pw),
                    None => Secret::Keep,
                };
                let ssh_password = ssh_password.as_deref().map_or(Secret::Keep, Secret::Set);
                let changes = ConnectionSecrets {
                    previous: previous.as_deref(),
                    name: &connection.name,
                    password,
                    ssh_password,
                };
                // The keychain first, undone if the config can't be written.
                secrets::save_connection(&changes, || save.run(Config::save))
            })
            .await;
            if let (Ok(_), Some(previous)) = (&result, prev.clone()) {
                io(async move {
                    db.forget(&previous).await;
                    Ok(())
                })
                .await
                .ok();
            }
            this.update_in(cx, |this, window, cx| {
                let saving = Saving {
                    form,
                    connection: conn,
                    previous: prev,
                };
                this.finish_saving_connection(saving, config, result, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Once the connection form's save is written.
    fn finish_saving_connection(
        &mut self,
        saving: Saving,
        config: Config,
        result: anyhow::Result<Option<PathBuf>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Saving {
            form,
            connection,
            previous,
        } = saving;
        let kept = match result {
            Ok(kept) => kept,
            Err(e) => {
                self.forget_unwritten_config_save();
                form.update(cx, |form, cx| form.set_error(format!("{e:#}"), cx));
                return;
            }
        };
        let (name, folder) = (connection.name.clone(), connection.folder.clone());
        // The config may have changed while this saved, such as the text size: apply the
        // change to it as it is now, and save that too if it isn't what was written.
        if let Err(e) = self.config.upsert(connection, previous.as_deref()) {
            form.update(cx, |form, cx| form.set_error(format!("{e:#}"), cx));
            return;
        }
        if self.config != config {
            self.save_config(cx);
        }
        let renamed_from = previous.filter(|old| *old != name);
        if let Some(old) = &renamed_from {
            self.session.rename_connection(old, &name);
            self.schedule_session_save(cx);
            if let Some(ix) = self.tab_index(old) {
                self.tabs[ix].name = name.clone();
            }
            if self.active.as_deref() == Some(old.as_str()) {
                self.active = Some(name.clone());
            }
        }
        self.sync_tabs(cx);
        self.connections
            .update(cx, |connections, _| connections.reveal(&folder));
        match self.tab(&name).cloned() {
            // Its new settings take effect: it connects again.
            Some(tab) => {
                self.form = None;
                self.db.allow_connecting(&name);
                let conn = self.config.find(&name).cloned();
                tab.update(cx, |tab, cx| {
                    if let Some(conn) = conn {
                        tab.reconnect(conn, cx);
                    }
                });
                self.show_tab(&name, cx);
            }
            // Renaming one that is not open leaves the open tab on screen.
            None if renamed_from.is_some() => {
                self.form = None;
                cx.notify();
            }
            None => self.open_connection(name, window, cx),
        }
        self.show_saved(Ok(kept), cx);
    }

    fn confirm_delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (this, target) = (this.clone(), name.clone());
            alert
                .title(format!("Delete the connection “{name}”?"))
                .description(
                    "Only the saved connection and its password in the keychain are removed. \
                     The database and its data are not touched.",
                )
                .confirm()
                .footer(dialogs::confirm_footer("Delete connection"))
                .on_ok({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |this, cx| this.delete_connection(target.clone(), cx))
                            .ok();
                        true
                    }
                })
                // Asked from the Connections dialog, which takes the keys back.
                .on_close(move |_, window, cx| {
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let this = this.read(cx);
                    if this.connections_open {
                        let connections = this.connections.clone();
                        ConnectionsDialog::take_keys_back(&connections, window, cx);
                    }
                })
        });
    }

    fn delete_connection(&mut self, name: String, cx: &mut Context<Self>) {
        self.remove_tab(&name, cx);
        if self
            .form
            .as_ref()
            .is_some_and(|form| form.read(cx).editing() == Some(name.as_str()))
        {
            self.form = None;
        }
        self.connected.remove(&name);
        let removed = self.config.find(&name).cloned();
        self.config.remove(&name);
        self.session.remove_connection(&name);
        self.schedule_session_save(cx);
        let save = self.config_saves.save(self.config.clone());
        let db = self.db.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let forget_name = name.clone();
            // The keychain first, put back if the config can't be written.
            let result =
                blocking(move || secrets::delete_connection(&name, || save.run(Config::save)))
                    .await;
            io(async move {
                db.forget(&forget_name).await;
                Ok(())
            })
            .await
            .ok();
            this.update(cx, |this, cx| {
                this.finish_deleting_connection(removed, result, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Once deleting a connection is written, or failed and was undone.
    fn finish_deleting_connection(
        &mut self,
        removed: Option<Connection>,
        result: anyhow::Result<Option<PathBuf>>,
        cx: &mut Context<Self>,
    ) {
        if result.is_err() {
            // Nothing was deleted: show the connection again, as it still is on disk.
            if let Some(conn) = removed {
                self.config.upsert(conn, None).ok();
                cx.notify();
            }
            self.forget_unwritten_config_save();
        }
        self.show_saved(result, cx);
    }

    /// After a connection's save or delete failed and its keychain change was undone: the
    /// config as it is becomes the newest save, so quitting doesn't write the failed one.
    fn forget_unwritten_config_save(&mut self) {
        self.config_saves.save(self.config.clone());
    }

    #[cfg(test)]
    pub(crate) fn session(&self) -> &SessionState {
        &self.session
    }

    #[cfg(test)]
    pub(crate) fn session_mut(&mut self) -> &mut SessionState {
        &mut self.session
    }

    fn sized_panel(&self, panel: Panel) -> ResizablePanel {
        panel.sized(&self.session.panels)
    }

    /// Keeps the sizes these panels of one group were dragged to.
    fn remember_panel_sizes(
        &self,
        panels: &'static [Panel],
        cx: &mut Context<Self>,
    ) -> impl Fn(&Entity<ResizableState>, &mut Window, &mut App) + 'static {
        let this = cx.entity().downgrade();
        move |state, _, cx| {
            let sizes = Panel::dragged_sizes(panels, state, cx);
            this.update(cx, |this, cx| {
                for (key, size) in sizes {
                    this.session.panels.insert(key.into(), size);
                }
                this.schedule_session_save(cx);
            })
            .ok();
        }
    }

    /// Saves shortly after the last change, so typing doesn't write on every keystroke.
    fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        self.session_unsaved = true;
        self._session_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(800))
                .await;
            let Ok(save) = this.update(cx, |this, _| this.session_save()) else {
                return;
            };
            blocking(move || save.run(SessionState::save)).await.ok();
        }));
    }

    fn session_save(&mut self) -> Save<SessionState> {
        self.session_unsaved = false;
        self.session_saves.save(self.session.clone())
    }

    /// What is left to write when the app quits: the session, if a change still waits
    /// for its delayed save, and the newest config save, skipped if already written.
    fn final_saves(&mut self) -> (Option<Save<SessionState>>, Option<Save<Config>>) {
        self._session_save = None;
        let session = self.session_unsaved.then(|| self.session_save());
        (session, self.config_saves.latest())
    }

    /// Writes [`Self::final_saves`] in the background; the app waits a little for it.
    fn flush_saves(&mut self) -> impl Future<Output = ()> + use<> {
        let (session, config) = self.final_saves();
        let saved = (session.is_some() || config.is_some()).then(|| {
            blocking(move || {
                if let Some(save) = session {
                    save.run(SessionState::save).ok();
                }
                if let Some(save) = config {
                    save.run(Config::save).ok();
                }
                Ok(())
            })
        });
        async move {
            if let Some(saved) = saved {
                saved.await.ok();
            }
        }
    }

    /// Downloads an update while the app stays open. Development builds go to the
    /// download page instead, so a release can't overwrite them.
    fn download_update(&mut self, version: String, cx: &mut Context<Self>) {
        let installation = std::env::current_exe()
            .ok()
            .and_then(|exe| updates::detect_installation(&exe));
        match installation {
            Some(installation) => self.start_download(version, installation, cx),
            None => cx.open_url(updates::DOWNLOAD_URL),
        }
    }

    fn start_download(
        &mut self,
        version: String,
        installation: updates::Installation,
        cx: &mut Context<Self>,
    ) {
        let percent = Arc::new(AtomicU8::new(0));
        self.update_state = Some(UpdateState::Downloading {
            version: version.clone(),
            percent: percent.clone(),
        });
        cx.notify();

        // Redraws as the percentage moves, until the download ends.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let downloading = this.update(cx, |this, cx| {
                    cx.notify();
                    matches!(this.update_state, Some(UpdateState::Downloading { .. }))
                });
                if !matches!(downloading, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let wanted = version.clone();
            let result = blocking(move || {
                updates::download(&wanted, |p| percent.store(p, Ordering::Relaxed))
            })
            .await;
            this.update(cx, |this, cx| {
                // Unticking "Check for updates" abandons the download.
                if !matches!(this.update_state, Some(UpdateState::Downloading { .. })) {
                    return;
                }
                this.update_state = Some(match result {
                    Ok(download) => UpdateState::Ready {
                        download,
                        installation,
                    },
                    Err(e) => UpdateState::Available {
                        version,
                        error: Some(format!("{e:#}").into()),
                    },
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let Some(UpdateState::Ready {
            download,
            installation,
        }) = &self.update_state
        else {
            return;
        };
        match updates::spawn_upgrade(installation, download) {
            Ok(()) => {
                // The usual delayed save would not run after quitting.
                self.session.save().ok();
                cx.quit();
            }
            Err(e) => {
                self.update_state = Some(UpdateState::Available {
                    version: download.version.clone(),
                    error: Some(format!("{e:#}").into()),
                });
                cx.notify();
            }
        }
    }

    fn toggle_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.chat_open = !self.chat_open;
        if self.chat_open {
            self.chat.update(cx, |chat, cx| chat.focus(window, cx));
        }
        cx.notify();
    }

    fn open_about(&self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        window.open_dialog(cx, |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            dialog
                .w(px(320.))
                .p_0()
                .close_button(false)
                .child(dialogs::with_close_button(
                    v_flex()
                        .id("about")
                        .test_support()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_6()
                        .child(logos::brand(72.))
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .child(tuplebase_core::PRODUCT_NAME),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(concat!("Version ", env!("CARGO_PKG_VERSION"))),
                        ),
                ))
        });
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // A failure, or where an unreadable connections file was kept: both matter most
        // here, when no connections could be shown.
        let failed = matches!(self.status, Status::Failed(_)) || self.kept_note.is_some();
        let none_saved = self.config.connections.is_empty();
        v_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(logos::brand(72.))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(if none_saved {
                                "Add a connection to start querying."
                            } else {
                                "Open a connection to start querying."
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .when(!none_saved, |el| {
                                el.child(
                                    Button::new("empty-open")
                                        .cursor_pointer()
                                        .primary()
                                        .label("Open a connection")
                                        .when_some(
                                            Keystroke::parse(CONNECTIONS_KEY).ok(),
                                            |button, key| button.child(Kbd::new(key)),
                                        )
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.toggle_connections(window, cx)
                                        })),
                                )
                            })
                            .child(
                                Button::new("empty-add")
                                    .cursor_pointer()
                                    .when(none_saved, |button| button.primary())
                                    .when(!none_saved, |button| button.outline())
                                    .label("Add connection")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_form(None, String::new(), window, cx)
                                    })),
                            ),
                    ),
            )
            .when(failed, |el| {
                el.child(connection_tab::render_status(&self.status, cx))
            })
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let shown = self.active_tab().filter(|_| self.form.is_none()).cloned();
        let color = shown.as_ref().map(|tab| {
            let conn = tab.read(cx).connection();
            logos::color_of(conn.adapter, &conn.color)
        });
        let main = if let Some(form) = self.form.clone() {
            div().size_full().child(form).into_any_element()
        } else if let Some(tab) = shown {
            tab.into_any_element()
        } else {
            self.render_empty(cx).into_any_element()
        };
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .when(cfg!(not(target_os = "macos")), |el| {
                el.child(TitleBar::new().child(self.app_menu.clone()))
            })
            .child(self.render_top_bar(cx))
            .when_some(color, |el, color| {
                el.child(
                    div()
                        .id("connection-color")
                        .test_support()
                        .w_full()
                        .h(px(3.))
                        .flex_none()
                        .bg(color),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .on_action(cx.listener(|this, _: &RunQuery, _, cx| {
                        if let Some(tab) = this.active_tab().filter(|_| this.form.is_none()) {
                            tab.update(cx, |tab, cx| tab.run_query(cx));
                        }
                    }))
                    .on_action(cx.listener(|this, _: &RunAll, _, cx| {
                        if let Some(tab) = this.active_tab().filter(|_| this.form.is_none()) {
                            tab.update(cx, |tab, cx| tab.run_all(cx));
                        }
                    }))
                    .child(
                        h_resizable("workspace-panels")
                            .on_resize(self.remember_panel_sizes(&[Panel::Chat], cx))
                            .child(
                                resizable_panel()
                                    .size_range(px(MAIN_MIN_WIDTH)..Pixels::MAX)
                                    .child(div().size_full().min_w_0().child(main)),
                            )
                            .when(self.chat_open, |el| {
                                el.child(self.sized_panel(Panel::Chat).child(self.chat.clone()))
                            }),
                    ),
            )
    }
}

fn mcp_added_note(statuses: &[AgentStatus]) -> Notification {
    if statuses.is_empty() {
        return Notification::warning("No supported agents found on this computer.");
    }
    let failures: Vec<String> = statuses
        .iter()
        .filter_map(|status| match &status.state {
            AgentState::Failed(reason) => Some(format!("{}: {reason}", status.name)),
            _ => None,
        })
        .collect();
    if !failures.is_empty() {
        return Notification::error(failures.join("\n"));
    }
    let names: Vec<&str> = statuses.iter().map(|status| status.name).collect();
    Notification::success(format!(
        "MCP server added to {}. Restart them to use it.",
        names.join(", ")
    ))
}

fn mcp_status_text(servers: &[Option<String>]) -> String {
    let count = match servers.len() {
        0 => return "MCP server not running. Agents start it when they need it.".to_owned(),
        1 => "MCP server running for 1 agent".to_owned(),
        n => format!("MCP server running for {n} agents"),
    };
    // An agent with several sessions runs several servers; name it once.
    let mut names: Vec<&str> = servers.iter().flatten().map(String::as_str).collect();
    names.dedup();
    if names.is_empty() {
        count
    } else {
        format!("{count}: {}", names.join(", "))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    // No globs from gpui_kit here: its `test` macro would shadow the built-in `#[test]`.
    use super::connections::{ConnectionsDialog, TreeRow};
    use super::{
        About, AgentState, AgentStatus, Config, ConnFormEvent, Connection, ConnectionTab,
        OpenSettings, SettingsPage, Status, UpdateState, Workspace, bind_keys,
        register_menu_actions,
    };
    use crate::connection_tab::Light;
    use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, WindowExt as _};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Entity, TestAppContext, WindowBounds,
        WindowOptions, point, px, size,
    };
    use tuplebase_core::config::{Adapter, Appearance, Auth, AwsIamAuth, AwsSecretRef, TlsMode};

    pub(crate) fn open(
        config: Config,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
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
                let workspace =
                    cx.new(|cx| Workspace::with_config(config, Status::Idle, window, cx));
                register_menu_actions(&workspace, window.window_handle(), cx);
                workspace
            })
            .expect("open test window")
        })
    }

    pub(crate) fn config_with_connection() -> Config {
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
            auth: Default::default(),
            source: None,
        };
        config.upsert(conn, None).unwrap();
        config
    }

    #[test]
    fn mcp_status_text_counts_agents() {
        assert!(super::mcp_status_text(&[]).contains("not running"));
        assert_eq!(
            super::mcp_status_text(&[None]),
            "MCP server running for 1 agent"
        );
        assert_eq!(
            super::mcp_status_text(&[None, None, None]),
            "MCP server running for 3 agents"
        );
    }

    #[test]
    fn mcp_status_text_names_each_agent_once() {
        let named = |name: &str| Some(name.to_owned());
        assert_eq!(
            super::mcp_status_text(&[
                None,
                named("claude-code"),
                named("claude-code"),
                named("codex")
            ]),
            "MCP server running for 4 agents: claude-code, codex"
        );
    }

    #[gpui_kit::test]
    fn settings_switches_between_light_dark_and_system(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_settings(SettingsPage::General, window, cx);
            });
        });
        window_action(handle, cx, |window, cx| window.click("appearance-dark", cx));
        cx.update(|cx| {
            assert_eq!(workspace.read(cx).config.appearance, Appearance::Dark);
            assert!(cx.theme().mode.is_dark());
        });
        window_action(handle, cx, |window, cx| {
            window.click("appearance-light", cx)
        });
        cx.update(|cx| {
            assert_eq!(workspace.read(cx).config.appearance, Appearance::Light);
            assert!(!cx.theme().mode.is_dark());
        });
        window_action(handle, cx, |window, cx| {
            window.click("appearance-system", cx)
        });
        cx.update(|cx| assert!(workspace.read(cx).config.appearance.is_system()));
    }

    #[gpui_kit::test]
    fn settings_offers_each_agent_its_own_add_button(cx: &mut TestAppContext) {
        let status = |name, state| AgentStatus { name, state };
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("mcp-status", cx));
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.agents = Some(vec![
                    status("Cursor", AgentState::NotAdded),
                    status("Codex", AgentState::Added),
                ]);
                cx.notify();
            });
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("add-mcp-Cursor").visible());
            assert!(window.try_find("add-mcp-Codex").is_none());
            workspace.update(cx, |this, cx| {
                this.agents = Some(vec![
                    status("Cursor", AgentState::Added),
                    status("Codex", AgentState::Added),
                ]);
                cx.notify();
            });
        });
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("add-mcp-Cursor").is_none());
        });
    }

    #[gpui_kit::test]
    fn adding_to_one_agent_changes_only_its_row(cx: &mut TestAppContext) {
        let status = |name, state| AgentStatus { name, state };
        let (_, workspace) = open(Config::default(), cx);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.agents = Some(vec![
                    status("Cursor", AgentState::Added),
                    status("Codex", AgentState::NotAdded),
                ]);
                // A result read while Cursor was still being added must not undo it.
                this.update_agents(vec![status("Codex", AgentState::Added)], cx);
                assert_eq!(
                    this.agents,
                    Some(vec![
                        status("Cursor", AgentState::Added),
                        status("Codex", AgentState::Added),
                    ])
                );
            });
        });
    }

    #[gpui_kit::test]
    fn the_connection_color_shows_above_an_open_connection(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("connection-color").is_none());
        });
        open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, _| {
            assert!(window.find("connection-color").visible());
        });
    }

    #[gpui_kit::test]
    fn right_clicking_a_connection_edits_it_without_opening_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.right_click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.click(0usize, cx));
        window_action(handle, cx, |window, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.form.as_ref().unwrap().read(cx).editing(), Some("db"));
            assert!(this.tabs.is_empty());
            // The form shows in the window, not under the dialog.
            assert!(!window.has_active_dialog(cx));
        });
    }

    #[gpui_kit::test]
    fn right_clicking_a_connection_can_delete_it_after_asking(cx: &mut TestAppContext) {
        let (handle, _) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.right_click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.click(2usize, cx));
        window_action(handle, cx, |window, cx| {
            assert!(alert_open(window));
            // Its Cancel still reaches the dialog after the menu took focus.
            window.click("cancel", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(!alert_open(window));
            // The Connections dialog it was asked from stays.
            assert!(window.find("conn-db").visible());
        });
    }

    #[gpui_kit::test]
    fn right_clicking_a_folder_can_delete_it_after_asking(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.right_click("folder-Production", cx);
        });
        window_action(handle, cx, |window, cx| window.click(4usize, cx));
        window_action(handle, cx, |window, cx| window.click("ok", cx));
        window_action(handle, cx, |window, _| {
            assert!(!alert_open(window));
            assert!(window.try_find("folder-Production").is_none());
            assert!(window.find("conn-db").visible());
        });
        cx.update(|cx| assert!(workspace.read(cx).config.folders().is_empty()));
    }

    #[gpui_kit::test]
    fn right_clicking_a_folder_renames_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.right_click("folder-Production", cx);
        });
        window_action(handle, cx, |window, cx| window.click(3usize, cx));
        assert!(picker(&workspace, cx, |picker| picker
            .folder_edit
            .is_some()));
    }

    #[gpui_kit::test]
    fn deleting_another_connection_keeps_the_open_one(cx: &mut TestAppContext) {
        let mut config = config_with_connection();
        let other = Connection {
            name: "other".into(),
            ..config.find("db").unwrap().clone()
        };
        config.upsert(other, None).unwrap();
        let (handle, workspace) = open(config, cx);
        let db = open_tab(handle, &workspace, "db", cx);
        open_tab(handle, &workspace, "other", cx);
        cx.update(|cx| {
            db.update(cx, |tab, _| tab.status = Status::Failed("kept".into()));
            workspace.update(cx, |this, cx| {
                this.delete_connection("other".into(), cx);
                assert_eq!(this.active.as_deref(), Some("db"));
                assert_eq!(this.tabs.len(), 1);
                assert!(this.config.find("other").is_none());
            });
            assert!(matches!(db.read(cx).status, Status::Failed(_)));
        });
    }

    #[gpui_kit::test]
    fn deleting_a_connection_closes_its_edit_form(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.right_click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.click(0usize, cx));
        cx.update(|cx| {
            workspace.update(cx, |this, cx| this.delete_connection("db".into(), cx));
            assert!(workspace.read(cx).form.is_none());
        });
    }

    #[gpui_kit::test]
    fn the_light_shows_only_on_connected_connections(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("conn-status-db").is_none());
        });
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.connected.insert("db".into());
                cx.notify();
            });
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("conn-status-db").visible());
        });
    }

    #[test]
    fn the_light_says_connecting_then_connected_or_off() {
        use crate::connection_tab::light;
        assert_eq!(light(true, false), Light::Connecting);
        // Checking a pool that is already open.
        assert_eq!(light(true, true), Light::Connecting);
        assert_eq!(light(false, true), Light::Connected);
        assert_eq!(light(false, false), Light::Off);
    }

    #[gpui_kit::test]
    fn a_tab_shows_its_light(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| this.open_connection("db".into(), window, cx));
        });
        let tab = active(&workspace, cx);
        let light = |cx: &mut TestAppContext| tab.read_with(cx, |tab, _| tab.light());
        window_action(handle, cx, |window, _| {
            assert!(window.find("tab-status-db").visible());
        });
        assert_eq!(light(cx), Light::Connecting);
        // Connecting failed: the status line says why, and the light goes out.
        tab.update(cx, |tab, cx| {
            tab.connecting = false;
            tab.set_status(Status::Failed("refused".into()), cx);
        });
        assert_eq!(light(cx), Light::Off);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.connected.insert("db".into());
                this.show_connected_tabs(cx);
            });
        });
        assert_eq!(light(cx), Light::Connected);
    }

    #[gpui_kit::test]
    fn closing_the_open_tab_disconnects_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_tab(handle, &workspace, "db", cx);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.connected.insert("db".into());
                this.show_connected_tabs(cx);
                cx.notify();
            });
        });
        window_action(handle, cx, |window, cx| window.click("tab-close-db", cx));
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("tab-db").is_none());
            assert!(window.try_find("sql-editor").is_none());
            assert!(window.find("empty-open").visible());
        });
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active, None);
            assert!(this.tabs.is_empty());
            assert!(this.connected.is_empty());
        });
    }

    #[gpui_kit::test]
    fn closing_another_tab_keeps_the_open_one(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        open_tab(handle, &workspace, "other", cx);
        open_tab(handle, &workspace, "db", cx);
        cx.update(|cx| {
            workspace.update(cx, |this, _| {
                this.connected
                    .extend(["db".to_string(), "other".to_string()]);
            });
        });
        window_action(handle, cx, |window, cx| window.click("tab-close-other", cx));
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active.as_deref(), Some("db"));
            assert_eq!(this.tabs.len(), 1);
            assert!(this.connected.contains("db"));
            assert!(!this.connected.contains("other"));
        });
    }

    #[gpui_kit::test]
    fn the_right_click_menu_offers_disconnect_when_connected(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_tab(handle, &workspace, "db", cx);
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.connected.insert("db".into());
                cx.notify();
            });
        });
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.right_click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.click(0usize, cx));
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert!(this.connected.is_empty());
            // Its tab closes with it.
            assert!(this.tabs.is_empty());
        });
    }

    #[gpui_kit::test]
    fn no_disconnect_button_without_a_connection(cx: &mut TestAppContext) {
        let (handle, _) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.hover("conn-db", cx));
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("disconnect-db").is_none());
        });
    }

    #[gpui_kit::test]
    fn the_mcp_status_opens_settings(cx: &mut TestAppContext) {
        let (handle, _) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("mcp-status", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.has_active_dialog(cx));
            // It opens on the agents page, not the general one.
            assert!(window.try_find("check-updates").is_none());
        });
    }

    #[gpui_kit::test]
    fn the_chat_button_opens_the_panel_and_its_close_icon_closes_it(cx: &mut TestAppContext) {
        let (handle, _) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("chat-panel").is_none());
            window.click("toggle-chat", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("chat-panel").visible());
            window.click("chat-close", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("chat-panel").is_none());
        });
    }

    #[gpui_kit::test]
    fn a_chat_suggestion_is_asked_and_clearing_forgets_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("toggle-chat", cx));
        window_action(handle, cx, |window, cx| {
            window.click("chat-suggestion-0", cx);
        });
        let mut stop = None;
        window_action(handle, cx, |window, cx| {
            let chat = workspace.read(cx).chat.read(cx);
            assert_eq!(chat.asked(), ["Which connections can you see?"]);
            assert!(chat.is_busy());
            assert!(window.find("chat-busy").visible());
            stop = chat.turn_stop();
            window.click("chat-clear", cx);
        });
        cx.update(|cx| {
            let chat = workspace.read(cx).chat.read(cx);
            assert!(chat.asked().is_empty());
            assert!(!chat.is_busy());
        });
        // Clearing also ends the agent still answering.
        assert!(stop.expect("the turn has a stop").is_stopped());
    }

    #[gpui_kit::test]
    fn a_chat_tool_call_shows_its_arguments_only_when_clicked(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.click("toggle-chat", cx));
        cx.update(|cx| {
            let chat = workspace.read(cx).chat.clone();
            chat.update(cx, |chat, cx| {
                chat.show(
                    tuplebase_core::chat::Event::Tool {
                        name: "list_tables".into(),
                        input: r#"{"connection":"db"}"#.into(),
                    },
                    cx,
                );
            });
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("chat-tool-input-0").is_none());
            window.click("chat-tool-0", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("chat-tool-input-0").visible());
            window.click("chat-tool-0", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("chat-tool-input-0").is_none());
        });
    }

    #[gpui_kit::test]
    fn form_opens_validates_and_cancels(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("empty-add").visible());
            window.click("empty-add", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // A new connection asks where the database is, then which one it is; the rest
        // follows the choice.
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("save").is_none());
            assert!(window.try_find("adapter-PostgreSQL").is_none());
            window.click("source-manual", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("save").is_none());
            assert!(window.try_find("conn-name").is_none());
            window.click("adapter-PostgreSQL", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert_eq!(window.find("conn-host").focused(), Some(true));
            // Blank name is rejected and the form stays open.
            window.click("save", cx);
        });
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
    fn double_clicking_a_connection_opens_and_connects_its_tab(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.double_click("conn-db", cx));
        window_action(handle, cx, |window, cx| {
            // The dialog has done its job.
            assert!(!window.has_active_dialog(cx));
            assert!(window.find("tab-db").visible());
            // Ready to type in.
            let editor = workspace
                .read(cx)
                .active_tab()
                .unwrap()
                .read(cx)
                .editor
                .clone();
            assert!(gpui_kit::Focusable::focus_handle(editor.read(cx), cx).is_focused(window));
        });
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert!(!this.connections_open);
            assert_eq!(this.active.as_deref(), Some("db"));
            let tab = this.active_tab().unwrap().read(cx);
            assert!(matches!(tab.status, Status::Busy(_)));
            assert!(tab.connecting);
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
                this.update_state = Some(super::UpdateState::Available {
                    version: "9.9.9".into(),
                    error: None,
                });
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
    fn an_update_shows_progress_and_then_offers_a_restart(cx: &mut TestAppContext) {
        use tuplebase_core::updates::{Download, Installation};
        let (handle, workspace) = open(Config::default(), cx);
        let place = || Installation::Directory("/tmp/none".into());
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.start_download("9.9.9".into(), place(), cx);
            });
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("update-progress").visible());
            assert!(window.try_find("download-update").is_none());
        });
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.update_state = Some(super::UpdateState::Ready {
                    download: Download {
                        dir: "/tmp/none".into(),
                        version: "9.9.9".into(),
                    },
                    installation: place(),
                });
                cx.notify();
            });
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("restart-to-update").visible());
            assert!(window.try_find("update-progress").is_none());
        });
    }

    #[gpui_kit::test]
    fn clicking_a_folder_collapses_and_expands_it(cx: &mut TestAppContext) {
        let (handle, _) = open(config_with_connection(), cx);
        open_picker(handle, cx);
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

    pub(crate) fn window_action<R>(
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

    /// Opens `name`'s tab as a click on it does, done connecting.
    pub(crate) fn open_tab(
        handle: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        name: &str,
        cx: &mut TestAppContext,
    ) -> Entity<ConnectionTab> {
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_connection(name.into(), window, cx);
                let tab = this.tab(name).expect("the tab opens").clone();
                tab.update(cx, |tab, _| {
                    // Nothing listens at the test connection's port, and tests never
                    // finish connecting: don't wait for it.
                    tab.connecting = false;
                    tab.status = Status::Idle;
                });
                tab
            })
        })
    }

    /// Opens the Connections dialog, as its shortcut does.
    pub(crate) fn open_picker(handle: AnyWindowHandle, cx: &mut TestAppContext) {
        window_action(handle, cx, |window, cx| window.press("secondary-k", cx));
    }

    /// Reads what the Connections dialog keeps.
    fn picker<R>(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
        read: impl FnOnce(&ConnectionsDialog) -> R,
    ) -> R {
        cx.update(|cx| read(workspace.read(cx).connections.read(cx)))
    }

    /// Whether a confirm dialog is asking, on top of any other dialog.
    fn alert_open(window: &mut gpui_kit::Window) -> bool {
        window.try_find("ok").is_some()
    }

    /// The tab on screen.
    fn active(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Entity<ConnectionTab> {
        cx.update(|cx| {
            workspace
                .read(cx)
                .active_tab()
                .expect("a tab is open")
                .clone()
        })
    }

    /// Opens the form for a new connection and chooses PostgreSQL, which shows the rest.
    fn open_new_form(handle: AnyWindowHandle, cx: &mut TestAppContext) {
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| window.click("source-manual", cx));
        window_action(handle, cx, |window, cx| {
            window.click("adapter-PostgreSQL", cx)
        });
    }

    #[gpui_kit::test]
    fn sqlite_only_asks_for_a_file(cx: &mut TestAppContext) {
        let (handle, _) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| window.click("source-manual", cx));
        window_action(handle, cx, |window, cx| window.click("adapter-SQLite", cx));
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("conn-host").is_none());
            assert!(window.try_find("conn-password").is_none());
            assert!(window.try_find("use-ssh").is_none());
            assert!(window.try_find("conn-database").is_some());
        });
    }

    #[gpui_kit::test]
    fn redshift_is_a_server_on_its_own_default_port(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| window.click("source-manual", cx));
        window_action(handle, cx, |window, cx| {
            // The fifth button is laid out with the others.
            assert!(window.find("adapter-Redshift").bounds().size.width > px(0.));
            window.click("adapter-Redshift", cx);
        });
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event, _| {
                if let ConnFormEvent::Save { connection, .. } = event {
                    *saved.borrow_mut() = Some(connection.clone());
                }
            })
            .detach();
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-password").is_some());
            assert!(window.try_find("use-ssh").is_some());
            window.click("conn-name", cx);
            window.input("warehouse", cx);
            window.click("conn-host", cx);
            window.input("dw.abc123.eu-west-1.redshift.amazonaws.com", cx);
            window.click("conn-user", cx);
            window.input("awsuser", cx);
        });
        window_action(handle, cx, |window, cx| window.click("save", cx));
        let connection = saved.borrow_mut().take().expect("the form saved");
        assert_eq!(connection.adapter, Adapter::Redshift);
        assert_eq!(connection.port, 5439);
    }

    #[gpui_kit::test]
    fn ssh_fields_appear_and_require_a_fingerprint(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_new_form(handle, cx);
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
        });
        window_action(handle, cx, |window, cx| window.click("save", cx));
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert!(this.config.connections.is_empty());
            let form = this.form.as_ref().expect("form stays open").read(cx);
            assert!(form.error.is_some());
        });
    }

    #[gpui_kit::test]
    fn passwords_keep_their_spaces_while_other_fields_are_trimmed(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_new_form(handle, cx);
        window_action(handle, cx, |window, cx| window.click("use-ssh", cx));
        window_action(handle, cx, |window, cx| {
            window.click("conn-name", cx);
            window.input(" spaced ", cx);
            window.click("conn-host", cx);
            window.input(" db ", cx);
            window.click("conn-user", cx);
            window.input(" me ", cx);
            window.click("conn-password", cx);
            window.input("  pass word ", cx);
            // The SSH fields are below the fold.
            let down = gpui_kit::ScrollDelta::Pixels(point(px(0.), px(-2000.)));
            window.scroll("conn-form", down, cx);
        });
        window_action(handle, cx, |window, cx| {
            window.click("conn-ssh-host", cx);
            window.input("bastion", cx);
            window.click("conn-ssh-user", cx);
            window.input("me", cx);
            window.click("conn-ssh-password", cx);
            window.input(" phrase\t", cx);
            window.click("conn-ssh-fingerprint", cx);
            window.input("SHA256:abc", cx);
        });
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event, _| {
                if let ConnFormEvent::Save {
                    connection,
                    password,
                    ssh_password,
                    ..
                } = event
                {
                    *saved.borrow_mut() =
                        Some((connection.clone(), password.clone(), ssh_password.clone()));
                }
            })
            .detach();
        });
        window_action(handle, cx, |window, cx| window.click("save", cx));
        let (connection, password, ssh_password) =
            saved.borrow_mut().take().expect("the form saved");
        assert_eq!(password.as_deref(), Some("  pass word "));
        assert_eq!(ssh_password.as_deref(), Some(" phrase\t"));
        assert_eq!(
            (connection.name.as_str(), connection.host.as_str()),
            ("spaced", "db")
        );
        assert_eq!(connection.user, "me");
    }

    #[gpui_kit::test]
    fn aws_credentials_replace_the_password_and_can_name_the_host(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_new_form(handle, cx);
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-aws-name").is_none());
            window.click("auth-Secrets Manager", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-password").is_none());
            assert!(window.try_find("conn-host").is_some());
            window.click("conn-name", cx);
            window.input("orders", cx);
            window.click("conn-aws-profile", cx);
            window.input("prod-sso", cx);
            window.click("conn-aws-name", cx);
            window.input("prod/orders", cx);
            // Left behind in a field that is about to be hidden; it must not block saving.
            window.click("conn-port", cx);
            window.input("not a port", cx);
            window.click("use-secret-host", cx);
        });
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event, _| {
                if let ConnFormEvent::Save {
                    connection,
                    password,
                    ..
                } = event
                {
                    *saved.borrow_mut() = Some((connection.clone(), password.clone()));
                }
            })
            .detach();
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-host").is_none());
            window.click("save", cx);
        });
        let (connection, password) = saved.borrow_mut().take().expect("the form saved");
        assert_eq!(password, None);
        assert_eq!(connection.host, "");
        assert_eq!(
            connection.auth,
            Auth::AwsSecret(AwsSecretRef {
                profile: "prod-sso".into(),
                region: String::new(),
                name: "prod/orders".into(),
                use_secret_host: true,
            })
        );
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            assert!(form.read(cx).error.is_none());
        });
    }

    #[gpui_kit::test]
    fn where_a_kept_connections_file_went_shows_on_the_empty_screen(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let kept = std::path::PathBuf::from("/cfg/connections.unreadable-1.json");
                this.show_saved(Ok(Some(kept)), cx);
            });
            assert!(window.try_find("status-line").is_none());
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("status-line").visible());
        });
    }

    #[gpui_kit::test]
    fn a_kept_connections_file_stays_in_the_status_line_past_connecting(cx: &mut TestAppContext) {
        let (_, workspace) = open(config_with_connection(), cx);
        let kept = std::path::PathBuf::from("/cfg/connections.unreadable-1.json");
        workspace.update(cx, |this, cx| {
            this.show_saved(Ok(Some(kept.clone())), cx);
            assert!(matches!(&this.status, Status::Done(s) if s.contains("connections.unreadable-1.json")));
            // Saving a connection opens it, and connecting reports back.
            let Status::Done(text) = this.connected_status("db", &Ok(())) else {
                panic!("done expected");
            };
            assert!(text.starts_with("Connected to db. "), "{text}");
            assert!(text.contains("connections.unreadable-1.json"), "{text}");
            // Said once: the next connect is reported as usual.
            let status = this.connected_status("db", &Ok(()));
            assert!(matches!(&status, Status::Done(s) if s.as_ref() == "Connected to db"));
            // A plain save leaves the status line alone.
            this.status = status;
            this.show_saved(Ok(None), cx);
            assert!(matches!(&this.status, Status::Done(s) if s.as_ref() == "Connected to db"));
        });
    }

    #[gpui_kit::test]
    fn a_found_connection_moved_to_another_host_forgets_where_it_was_found(
        cx: &mut TestAppContext,
    ) {
        let (handle, workspace) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| window.click("source-manual", cx));
        let found = crate::aws_picker::tests::found("orders", Some(Adapter::Postgres))
            .connection()
            .unwrap();
        let saved = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update_window(handle, |_, window, cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            form.update(cx, |form, cx| form.prefill(&found, window, cx));
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event, _| {
                if let ConnFormEvent::Save { connection, .. } = event {
                    saved.borrow_mut().push(connection.source.is_some());
                }
            })
            .detach();
        })
        .unwrap();
        // Saved as found, it keeps its source (see the test below); here the host changes.
        window_action(handle, cx, |window, cx| {
            window.click("conn-host", cx);
            window.input(".example.com", cx);
        });
        window_action(handle, cx, |window, cx| window.click("save", cx));
        assert_eq!(*saved.borrow(), [false]);
    }

    #[gpui_kit::test]
    fn a_found_connection_taking_its_host_from_a_secret_forgets_where_it_was_found(
        cx: &mut TestAppContext,
    ) {
        let (handle, workspace) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| window.click("source-manual", cx));
        let found = crate::aws_picker::tests::found("orders", Some(Adapter::Postgres))
            .connection()
            .unwrap();
        let saved = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update_window(handle, |_, window, cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            form.update(cx, |form, cx| form.prefill(&found, window, cx));
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event, _| {
                if let ConnFormEvent::Save { connection, .. } = event {
                    saved.borrow_mut().push(connection.source.is_some());
                }
            })
            .detach();
        })
        .unwrap();
        window_action(handle, cx, |window, cx| {
            window.click("auth-Secrets Manager", cx)
        });
        window_action(handle, cx, |window, cx| {
            window.click("conn-aws-name", cx);
            window.input("prod/orders", cx);
            window.click("use-secret-host", cx);
        });
        window_action(handle, cx, |window, cx| window.click("save", cx));
        assert_eq!(*saved.borrow(), [false]);
    }

    #[gpui_kit::test]
    fn a_database_found_in_aws_fills_in_a_new_connection(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("save").is_none());
            window.click("source-aws", cx);
        });
        window_action(handle, cx, |window, cx| {
            window.click("aws-regions", cx);
            window.input("eu-west-1", cx);
        });
        window_action(handle, cx, |window, cx| window.click("aws-find", cx));
        // The search itself runs on the runtime, which tests don't; give its answer.
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            let source = form.read(cx).start().cloned().expect("source step shown");
            let picker = source.read(cx).aws_picker().cloned().expect("AWS step");
            picker.update(cx, |picker, cx| {
                picker.set_discovery(
                    tuplebase_core::cloud::Discovery {
                        found: vec![crate::aws_picker::tests::found(
                            "orders",
                            Some(Adapter::Postgres),
                        )],
                        ..Default::default()
                    },
                    cx,
                )
            });
        });
        window_action(handle, cx, |window, cx| window.click("aws-use", cx));
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            assert!(form.read(cx).start().is_none());
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event, _| {
                if let ConnFormEvent::Save {
                    connection,
                    previous,
                    ..
                } = event
                {
                    *saved.borrow_mut() = Some((connection.clone(), previous.clone()));
                }
            })
            .detach();
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("conn-notice").visible());
            assert!(window.try_find("conn-password").is_none());
            assert!(window.find("conn-aws-profile").visible());
            assert!(window.try_find("conn-tls-hint").is_some());
            window.click("save", cx);
        });
        let (connection, previous) = saved.borrow_mut().take().expect("the form saved");
        // A new connection, not an edit of one with the same name.
        assert_eq!(previous, None);
        assert_eq!(connection.name, "orders");
        assert_eq!(connection.host, "orders.abc.eu-west-1.rds.amazonaws.com");
        assert_eq!((connection.port, connection.user.as_str()), (5432, "admin"));
        assert_eq!(connection.tls, TlsMode::Require);
        assert_eq!(
            connection.auth,
            Auth::AwsIam(AwsIamAuth {
                profile: "prod".into(),
                region: "eu-west-1".into(),
            })
        );
        assert!(connection.source.is_some());
        cx.update(|cx| {
            // Where it looked is remembered for next time.
            let search = &workspace.read(cx).session.aws_search;
            assert_eq!(search.regions, ["eu-west-1"]);
            assert_eq!(search.profile, "");
        });
    }

    #[gpui_kit::test]
    fn the_credentials_choice_switches_the_fields_and_iam_turns_on_encryption(
        cx: &mut TestAppContext,
    ) {
        let (handle, workspace) = open(Config::default(), cx);
        open_new_form(handle, cx);
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-password").is_some());
            window.click("auth-IAM token", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-password").is_none());
            assert!(window.try_find("conn-aws-name").is_none());
            assert!(window.try_find("conn-aws-profile").is_some());
            assert!(window.try_find("conn-host").is_some());
            window.click("auth-Parameter Store", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("conn-aws-name").is_some());
            window.click("auth-IAM token", cx);
        });
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            let saved = saved.clone();
            cx.subscribe(&form, move |_, event, _| {
                if let ConnFormEvent::Save { connection, .. } = event {
                    *saved.borrow_mut() = Some(connection.clone());
                }
            })
            .detach();
        });
        window_action(handle, cx, |window, cx| {
            window.click("conn-name", cx);
            window.input("orders", cx);
            window.click("conn-host", cx);
            window.input("orders.abc.eu-west-1.rds.amazonaws.com", cx);
            window.click("conn-user", cx);
            window.input("app", cx);
            window.click("conn-aws-profile", cx);
            window.input("prod", cx);
        });
        window_action(handle, cx, |window, cx| window.click("save", cx));
        let connection = saved.borrow_mut().take().expect("the form saved");
        assert_eq!(
            connection.auth,
            Auth::AwsIam(AwsIamAuth {
                profile: "prod".into(),
                region: String::new(),
            })
        );
        assert_eq!(connection.tls, TlsMode::Require);
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form is open");
            assert!(form.read(cx).error.is_none());
        });
    }

    #[gpui_kit::test]
    fn each_tab_keeps_its_own_editor_and_draft(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.double_click("conn-db", cx));
        let db = active(&workspace, cx);
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 42", cx);
        });
        cx.update(|cx| {
            db.update(cx, |tab, cx| {
                tab.status = Status::Idle;
                tab.run_query(cx);
            });
        });
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.double_click("conn-other", cx)
        });
        // Its own editor, which starts with the connection's draft.
        let other = active(&workspace, cx);
        assert_ne!(other, db);
        window_action(handle, cx, |_, cx| {
            assert_eq!(other.read(cx).editor.read(cx).value().as_str(), "");
        });
        window_action(handle, cx, |window, cx| window.click("tab-db", cx));
        window_action(handle, cx, |_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active_tab(), Some(&db));
            assert_eq!(this.tabs.len(), 2);
            assert_eq!(db.read(cx).editor.read(cx).value().as_str(), "select 42");
            assert_eq!(this.session.drafts["db"], "select 42");
            assert_eq!(this.session.history_for("db").next(), Some("select 42"));
            assert_eq!(this.session.history_for("other").next(), None);
        });
    }

    #[gpui_kit::test]
    fn arrow_keys_walk_and_fold_the_tree(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        let cursor = |cx: &mut TestAppContext| {
            picker(&workspace, cx, |picker| picker.cursor.clone()).expect("a cursor")
        };
        let collapsed =
            |cx: &mut TestAppContext| picker(&workspace, cx, |picker| picker.collapsed.clone());
        // The search field has the keys as soon as the dialog opens, and passes Down on.
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.press("down", cx));
        assert_eq!(cursor(cx), TreeRow::Folder("Production".into()));

        // A click on a row gives the tree the keys, Left and Right among them.
        window_action(handle, cx, |window, cx| window.click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.press("left", cx));
        assert_eq!(cursor(cx), TreeRow::Folder("Production".into()));

        window_action(handle, cx, |window, cx| window.press("left", cx));
        assert!(collapsed(cx).contains("Production"));

        window_action(handle, cx, |window, cx| window.press("right", cx));
        assert!(collapsed(cx).is_empty());

        window_action(handle, cx, |window, cx| window.press("right", cx));
        assert_eq!(cursor(cx), TreeRow::Connection("db".into()));

        window_action(handle, cx, |window, cx| window.press("left", cx));
        assert_eq!(cursor(cx), TreeRow::Folder("Production".into()));

        // Enter on a connection opens it.
        window_action(handle, cx, |window, cx| window.press("down", cx));
        window_action(handle, cx, |window, cx| window.press("enter", cx));
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx))
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).active.as_deref(), Some("db")));
    }

    #[gpui_kit::test]
    fn a_connection_color_can_be_chosen_in_the_form(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_new_form(handle, cx);
        window_action(handle, cx, |window, cx| window.click("color-3", cx));
        cx.update(|cx| {
            let this = workspace.read(cx);
            assert_eq!(this.form.as_ref().unwrap().read(cx).color, "#E2A336");
        });
    }

    #[gpui_kit::test]
    fn settings_opens_in_a_dialog_and_its_close_icon_closes_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("check-updates").is_none());
            window.click("open-settings", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert_eq!(window.find("check-updates").checked(), Some(true));
            window.click("check-updates", cx);
        });
        cx.update(|cx| assert!(!workspace.read(cx).config.check_for_updates));
        window_action(handle, cx, |window, cx| {
            assert_eq!(window.find("check-updates").checked(), Some(false));
            // Scoped: on Windows the title bar has a "close" button too.
            window.within("surface").click("close", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx));
        });
    }

    #[gpui_kit::test]
    fn the_menu_and_shortcut_open_one_settings_dialog(cx: &mut TestAppContext) {
        let (handle, _) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| window.press("secondary-,", cx));
        window_action(handle, cx, |window, cx| {
            window.dispatch_action(Box::new(OpenSettings), cx);
        });
        window_action(handle, cx, |window, _| {
            assert_eq!(window.find_all("check-updates").len(), 1);
        });
    }

    #[gpui_kit::test]
    fn the_chat_opens_at_its_saved_width_and_dragging_its_edge_resizes_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        workspace.update(cx, |this, _| {
            this.session.panels.insert("chat".into(), 420);
        });
        window_action(handle, cx, |window, cx| window.click("toggle-chat", cx));
        let chat = window_action(handle, cx, |window, _| window.find("chat-panel").bounds());
        assert_eq!(chat.size.width, px(420.));
        let edge = point(chat.left(), px(400.));
        window_action(handle, cx, |window, cx| {
            window.drag(edge, edge - point(px(60.), px(0.)), cx);
        });
        window_action(handle, cx, |window, _| {
            assert_eq!(window.find("chat-panel").bounds().size.width, px(480.));
        });
        cx.update(|cx| {
            let panels = &workspace.read(cx).session.panels;
            assert_eq!(panels["chat"], 480);
        });
    }

    #[gpui_kit::test]
    fn shortcuts_resize_the_text_and_reset_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        let sizes = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                let theme = Theme::global(cx);
                (
                    workspace.read(cx).config.font_size,
                    theme.font_size,
                    theme.mono_font_size,
                )
            })
        };
        assert_eq!(sizes(cx), (15, px(15.), px(13.)));
        for _ in 0..2 {
            window_action(handle, cx, |window, cx| window.press("secondary-=", cx));
        }
        assert_eq!(sizes(cx), (17, px(17.), px(15.)));
        window_action(handle, cx, |window, _| {
            assert_eq!(window.rem_size(), px(17.));
        });
        window_action(handle, cx, |window, cx| window.press("secondary--", cx));
        assert_eq!(sizes(cx), (16, px(16.), px(14.)));
        window_action(handle, cx, |window, cx| window.press("secondary-0", cx));
        assert_eq!(sizes(cx), (15, px(15.), px(13.)));
    }

    #[gpui_kit::test]
    fn the_text_size_survives_a_switch_between_light_and_dark(cx: &mut TestAppContext) {
        let config = Config {
            font_size: 20,
            ..Config::default()
        };
        let (_, _) = open(config, cx);
        cx.update(|cx| {
            Theme::change(ThemeMode::Dark, None, cx);
            assert_eq!(Theme::global(cx).font_size, px(20.));
            Theme::change(ThemeMode::Light, None, cx);
            assert_eq!(Theme::global(cx).font_size, px(20.));
        });
    }

    #[gpui_kit::test]
    fn the_menu_opens_about(cx: &mut TestAppContext) {
        let (handle, _) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            window.dispatch_action(Box::new(About), cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("about").visible());
            // The app's close icon, in the dialog's top right corner like the library's.
            let (close, surface) = (
                window.within("surface").find("close").bounds(),
                window.find("surface").bounds(),
            );
            assert!((close.top() - surface.top() - px(8.)).abs() <= px(1.));
            assert!((surface.right() - close.right() - px(8.)).abs() <= px(1.));
            window.within("surface").click("close", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx));
        });
    }

    #[gpui_kit::test]
    fn a_menu_check_that_finds_a_version_offers_it(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                this.finish_update_check(Ok(Some("9.9.9".into())), window, cx);
            });
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("download-update").visible());
        });
    }

    #[gpui_kit::test]
    fn a_menu_check_does_not_disturb_a_download(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                this.update_state = Some(UpdateState::Downloading {
                    version: "9.9.9".into(),
                    percent: Default::default(),
                });
                this.finish_update_check(Ok(Some("9.9.10".into())), window, cx);
            });
        });
        cx.update(|cx| {
            assert!(matches!(
                workspace.read(cx).update_state,
                Some(UpdateState::Downloading { .. })
            ));
        });
    }

    #[gpui_kit::test]
    fn settings_leaves_an_open_form_in_place(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-connection", cx));
        window_action(handle, cx, |window, cx| window.click("open-settings", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.has_active_dialog(cx));
            window.press("escape", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx));
        });
        cx.update(|cx| assert!(workspace.read(cx).form.is_some()));
    }

    #[gpui_kit::test]
    fn the_run_shortcut_does_nothing_behind_the_settings_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        let tab = open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| window.click("sql-editor", cx));
        window_action(handle, cx, |window, cx| window.click("open-settings", cx));
        window_action(handle, cx, |window, cx| {
            window.press("secondary-enter", cx);
        });
        cx.update(|cx| {
            assert!(!matches!(tab.read(cx).status, Status::Busy(_)));
        });
    }

    #[gpui_kit::test]
    fn new_folder_is_created_from_the_connections_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            window.input("Local", cx);
            window.press("enter", cx);
        });
        window_action(handle, cx, |window, _| {
            // Enter names the folder and leaves the dialog open.
            assert!(window.find("folder-Local").visible());
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).config.folders(), ["Local"]));
    }

    #[gpui_kit::test]
    fn enter_in_the_folder_field_leaves_the_tree_alone(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.press("down", cx));
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            window.input("Local", cx);
            window.press("enter", cx);
        });
        cx.update(|cx| {
            assert!(
                workspace
                    .read(cx)
                    .config
                    .folders()
                    .contains(&"Local".to_owned())
            );
        });
        assert!(picker(&workspace, cx, |picker| picker.collapsed.is_empty()));
    }

    #[gpui_kit::test]
    fn escape_cancels_creating_a_folder(cx: &mut TestAppContext) {
        let (handle, workspace) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            window.input("Nope", cx);
            window.press("escape", cx);
        });
        window_action(handle, cx, |window, cx| {
            // Only the field goes; a second Escape closes the dialog.
            assert!(window.try_find("folder-name").is_none());
            assert!(window.has_active_dialog(cx));
            window.press("escape", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(!window.has_active_dialog(cx))
        });
        cx.update(|cx| assert!(workspace.read(cx).config.folders().is_empty()));
    }

    #[gpui_kit::test]
    fn clicking_elsewhere_closes_the_folder_field(cx: &mut TestAppContext) {
        let (handle, _) = open(Config::default(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("add-folder", cx));
        window_action(handle, cx, |window, cx| {
            assert!(window.find("folder-name").visible());
            window.click("connections-title", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("folder-name").is_none());
        });
    }

    #[gpui_kit::test]
    fn a_folder_can_be_created_inside_another(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
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
        open_picker(handle, cx);
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
        open_picker(handle, cx);
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
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.hover("folder-Production", cx);
            window.click("add-in-Production", cx);
        });
        assert!(picker(&workspace, cx, |picker| picker.collapsed.is_empty()));
        cx.update(|cx| {
            let form = workspace.read(cx).form.clone().expect("form opens");
            assert_eq!(form.read(cx).folder, "Production");
        });
    }

    #[gpui_kit::test]
    fn deleting_an_empty_folder_needs_no_confirmation(cx: &mut TestAppContext) {
        let mut config = Config::default();
        config.add_folder("", "Old").unwrap();
        let (handle, workspace) = open(config, cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.hover("folder-Old", cx);
            window.click("remove-Old", cx);
        });
        cx.update(|cx| assert!(workspace.read(cx).config.folders().is_empty()));
    }

    #[gpui_kit::test]
    fn the_buttons_of_a_confirm_dialog_cancel_and_confirm(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        let ask = |cx: &mut TestAppContext| {
            window_action(handle, cx, |window, cx| {
                window.hover("folder-Production", cx);
                window.click("remove-Production", cx);
            });
        };
        let folders = |cx: &mut TestAppContext| {
            workspace.read_with(cx, |this, _| this.config.folders().len())
        };
        ask(cx);
        window_action(handle, cx, |window, cx| {
            assert!(alert_open(window));
            // The app's own Cancel and OK, laid out as the library's: side by side, as
            // wide as their labels, OK on the right.
            let (cancel, ok) = (window.find("cancel").bounds(), window.find("ok").bounds());
            assert_eq!(cancel.center().y, ok.center().y);
            assert!(cancel.right() < ok.left());
            assert!(cancel.size.width < px(100.) && ok.size.width < px(150.));
            window.click("cancel", cx);
        });
        window_action(handle, cx, |window, _| assert!(!alert_open(window)));
        assert_eq!(folders(cx), 1);
        ask(cx);
        window_action(handle, cx, |window, cx| window.press("escape", cx));
        window_action(handle, cx, |window, _| assert!(!alert_open(window)));
        assert_eq!(folders(cx), 1);
        ask(cx);
        window_action(handle, cx, |window, cx| window.click("ok", cx));
        window_action(handle, cx, |window, _| assert!(!alert_open(window)));
        assert_eq!(folders(cx), 0);
    }

    #[gpui_kit::test]
    fn enter_confirms_a_confirm_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.hover("folder-Production", cx);
            window.click("remove-Production", cx);
        });
        window_action(handle, cx, |window, cx| window.press("enter", cx));
        window_action(handle, cx, |window, _| assert!(!alert_open(window)));
        cx.update(|cx| assert!(workspace.read(cx).config.folders().is_empty()));
    }

    #[gpui_kit::test]
    fn dragging_a_connection_onto_a_folder_moves_it(cx: &mut TestAppContext) {
        let mut config = config_with_connection();
        config.add_folder("", "Local").unwrap();
        let (handle, workspace) = open(config, cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.drag_to("conn-db", "folder-Local", cx);
        });
        cx.update(|cx| assert_eq!(workspace.read(cx).config.connections[0].folder, "Local"));
    }

    pub(crate) fn config_with_two_connections() -> Config {
        let mut config = config_with_connection();
        let mut other = config.connections[0].clone();
        other.name = "other".into();
        config.upsert(other, None).unwrap();
        config
    }

    /// Opens the edit form of `name`, as its right-click menu does.
    fn edit_form(
        handle: AnyWindowHandle,
        workspace: &Entity<Workspace>,
        name: &str,
        cx: &mut TestAppContext,
    ) -> Entity<crate::conn_form::ConnForm> {
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let conn = this.config.find(name).unwrap().clone();
                this.edit_connection(conn, window, cx);
                this.form.clone().unwrap()
            })
        })
    }

    #[gpui_kit::test]
    fn renaming_a_connection_that_is_not_open_keeps_both_drafts(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        cx.update(|cx| {
            workspace.update(cx, |this, _| {
                this.session.set_draft("other", "select 'other'")
            });
        });
        open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 42", cx);
        });
        let form = edit_form(handle, &workspace, "other", cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let mut renamed = this.config.find("other").unwrap().clone();
                renamed.name = "renamed".into();
                let mut saved = this.config.clone();
                saved.upsert(renamed.clone(), Some("other")).unwrap();
                let previous = Some("other".into());
                let saving = super::Saving {
                    form,
                    connection: renamed,
                    previous,
                };
                this.finish_saving_connection(saving, saved, Ok(None), window, cx);
            });
        });
        window_action(handle, cx, |_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active.as_deref(), Some("db"));
            assert_eq!(this.tabs.len(), 1);
            let editor = this.active_tab().unwrap().read(cx).editor.read(cx);
            assert_eq!(editor.value().as_str(), "select 42");
            assert_eq!(this.session.drafts["db"], "select 42");
            assert_eq!(this.session.drafts["renamed"], "select 'other'");
            assert!(!this.session.drafts.contains_key("other"));
            assert!(this.form.is_none());
        });
    }

    #[gpui_kit::test]
    fn renaming_the_open_connection_keeps_it_open_with_its_draft(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 42", cx);
        });
        let form = edit_form(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let mut renamed = this.config.find("db").unwrap().clone();
                renamed.name = "renamed".into();
                let mut saved = this.config.clone();
                saved.upsert(renamed.clone(), Some("db")).unwrap();
                let previous = Some("db".into());
                let saving = super::Saving {
                    form,
                    connection: renamed,
                    previous,
                };
                this.finish_saving_connection(saving, saved, Ok(None), window, cx);
            });
        });
        window_action(handle, cx, |_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active.as_deref(), Some("renamed"));
            let tab = this.active_tab().unwrap().read(cx);
            assert_eq!(tab.name(), "renamed");
            assert_eq!(tab.editor.read(cx).value().as_str(), "select 42");
            assert_eq!(this.session.drafts["renamed"], "select 42");
            assert!(!this.session.drafts.contains_key("db"));
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("tab-renamed").visible());
            assert!(window.try_find("tab-db").is_none());
        });
    }

    #[gpui_kit::test]
    fn an_edit_to_an_open_connection_reaches_its_tab(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        let tab = open_tab(handle, &workspace, "db", cx);
        // An edit saved from the form: the tab runs with the new settings, such as the
        // read-only switch.
        let form = edit_form(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let mut edited = this.config.find("db").unwrap().clone();
                edited.readonly = true;
                let mut saved = this.config.clone();
                saved.upsert(edited.clone(), Some("db")).unwrap();
                let saving = super::Saving {
                    form,
                    connection: edited,
                    previous: Some("db".into()),
                };
                this.finish_saving_connection(saving, saved, Ok(None), window, cx);
            });
        });
        cx.update(|cx| {
            let tab = tab.read(cx);
            assert!(tab.connection().readonly);
            // It connects again, with the new settings.
            assert!(tab.connecting);
        });
        // A change made in the dialog, such as a move to another folder.
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                assert!(this.move_connection("db", "Staging", cx));
            });
            assert_eq!(tab.read(cx).connection().folder, "Staging");
        });
        window_action(handle, cx, |window, _| {
            let crumb = window.find("breadcrumb");
            assert!(crumb.visible());
        });
    }

    fn config_with_three_connections() -> Config {
        let mut config = config_with_two_connections();
        let mut third = config.connections[0].clone();
        third.name = "third".into();
        config.upsert(third, None).unwrap();
        config
    }

    fn tab_names(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Vec<String> {
        workspace.read_with(cx, |this, _| {
            this.tabs.iter().map(|open| open.name.clone()).collect()
        })
    }

    fn active_name(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<String> {
        workspace.read_with(cx, |this, _| this.active.clone())
    }

    #[gpui_kit::test]
    fn the_shortcut_and_the_button_open_one_connections_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        let shown = |cx: &mut TestAppContext| {
            window_action(handle, cx, |window, _| {
                window.find_all("connections-dialog").len()
            })
        };
        assert_eq!(shown(cx), 0);
        open_picker(handle, cx);
        assert_eq!(shown(cx), 1);
        // The shortcut again closes it.
        open_picker(handle, cx);
        assert_eq!(shown(cx), 0);
        window_action(handle, cx, |window, cx| {
            window.click("open-connections", cx)
        });
        assert_eq!(shown(cx), 1);
        // The menu's item while it is open closes it too: never two of them.
        window_action(handle, cx, |window, cx| {
            window.dispatch_action(Box::new(super::OpenConnections), cx);
        });
        assert_eq!(shown(cx), 0);
        window_action(handle, cx, |window, cx| window.click("new-tab", cx));
        assert_eq!(shown(cx), 1);
        window_action(handle, cx, |window, cx| window.press("escape", cx));
        assert_eq!(shown(cx), 0);
        assert!(workspace.read_with(cx, |this, _| !this.connections_open));
        window_action(handle, cx, |window, cx| window.click("empty-open", cx));
        assert_eq!(shown(cx), 1);
    }

    #[gpui_kit::test]
    fn the_shortcut_leaves_a_question_asked_from_the_dialog(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.right_click("conn-db", cx));
        window_action(handle, cx, |window, cx| window.click(2usize, cx));
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            // Neither the question nor the dialog under it went.
            assert!(alert_open(window));
            assert!(window.find("connections-dialog").visible());
            window.click("cancel", cx);
        });
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("connections-dialog").is_none());
            assert!(!window.has_active_dialog(cx));
        });
        assert!(workspace.read_with(cx, |this, _| !this.connections_open));
    }

    #[gpui_kit::test]
    fn the_shortcut_works_from_the_editor_but_not_over_settings(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| window.click("sql-editor", cx));
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert!(window.find("connections-dialog").visible());
        });
        // Nothing was typed into the editor.
        let tab = active(&workspace, cx);
        assert_eq!(
            tab.read_with(cx, |tab, cx| tab.editor.read(cx).value().to_string()),
            ""
        );
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.click("open-settings", cx));
        open_picker(handle, cx);
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("connections-dialog").is_none());
            assert!(window.find("check-updates").visible());
        });
    }

    #[gpui_kit::test]
    fn opening_two_connections_gives_two_tabs_with_the_second_on_screen(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        for name in ["db", "other"] {
            open_picker(handle, cx);
            let id = gpui_kit::SharedString::from(format!("conn-{name}"));
            window_action(handle, cx, |window, cx| window.double_click(id, cx));
        }
        assert_eq!(tab_names(&workspace, cx), ["db", "other"]);
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("other"));
        window_action(handle, cx, |window, _| {
            assert!(window.find("tab-db").visible());
            assert!(window.find("tab-other").visible());
            assert!(window.find("tab-close-other").visible());
        });
        // Opening one that has a tab shows that tab, rather than another.
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.double_click("conn-db", cx));
        assert_eq!(tab_names(&workspace, cx), ["db", "other"]);
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("db"));
    }

    #[gpui_kit::test]
    fn clicking_a_tab_shows_its_editor_results_and_schema(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        let db = open_tab(handle, &workspace, "db", cx);
        let other = open_tab(handle, &workspace, "other", cx);
        window_action(handle, cx, |window, cx| window.click("tab-db", cx));
        assert_eq!(active(&workspace, cx), db);
        // What is typed goes to the tab on screen only.
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 'db'", cx);
        });
        window_action(handle, cx, |window, cx| window.click("tab-other", cx));
        assert_eq!(active(&workspace, cx), other);
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 'other'", cx);
        });
        cx.update(|cx| {
            let text =
                |tab: &Entity<ConnectionTab>| tab.read(cx).editor.read(cx).value().to_string();
            assert_eq!(text(&db), "select 'db'");
            assert_eq!(text(&other), "select 'other'");
            // Each tab has its own results and schema, for its own connection.
            let (db, other) = (db.read(cx), other.read(cx));
            assert_ne!(db.results.entity_id(), other.results.entity_id());
            assert_ne!(db.schema.entity_id(), other.schema.entity_id());
        });
    }

    #[gpui_kit::test]
    fn closing_the_tab_on_screen_shows_its_neighbour(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_three_connections(), cx);
        for name in ["db", "other", "third"] {
            open_tab(handle, &workspace, name, cx);
        }
        window_action(handle, cx, |window, cx| window.click("tab-other", cx));
        window_action(handle, cx, |window, cx| window.click("tab-close-other", cx));
        // The one to its right.
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("third"));
        window_action(handle, cx, |window, cx| window.click("tab-close-third", cx));
        // The last one has none to its right: the one to its left.
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("db"));
        // A tab not on screen closes without changing which one is.
        open_tab(handle, &workspace, "other", cx);
        window_action(handle, cx, |window, cx| window.click("tab-close-db", cx));
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("other"));
        assert_eq!(tab_names(&workspace, cx), ["other"]);
    }

    #[gpui_kit::test]
    fn a_result_arriving_after_its_tab_closed_is_dropped(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        let tab = open_tab(handle, &workspace, "db", cx);
        tab.update(cx, |tab, cx| tab.run_sql("select 1".into(), cx));
        let running = tab.downgrade();
        drop(tab);
        window_action(handle, cx, |window, cx| window.click("tab-close-db", cx));
        window_action(handle, cx, |_, _| {});
        // The query's task holds the tab weakly: with the tab gone, its result has
        // nowhere to go.
        assert!(running.upgrade().is_none());
    }

    #[gpui_kit::test]
    fn the_run_key_runs_only_in_the_tab_on_screen(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        let db = open_tab(handle, &workspace, "db", cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.double_click("conn-other", cx)
        });
        let other = active(&workspace, cx);
        other.update(cx, |tab, cx| {
            tab.connecting = false;
            tab.status = Status::Idle;
            cx.notify();
        });
        // Picking it put the keys in its editor: no click needed.
        window_action(handle, cx, |window, cx| {
            window.input("select 2", cx);
            window.press("secondary-enter", cx);
        });
        cx.update(|cx| {
            assert!(matches!(other.read(cx).status, Status::Busy(_)));
            assert!(matches!(db.read(cx).status, Status::Idle));
            let session = &workspace.read(cx).session;
            assert_eq!(session.history_for("other").next(), Some("select 2"));
            assert_eq!(session.history_for("db").next(), None);
        });
    }

    #[gpui_kit::test]
    fn a_dragged_panel_size_holds_in_every_tab(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        open_tab(handle, &workspace, "db", cx);
        open_tab(handle, &workspace, "other", cx);
        window_action(handle, cx, |window, cx| window.click("tab-db", cx));
        let editor = window_action(handle, cx, |window, _| window.find("sql-editor").bounds());
        let edge = point(editor.center().x, editor.bottom());
        window_action(handle, cx, |window, cx| {
            window.drag(edge, edge + point(px(0.), px(60.)), cx);
        });
        let height = |cx: &mut TestAppContext| {
            window_action(handle, cx, |window, _| {
                window.find("sql-editor").bounds().size.height
            })
        };
        let dragged = editor.size.height + px(60.);
        assert_eq!(height(cx), dragged);
        // A tab that was already open takes it too.
        window_action(handle, cx, |window, cx| window.click("tab-other", cx));
        assert_eq!(height(cx), dragged);
        window_action(handle, cx, |window, cx| window.click("tab-db", cx));
        assert_eq!(height(cx), dragged);
    }

    #[gpui_kit::test]
    fn deleting_a_connection_closes_its_tab(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        open_tab(handle, &workspace, "db", cx);
        open_tab(handle, &workspace, "other", cx);
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.right_click("conn-other", cx)
        });
        // Disconnect, Edit and Delete, as it has a tab, with a line between each.
        window_action(handle, cx, |window, cx| window.click(4usize, cx));
        window_action(handle, cx, |window, cx| window.click("ok", cx));
        assert_eq!(tab_names(&workspace, cx), ["db"]);
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("db"));
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("tab-other").is_none());
            assert!(window.try_find("conn-other").is_none());
        });
    }

    #[gpui_kit::test]
    fn a_change_made_while_a_connection_saves_is_kept(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        let form = edit_form(handle, &workspace, "other", cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let mut moved = this.config.find("other").unwrap().clone();
                moved.folder = "Staging".into();
                // Taken at Save; the keychain may then keep it waiting.
                let mut saved = this.config.clone();
                saved.upsert(moved.clone(), Some("other")).unwrap();
                this.change_font_size(|config| config.resize_font(1), cx);
                let previous = Some("other".into());
                let saving = super::Saving {
                    form,
                    connection: moved,
                    previous,
                };
                this.finish_saving_connection(saving, saved, Ok(None), window, cx);
                assert_eq!(this.config.font_size, super::DEFAULT_FONT_SIZE + 1);
                assert_eq!(this.config.find("other").unwrap().folder, "Staging");
                // Neither save written so far holds both changes: one more does.
                let newest = this.config_saves.latest().unwrap();
                let written = newest.run(|config| Ok(Some(config.clone()))).unwrap();
                assert_eq!(written.as_ref(), Some(&this.config));
            });
        });
    }

    /// A save that failed was undone in the keychain: quitting must not write it after all.
    #[gpui_kit::test]
    fn quitting_after_a_failed_save_or_delete_writes_the_config_as_it_is(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_two_connections(), cx);
        let form = edit_form(handle, &workspace, "other", cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let before = this.config.clone();
                let mut renamed = this.config.find("other").unwrap().clone();
                renamed.name = "renamed".into();
                // Taken at Save, as `save_connection` does, then never written.
                let mut saved = this.config.clone();
                saved.upsert(renamed.clone(), Some("other")).unwrap();
                this.config_saves.save(saved.clone());
                let failed = Err(anyhow::anyhow!("the keychain refused"));
                let previous = Some("other".into());
                let saving = super::Saving {
                    form,
                    connection: renamed,
                    previous,
                };
                this.finish_saving_connection(saving, saved, failed, window, cx);
                let newest = this.final_saves().1.expect("a config save was made");
                let written = newest.run(|config| Ok(Some(config.clone()))).unwrap();
                assert_eq!(written.as_ref(), Some(&before));

                // The same after a delete that failed.
                let removed = this.config.find("other").cloned();
                this.config.remove("other");
                this.config_saves.save(this.config.clone());
                let failed = Err(anyhow::anyhow!("the keychain refused"));
                this.finish_deleting_connection(removed, failed, cx);
                assert!(this.config.find("other").is_some(), "shown again");
                let newest = this.final_saves().1.expect("a config save was made");
                let written = newest.run(|config| Ok(Some(config.clone()))).unwrap();
                assert!(written.unwrap().find("other").is_some());
            });
        });
    }

    #[gpui_kit::test]
    fn quitting_writes_what_still_waits_to_be_saved(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 42", cx);
        });
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                this.change_font_size(|config| config.resize_font(1), cx);
                let (session, config) = this.final_saves();
                let session = session.expect("the draft waits for its delayed save");
                let draft = session.run(|s| Ok(s.drafts.get("db").cloned())).unwrap();
                assert!(draft.unwrap().contains("select 42"));
                let config = config.expect("the text size is saved");
                let size = config.run(|c| Ok(Some(c.font_size))).unwrap();
                assert_eq!(size, Some(super::DEFAULT_FONT_SIZE + 1));
                // Written once: nothing of the session waits now.
                assert!(this.final_saves().0.is_none());
            });
        });
    }

    #[gpui_kit::test]
    fn the_app_quitting_takes_the_session_save_still_waiting(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        open_tab(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            window.click("sql-editor", cx);
            window.input("select 42", cx);
        });
        assert!(workspace.read_with(cx, |this, _| this.session_unsaved));
        cx.quit();
        assert!(workspace.read_with(cx, |this, _| !this.session_unsaved));
    }

    #[gpui_kit::test]
    fn double_clicking_the_open_connection_again_leaves_a_running_query(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_connection(), cx);
        let tab = open_tab(handle, &workspace, "db", cx);
        tab.update(cx, |tab, cx| tab.run_sql("select 1".into(), cx));
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| window.double_click("conn-db", cx));
        cx.update(|cx| {
            let status = &tab.read(cx).status;
            assert!(matches!(status, Status::Busy(s) if s.as_ref() == "Running…"));
        });
    }

    /// Opens the window as a launch does after the tabs `open` were left open, with
    /// `active` on screen. "other" has a draft.
    fn restore(
        config: Config,
        open_tabs: &[&str],
        active: Option<&str>,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>) {
        let (handle, workspace) = open(config, cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                this.session.set_draft("other", "select 'other'");
                let open_tabs = open_tabs.iter().map(|name| name.to_string()).collect();
                this.session.set_tabs(open_tabs, active.map(str::to_owned));
                this.restore_tabs(window, cx);
            })
        });
        (handle, workspace)
    }

    /// The open tabs and the tab on screen, as the session will save them.
    fn saved_tabs(
        workspace: &Entity<Workspace>,
        cx: &mut TestAppContext,
    ) -> (Vec<String>, Option<String>) {
        workspace.read_with(cx, |this, _| {
            (
                this.session.open_tabs.clone(),
                this.session.active_tab.clone(),
            )
        })
    }

    /// Whether `name`'s tab has asked for a pool: connected, or loaded its schema.
    fn started(workspace: &Entity<Workspace>, name: &str, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            let tab = workspace
                .read(cx)
                .tab(name)
                .expect("the tab is open")
                .read(cx);
            let loading = !matches!(tab.schema.read(cx).load, crate::schema::Load::Idle);
            assert_eq!(tab.started, loading, "{name}");
            assert_eq!(tab.started, tab.connecting, "{name}");
            tab.started
        })
    }

    #[gpui_kit::test]
    fn tabs_come_back_in_order_without_a_deleted_one_and_only_the_shown_one_connects(
        cx: &mut TestAppContext,
    ) {
        let (handle, workspace) = restore(
            config_with_three_connections(),
            &["third", "gone", "db", "other"],
            Some("db"),
            cx,
        );
        assert_eq!(tab_names(&workspace, cx), ["third", "db", "other"]);
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("db"));
        assert!(started(&workspace, "db", cx));
        assert!(!started(&workspace, "third", cx));
        assert!(!started(&workspace, "other", cx));
        cx.update(|cx| {
            let this = workspace.read(cx);
            let other = this.tab("other").unwrap().read(cx);
            assert_eq!(other.editor.read(cx).value().as_str(), "select 'other'");
            assert_eq!(other.light(), Light::Off);
            // Nothing changed yet, so what was remembered stays as it was.
            assert!(!this.session_unsaved);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("tab-third").visible());
            assert!(window.try_find("tab-gone").is_none());
        });
    }

    #[gpui_kit::test]
    fn a_restored_tab_connects_when_first_shown_and_is_remembered(cx: &mut TestAppContext) {
        let (handle, workspace) = restore(
            config_with_three_connections(),
            &["db", "gone", "other"],
            Some("db"),
            cx,
        );
        window_action(handle, cx, |window, cx| window.click("tab-other", cx));
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("other"));
        assert!(started(&workspace, "other", cx));
        assert_eq!(
            saved_tabs(&workspace, cx),
            (vec!["db".into(), "other".into()], Some("other".into()))
        );
        assert!(workspace.read_with(cx, |this, _| this.session_unsaved));
    }

    #[gpui_kit::test]
    fn opening_a_restored_tab_from_the_dialog_connects_it(cx: &mut TestAppContext) {
        let (handle, workspace) = restore(
            config_with_two_connections(),
            &["db", "other"],
            Some("db"),
            cx,
        );
        open_picker(handle, cx);
        window_action(handle, cx, |window, cx| {
            window.double_click("conn-other", cx)
        });
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("other"));
        assert!(started(&workspace, "other", cx));
    }

    #[gpui_kit::test]
    fn running_in_a_restored_tab_connects_it(cx: &mut TestAppContext) {
        let (_, workspace) = restore(
            config_with_two_connections(),
            &["db", "other"],
            Some("db"),
            cx,
        );
        let other = cx.update(|cx| workspace.read(cx).tab("other").unwrap().clone());
        other.update(cx, |tab, cx| tab.run_sql("select 1".into(), cx));
        cx.update(|cx| {
            let tab = other.read(cx);
            assert!(tab.started);
            assert!(!matches!(
                tab.schema.read(cx).load,
                crate::schema::Load::Idle
            ));
            assert!(matches!(&tab.status, Status::Busy(s) if s.as_ref() == "Running…"));
        });
    }

    #[gpui_kit::test]
    fn without_a_remembered_tab_on_screen_the_first_shows(cx: &mut TestAppContext) {
        let (_, workspace) = restore(config_with_two_connections(), &["other", "db"], None, cx);
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("other"));
        assert!(started(&workspace, "other", cx));
        assert!(!started(&workspace, "db", cx));
        let (_, workspace) = restore(config_with_two_connections(), &["other"], Some("gone"), cx);
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("other"));
    }

    #[gpui_kit::test]
    fn with_nothing_remembered_no_tab_opens(cx: &mut TestAppContext) {
        let (handle, workspace) = restore(config_with_two_connections(), &[], None, cx);
        assert!(tab_names(&workspace, cx).is_empty());
        assert_eq!(active_name(&workspace, cx), None);
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("tab-db").is_none());
        });
    }

    #[gpui_kit::test]
    fn opening_switching_and_closing_tabs_is_remembered(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_three_connections(), cx);
        open_tab(handle, &workspace, "db", cx);
        open_tab(handle, &workspace, "other", cx);
        open_tab(handle, &workspace, "third", cx);
        assert_eq!(
            saved_tabs(&workspace, cx),
            (
                vec!["db".into(), "other".into(), "third".into()],
                Some("third".into())
            )
        );
        window_action(handle, cx, |window, cx| window.click("tab-other", cx));
        assert_eq!(saved_tabs(&workspace, cx).1.as_deref(), Some("other"));
        window_action(handle, cx, |window, cx| window.click("tab-close-other", cx));
        assert_eq!(
            saved_tabs(&workspace, cx),
            (vec!["db".into(), "third".into()], Some("third".into()))
        );
        window_action(handle, cx, |window, cx| window.click("tab-close-db", cx));
        window_action(handle, cx, |window, cx| window.click("tab-close-third", cx));
        assert_eq!(saved_tabs(&workspace, cx), (Vec::new(), None));
    }

    #[gpui_kit::test]
    fn closing_the_shown_tab_connects_the_restored_one_that_shows_next(cx: &mut TestAppContext) {
        let (handle, workspace) = restore(
            config_with_two_connections(),
            &["db", "other"],
            Some("db"),
            cx,
        );
        window_action(handle, cx, |window, cx| window.click("tab-close-db", cx));
        assert_eq!(active_name(&workspace, cx).as_deref(), Some("other"));
        assert!(started(&workspace, "other", cx));
        assert_eq!(
            saved_tabs(&workspace, cx),
            (vec!["other".into()], Some("other".into()))
        );
    }

    #[gpui_kit::test]
    fn renaming_and_deleting_a_connection_update_the_remembered_tabs(cx: &mut TestAppContext) {
        let (handle, workspace) = open(config_with_three_connections(), cx);
        open_tab(handle, &workspace, "db", cx);
        open_tab(handle, &workspace, "other", cx);
        let form = edit_form(handle, &workspace, "db", cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                let mut renamed = this.config.find("db").unwrap().clone();
                renamed.name = "renamed".into();
                let mut saved = this.config.clone();
                saved.upsert(renamed.clone(), Some("db")).unwrap();
                let saving = super::Saving {
                    form,
                    connection: renamed,
                    previous: Some("db".into()),
                };
                this.finish_saving_connection(saving, saved, Ok(None), window, cx);
            });
        });
        // Saving the edit shows the edited connection.
        assert_eq!(
            saved_tabs(&workspace, cx),
            (
                vec!["renamed".into(), "other".into()],
                Some("renamed".into())
            )
        );
        workspace.update(cx, |this, cx| this.delete_connection("renamed".into(), cx));
        assert_eq!(
            saved_tabs(&workspace, cx),
            (vec!["other".into()], Some("other".into()))
        );
    }
}
