//! Add / edit connection form. Collects values; the workspace persists them.
//!
//! A new connection is typed in or found in AWS, and a switch at the top moves between the
//! two without losing what either holds. The buttons sit in a footer outside the
//! scrolling part, so they are always in view.

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::cloud::Discovered;
use tuplebase_core::config::{
    self, Adapter, Auth, AuthKind, AwsIamAuth, AwsSecretRef, Connection, Family, MongoSettings,
    Source, SshConfig, TlsMode,
};
use tuplebase_core::ssh;
use tuplebase_core::state::CloudSearch;

use crate::aws_picker::{AwsPicker, AwsPickerEvent};
use crate::conn_source::{ConnSource, ConnSourceEvent};
use crate::logos;
use crate::runtime::io;

/// Icon color choices. The first, empty one means the database's own color.
const COLORS: [&str; 9] = [
    "", "#E5484D", "#F76B15", "#E2A336", "#46A758", "#2EA69A", "#3E8BD6", "#8E6FD8", "#D66BA0",
];

// Short-lived event, so boxing the larger variant would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum ConnFormEvent {
    Save {
        connection: Connection,
        /// `None` when left blank: keeps any stored password.
        password: Option<String>,
        /// SSH password or key passphrase; `None` when left blank.
        ssh_password: Option<String>,
        /// Name before editing; `None` for a new connection.
        previous: Option<String>,
    },
    Cancel,
    /// Remember where a search for databases in AWS looked.
    SearchedAws(CloudSearch),
}

/// How a new connection is being filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Typed in.
    Manual,
    /// Found in AWS first.
    Aws,
}

pub struct ConnForm {
    /// The step before the form for a new connection: where the database is.
    start: Option<Entity<ConnSource>>,
    _start_subscription: Option<Subscription>,
    mode: Mode,
    /// Where the last search in AWS looked, for the picker.
    last_aws: CloudSearch,
    /// Finding the database in AWS, kept once opened so switching back finds it as left.
    picker: Option<Entity<AwsPicker>>,
    _picker_subscriptions: Vec<Subscription>,
    /// The database found in AWS that filled in the form, shown above it.
    found: Option<Box<Discovered>>,
    form_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
    previous: Option<String>,
    adapter: Adapter,
    /// A new connection asks for the database first and shows the rest once it is chosen.
    chosen: bool,
    /// Once chosen, the engine shows in one line; this opens the list to change it.
    picking: bool,
    /// Narrows the list of engines by name or family.
    engine_search: Entity<InputState>,
    pub(crate) folder: String,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    /// Where the password comes from.
    auth_kind: AuthKind,
    /// Where the connection was discovered; kept as it is.
    source: Option<Source>,
    /// The host it was found at; `source` is dropped if the host changes.
    source_host: String,
    aws_profile: Entity<InputState>,
    aws_region: Entity<InputState>,
    aws_name: Entity<InputState>,
    use_secret_host: bool,
    /// MongoDB and DocumentDB: find the servers through DNS (`mongodb+srv://`).
    mongo_srv: bool,
    /// MongoDB and DocumentDB: connection string options, never a password.
    mongo_options: Entity<InputState>,
    /// What filling in the form from a pasted connection string left out, if anything.
    paste_note: Option<SharedString>,
    readonly: bool,
    pub(crate) color: String,
    tls: TlsMode,
    use_ssh: bool,
    ssh_host: Entity<InputState>,
    ssh_port: Entity<InputState>,
    ssh_user: Entity<InputState>,
    ssh_key: Entity<InputState>,
    ssh_password: Entity<InputState>,
    ssh_fingerprint: Entity<InputState>,
    fetching_fingerprint: bool,
    pub(crate) error: Option<SharedString>,
}

impl EventEmitter<ConnFormEvent> for ConnForm {}

impl ConnForm {
    /// `existing` pre-fills the form for editing; `folder` is where it gets saved.
    /// `last_aws` is where the last search for databases in AWS looked.
    pub fn new(
        existing: Option<&Connection>,
        folder: String,
        last_aws: CloudSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut input = |placeholder: &str, value: String| {
            let placeholder = placeholder.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let field = |f: fn(&Connection) -> String| existing.map(f).unwrap_or_default();
        let name = input("orders-db", field(|c| c.name.clone()));
        let host = input("localhost", field(|c| c.host.clone()));
        let port = input("Default", field(|c| c.port.to_string()));
        let engine_search = input("Search engines", String::new());
        let database = input("Optional", field(|c| c.database.clone()));
        let adapter = existing.map_or(Adapter::Postgres, |c| c.adapter);
        let user = input(adapter.user_hint(), field(|c| c.user.clone()));
        let password = input(
            if existing.is_some() {
                "Leave blank to keep the current password"
            } else {
                "Optional"
            },
            String::new(),
        );
        let auth = existing.map(|c| c.auth.clone()).unwrap_or_default();
        let (profile, region) = auth.aws_profile().unwrap_or_default();
        let secret = auth.secret().map(|(_, secret)| secret);
        let aws_profile = input("Default credentials", profile.to_owned());
        let aws_region = input("The profile's region", region.to_owned());
        let aws_name = input(
            "prod/orders-db",
            secret.map(|s| s.name.clone()).unwrap_or_default(),
        );
        let use_secret_host = secret.is_some_and(|s| s.use_secret_host);
        let mongo = existing.and_then(|c| c.mongo.clone()).unwrap_or_default();
        let mongo_options = input("authSource=admin&replicaSet=rs0", mongo.options);
        let ssh = existing.and_then(|c| c.ssh.as_ref());
        let ssh_field = |f: fn(&SshConfig) -> String| ssh.map(f).unwrap_or_default();
        let ssh_host = input("bastion.example.com", ssh_field(|s| s.host.clone()));
        let ssh_port = input("22", ssh_field(|s| s.port.to_string()));
        let ssh_user = input("ubuntu", ssh_field(|s| s.user.clone()));
        let ssh_key = input("~/.ssh/id_ed25519", ssh_field(|s| s.key_path.clone()));
        let ssh_password = input(
            if ssh.is_some() {
                "Leave blank to keep the current one"
            } else {
                "Optional if the key has no passphrase"
            },
            String::new(),
        );
        let ssh_fingerprint = input("SHA256:…", ssh_field(|s| s.fingerprint.clone()));
        // A new connection first asks where the database is.
        let (start, start_subscription) = if existing.is_none() {
            let start = cx.new(|_| ConnSource);
            let subscription = cx.subscribe_in(&start, window, Self::on_start_event);
            (Some(start), Some(subscription))
        } else {
            (None, None)
        };
        // The host decides whether Redshift Serverless names the user.
        let subscriptions = vec![
            cx.subscribe(&host, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            // Enter picks the only engine left, or the first.
            cx.subscribe_in(
                &engine_search,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => cx.notify(),
                    InputEvent::PressEnter { .. } => {
                        if let Some(&adapter) = this.matching_engines(cx).first() {
                            this.pick_engine(adapter, window, cx);
                        }
                    }
                    _ => {}
                },
            ),
        ];
        Self {
            start,
            _start_subscription: start_subscription,
            mode: Mode::Manual,
            last_aws,
            picker: None,
            _picker_subscriptions: Vec::new(),
            found: None,
            form_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
            previous: existing.map(|c| c.name.clone()),
            adapter,
            chosen: existing.is_some(),
            picking: false,
            engine_search,
            folder,
            name,
            host,
            port,
            database,
            user,
            password,
            auth_kind: auth.kind(),
            source: existing.and_then(|c| c.source.clone()),
            source_host: existing.map(|c| c.host.clone()).unwrap_or_default(),
            aws_profile,
            aws_region,
            aws_name,
            use_secret_host,
            mongo_srv: mongo.srv,
            mongo_options,
            paste_note: None,
            readonly: existing.is_some_and(|c| c.readonly),
            color: existing.map(|c| c.color.clone()).unwrap_or_default(),
            tls: existing.map_or(TlsMode::default(), |c| c.tls),
            use_ssh: ssh.is_some(),
            ssh_host,
            ssh_port,
            ssh_user,
            ssh_key,
            ssh_password,
            ssh_fingerprint,
            fetching_fingerprint: false,
            error: None,
        }
    }

    fn on_start_event(
        &mut self,
        _: &Entity<ConnSource>,
        event: &ConnSourceEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start = None;
        self._start_subscription = None;
        match event {
            ConnSourceEvent::Manual => self.show_manual(cx),
            ConnSourceEvent::Aws => self.show_aws(window, cx),
        }
    }

    fn show_manual(&mut self, cx: &mut Context<Self>) {
        self.mode = Mode::Manual;
        cx.notify();
    }

    /// Shows the search in AWS, as it was left.
    fn show_aws(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::Aws;
        let picker = match &self.picker {
            Some(picker) => picker.clone(),
            None => {
                let last = self.last_aws.clone();
                let picker = cx.new(|cx| AwsPicker::new(last, window, cx));
                self._picker_subscriptions = vec![
                    cx.subscribe_in(&picker, window, |this, _, event, window, cx| match event {
                        AwsPickerEvent::Use(found) => this.use_found(found, window, cx),
                        AwsPickerEvent::Searched(search) => {
                            this.last_aws = search.clone();
                            cx.emit(ConnFormEvent::SearchedAws(search.clone()));
                        }
                    }),
                    // The footer's button follows the picker's choice.
                    cx.observe(&picker, |_, _, cx| cx.notify()),
                ];
                self.picker = Some(picker.clone());
                picker
            }
        };
        picker.update(cx, |picker, cx| picker.focus(window, cx));
        cx.notify();
    }

    /// Fills in the form with a database found in AWS and shows it from the top, with
    /// the keys where something is still missing.
    fn use_found(&mut self, found: &Discovered, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = found.connection() else {
            return;
        };
        self.prefill(&connection, window, cx);
        self.found = Some(Box::new(found.clone()));
        self.mode = Mode::Manual;
        self.error = None;
        self.form_scroll.set_offset(point(px(0.), px(0.)));
        let next = if connection.auth.is_password() {
            &self.password
        } else {
            &self.name
        };
        next.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// The search in AWS, once opened.
    #[cfg(test)]
    pub fn aws_picker(&self) -> Option<&Entity<AwsPicker>> {
        self.picker.as_ref()
    }

    /// Fills the form with a connection that was found, to check and save as new.
    pub fn prefill(&mut self, conn: &Connection, window: &mut Window, cx: &mut Context<Self>) {
        let mut set = |input: &Entity<InputState>, value: String| {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.name, conn.name.clone());
        set(&self.host, conn.host.clone());
        set(&self.port, conn.port.to_string());
        set(&self.database, conn.database.clone());
        set(&self.user, conn.user.clone());
        let (profile, region) = conn.auth.aws_profile().unwrap_or_default();
        set(&self.aws_profile, profile.to_owned());
        set(&self.aws_region, region.to_owned());
        let secret = conn.auth.secret().map(|(_, secret)| secret);
        set(
            &self.aws_name,
            secret.map(|s| s.name.clone()).unwrap_or_default(),
        );
        self.use_secret_host = secret.is_some_and(|s| s.use_secret_host);
        let mongo = conn.mongo.clone().unwrap_or_default();
        set(&self.mongo_options, mongo.options);
        self.mongo_srv = mongo.srv;
        self.set_adapter(conn.adapter, window, cx);
        self.chosen = true;
        self.auth_kind = conn.auth.kind();
        self.tls = conn.tls;
        self.source = conn.source.clone();
        self.source_host = conn.host.clone();
        cx.notify();
    }

    /// Whether the secret names the host, so the Host and Port fields are hidden.
    fn host_from_secret(&self) -> bool {
        self.uses_secret() && self.use_secret_host && !self.adapter.is_file()
    }

    /// Whether AWS names the database user from the IAM identity (Redshift Serverless),
    /// so the User field is not used.
    fn iam_names_the_user(&self, cx: &App) -> bool {
        self.adapter == Adapter::Redshift
            && self.auth_kind == AuthKind::AwsIam
            && config::is_redshift_serverless_host(&self.host.read(cx).value())
    }

    /// Whether the credentials come from a secret or parameter in AWS.
    fn uses_secret(&self) -> bool {
        matches!(self.auth_kind, AuthKind::AwsSecret | AuthKind::AwsParameter)
    }

    /// The credentials as chosen, from the visible fields only.
    fn auth(&self, cx: &App) -> Auth {
        let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let secret = || AwsSecretRef {
            profile: text(&self.aws_profile),
            region: text(&self.aws_region),
            name: text(&self.aws_name),
            use_secret_host: self.use_secret_host,
        };
        match self.auth_kind {
            _ if self.adapter.is_file() => Auth::Password,
            AuthKind::Password => Auth::Password,
            AuthKind::AwsSecret => Auth::AwsSecret(secret()),
            AuthKind::AwsParameter => Auth::AwsParameter(secret()),
            AuthKind::AwsIam => Auth::AwsIam(AwsIamAuth {
                profile: text(&self.aws_profile),
                region: text(&self.aws_region),
            }),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = |input: &Entity<InputState>, cx: &App| input.read(cx).value().trim().to_string();
        // Hidden fields are skipped, so what was typed in them can't block saving.
        let visible = |input: &Entity<InputState>, shown: bool, cx: &App| {
            if shown {
                text(input, cx)
            } else {
                String::new()
            }
        };
        let host_shown = !self.host_from_secret();
        let port_text = visible(&self.port, host_shown, cx);
        let port = if port_text.is_empty() {
            self.adapter.default_port()
        } else {
            match port_text.parse() {
                Ok(port) => port,
                Err(_) => {
                    self.error = Some("Port must be a number.".into());
                    cx.notify();
                    return;
                }
            }
        };
        let ssh = if self.use_ssh && !self.adapter.is_file() {
            let Some(port) = self.ssh_port_value(cx) else {
                self.error = Some("SSH port must be a number.".into());
                cx.notify();
                return;
            };
            Some(SshConfig {
                host: text(&self.ssh_host, cx),
                port,
                user: text(&self.ssh_user, cx),
                key_path: text(&self.ssh_key, cx),
                fingerprint: text(&self.ssh_fingerprint, cx),
            })
        } else {
            None
        };
        let uses_ssh = ssh.is_some();
        let auth = self.auth(cx);
        // Secrets are kept exactly as typed: spaces can be part of a password.
        let secret = |input: &Entity<InputState>, cx: &App| input.read(cx).value().to_string();
        let password = if auth.is_password() {
            secret(&self.password, cx)
        } else {
            String::new()
        };
        let ssh_password = secret(&self.ssh_password, cx);
        cx.emit(ConnFormEvent::Save {
            connection: Connection {
                name: text(&self.name, cx),
                folder: self.folder.clone(),
                adapter: self.adapter,
                host: visible(&self.host, host_shown, cx),
                port,
                database: text(&self.database, cx),
                user: visible(&self.user, !self.iam_names_the_user(cx), cx),
                readonly: self.readonly,
                color: self.color.clone(),
                tls: self.tls,
                ssh,
                auth,
                // Where it was found no longer applies to another server, such as one a
                // secret names.
                source: self.source.clone().filter(|_| {
                    !self.host_from_secret() && text(&self.host, cx) == self.source_host
                }),
                mongo: self.mongo_settings(cx),
            },
            password: (!password.is_empty()).then_some(password),
            ssh_password: (uses_ssh && !ssh_password.is_empty()).then_some(ssh_password),
            previous: self.previous.clone(),
        });
    }

    /// Fills in the server and sign-in fields from the connection string on the clipboard.
    /// The string itself is kept nowhere: its password goes to the password field, saved
    /// to the credential store like a typed one, and the rest to plain fields (S1).
    fn paste_connection_string(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        self.fill_from_connection_string(&text, window, cx);
    }

    pub(crate) fn fill_from_connection_string(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let uri = match config::parse_mongo_uri(text) {
            Ok(uri) => uri,
            Err(e) => {
                // The error never quotes the string, which may hold a password.
                self.error = Some(format!("Couldn't read the connection string: {e:#}").into());
                self.paste_note = None;
                cx.notify();
                return;
            }
        };
        let mut set = |input: &Entity<InputState>, value: String| {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.host, uri.host);
        set(
            &self.port,
            uri.port.map(|p| p.to_string()).unwrap_or_default(),
        );
        set(&self.database, uri.database);
        set(&self.user, uri.user);
        set(&self.mongo_options, uri.options);
        if let Some(password) = uri.password {
            set(&self.password, password);
            // Only a password sign-in saves what the password field holds.
            self.auth_kind = AuthKind::Password;
        }
        self.mongo_srv = uri.srv;
        let mut notes = Vec::new();
        if uri.other_hosts > 0 {
            notes.push(format!(
                "Kept the first of {} hosts. With replicaSet in the options, the driver \
                 finds the others from it.",
                uri.other_hosts + 1
            ));
        }
        if !uri.dropped.is_empty() {
            notes.push(format!(
                "Left out {}: options can't hold a secret.",
                uri.dropped.join(", ")
            ));
        }
        self.paste_note = (!notes.is_empty()).then(|| notes.join(" ").into());
        self.error = None;
        cx.notify();
    }

    /// What a MongoDB or DocumentDB connection saves besides the common fields; `None`
    /// for other engines, or when nothing is set.
    fn mongo_settings(&self, cx: &App) -> Option<MongoSettings> {
        let settings = MongoSettings {
            srv: self.mongo_srv,
            options: self.mongo_options.read(cx).value().trim().to_string(),
        };
        (self.adapter.family() == Family::Document && settings != MongoSettings::default())
            .then_some(settings)
    }

    /// The SSH port, defaulting to 22; `None` if it isn't a number.
    fn ssh_port_value(&self, cx: &App) -> Option<u16> {
        let value = self.ssh_port.read(cx).value();
        let value = value.trim();
        if value.is_empty() {
            Some(22)
        } else {
            value.parse().ok()
        }
    }

    /// Reads the server's key so the user can compare it before trusting it.
    fn fetch_fingerprint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.ssh_host.read(cx).value().trim().to_string();
        let port = self.ssh_port_value(cx);
        let (false, Some(port)) = (host.is_empty(), port) else {
            self.error = Some("Enter the SSH host and port first.".into());
            cx.notify();
            return;
        };
        self.error = None;
        self.fetching_fingerprint = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = io(async move { ssh::fetch_fingerprint(&host, port).await }).await;
            this.update_in(cx, |this, window, cx| {
                this.fetching_fingerprint = false;
                match result {
                    Ok(fingerprint) => this.ssh_fingerprint.update(cx, |input, cx| {
                        input.set_value(fingerprint, window, cx);
                    }),
                    Err(e) => this.error = Some(format!("{e:#}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Shown under the form, e.g. when the workspace rejects the values.
    pub fn set_error(&mut self, error: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = Some(error.into());
        cx.notify();
    }

    /// The engine, and the user its servers usually have as the User field's example.
    fn set_adapter(&mut self, adapter: Adapter, window: &mut Window, cx: &mut Context<Self>) {
        // DocumentDB accepts only encrypted connections unless its cluster was changed.
        // Required, not Verified, until Amazon's authorities are given to the driver.
        if adapter == Adapter::Documentdb && self.adapter != adapter && self.tls == TlsMode::Prefer
        {
            self.tls = TlsMode::Require;
        }
        self.adapter = adapter;
        // A way to sign in the new engine doesn't take goes back to a password.
        if !adapter.auth_kinds().contains(&self.auth_kind) {
            self.auth_kind = AuthKind::Password;
        }
        self.user.update(cx, |input, cx| {
            input.set_placeholder(adapter.user_hint(), window, cx);
        });
    }

    /// The engines whose name or family holds what the search box holds.
    fn matching_engines(&self, cx: &App) -> Vec<Adapter> {
        let search = self.engine_search.read(cx).value().trim().to_lowercase();
        Adapter::ALL
            .into_iter()
            .filter(|adapter| {
                search.is_empty()
                    || adapter.label().to_lowercase().contains(&search)
                    || family_label(adapter.family())
                        .to_lowercase()
                        .contains(&search)
            })
            .collect()
    }

    fn pick_engine(&mut self, adapter: Adapter, window: &mut Window, cx: &mut Context<Self>) {
        self.set_adapter(adapter, window, cx);
        self.picking = false;
        // The first choice reveals the rest of the form, so move on to it.
        if !self.chosen {
            self.chosen = true;
            let first = if adapter.is_file() {
                &self.database
            } else {
                &self.host
            };
            first.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// One engine in the list: its logo and whole name, never cut short; a narrow form
    /// wraps the list instead.
    fn render_adapter(&self, adapter: Adapter, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let selected = self.chosen && self.adapter == adapter;
        let border = if selected {
            theme.foreground
        } else {
            theme.input
        };
        h_flex()
            .id(SharedString::from(format!("adapter-{}", adapter.label())))
            .test_support()
            .flex_none()
            .min_w(px(104.))
            .gap_2()
            .px_3()
            .py_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .child(logos::icon(adapter, 18.))
            .child(div().whitespace_nowrap().text_sm().child(adapter.label()))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.pick_engine(adapter, window, cx);
            }))
    }

    /// Every engine, grouped by family, with a search box above.
    fn render_engine_list(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let matching = self.matching_engines(cx);
        let groups = [Family::Sql, Family::Document, Family::KeyValue]
            .into_iter()
            .filter_map(|family| {
                let engines: Vec<Adapter> = matching
                    .iter()
                    .copied()
                    .filter(|a| a.family() == family)
                    .collect();
                (!engines.is_empty()).then(|| {
                    v_flex()
                        .id(SharedString::from(format!(
                            "engines-{}",
                            family_label(family)
                        )))
                        .test_support()
                        .gap_1p5()
                        .child(
                            div()
                                .text_xs()
                                .font_medium()
                                .text_color(muted)
                                .child(family_label(family)),
                        )
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .children(engines.into_iter().map(|a| self.render_adapter(a, cx))),
                        )
                })
            })
            .collect::<Vec<_>>();
        let none = groups.is_empty();
        v_flex()
            .gap_3()
            .child(
                Input::new(&self.engine_search)
                    .id("engine-search")
                    .prefix(Icon::new(IconName::Search).small()),
            )
            .children(groups)
            .when(none, |el| {
                el.child(
                    div()
                        .id("engines-none")
                        .test_support()
                        .text_sm()
                        .text_color(muted)
                        .child("No engine matches the search."),
                )
            })
    }

    /// The chosen engine in one line, with a way to change it.
    fn render_engine_current(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let adapter = self.adapter;
        h_flex()
            .id("engine-current")
            .test_support()
            .gap_2()
            .px_3()
            .py_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.input)
            .cursor_pointer()
            .child(logos::icon(adapter, 18.))
            .child(div().whitespace_nowrap().text_sm().child(adapter.label()))
            .child(
                div()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(family_label(adapter.family())),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Change")
                    .child(Icon::new(IconName::ChevronDown).small()),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.picking = true;
                this.engine_search
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }))
    }

    /// The name of the saved connection being edited, if any.
    pub fn editing(&self) -> Option<&str> {
        self.previous.as_deref()
    }

    fn render_swatch(
        &self,
        index: usize,
        value: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let selected = self.color == value;
        let ring = cx.theme().foreground;
        let fill = logos::color_of(self.adapter, value);
        div()
            .id(SharedString::from(format!("color-{index}")))
            .test_support()
            .size(px(24.))
            .p(px(2.))
            .rounded_full()
            .border_2()
            .when(selected, |d| d.border_color(ring))
            .when(!selected, |d| d.border_color(transparent_black()))
            .cursor_pointer()
            .child(div().size_full().rounded_full().bg(fill))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.color = value.to_owned();
                cx.notify();
            }))
    }

    fn render_tls(&self, mode: TlsMode, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        choice("tls", mode.label(), self.tls == mode, cx).on_click(cx.listener(
            move |this, _, _, cx| {
                this.tls = mode;
                cx.notify();
            },
        ))
    }

    fn render_auth(&self, kind: AuthKind, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        choice("auth", kind.label(), self.auth_kind == kind, cx).on_click(cx.listener(
            move |this, _, _, cx| {
                this.choose_auth(kind);
                cx.notify();
            },
        ))
    }

    fn choose_auth(&mut self, kind: AuthKind) {
        self.auth_kind = kind;
        // RDS takes an IAM token only over an encrypted connection.
        if kind == AuthKind::AwsIam && self.tls == TlsMode::Prefer {
            self.tls = TlsMode::Require;
        }
    }

    fn render_aws(&self, kind: AuthKind, cx: &mut Context<Self>) -> impl IntoElement {
        let profile = h_flex()
            .gap_3()
            .child(field(
                "AWS profile",
                Input::new(&self.aws_profile).id("conn-aws-profile"),
                cx,
            ))
            .child(field(
                "Region",
                Input::new(&self.aws_region).id("conn-aws-region"),
                cx,
            ));
        let column = v_flex()
            .gap_3()
            .pl_4()
            .border_l_2()
            .border_color(cx.theme().border)
            .child(profile);
        let name_label = match kind {
            AuthKind::AwsSecret => "Secret name or ARN",
            AuthKind::AwsParameter => "Parameter name",
            AuthKind::Password | AuthKind::AwsIam => {
                let text = if self.iam_names_the_user(cx) {
                    "Each time you connect, AWS signs you in to the workgroup as your IAM \
                     identity (IAM:<user> or IAMR:<role>), so no database user is needed. \
                     Your identity needs redshift-serverless:GetCredentials on it."
                } else if self.adapter == Adapter::Redshift {
                    "Each time you connect, AWS gives a temporary password for the user \
                     below, which must already exist. Your identity needs \
                     redshift:GetClusterCredentials."
                } else {
                    "A token for the user below is made from your AWS profile each time you \
                     connect. The user needs rds-db:connect, and the database IAM \
                     authentication turned on."
                };
                return column.child(hint(text, cx).id("conn-iam-hint").test_support());
            }
        };
        column
            .child(
                field(
                    name_label,
                    Input::new(&self.aws_name).id("conn-aws-name"),
                    cx,
                )
                .child(hint(
                    "Read from AWS each time you connect and never saved here. JSON with \
                     \"username\" and \"password\", or just the password.",
                    cx,
                )),
            )
            .child(
                Checkbox::new("use-secret-host")
                    .cursor_pointer()
                    .label("Take the host, port and database from the secret too")
                    .checked(self.use_secret_host)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.use_secret_host = *checked;
                        cx.notify();
                    })),
            )
    }

    fn render_ssh(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .pl_4()
            .border_l_2()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_3()
                    .child(field(
                        "SSH host",
                        Input::new(&self.ssh_host).id("conn-ssh-host"),
                        cx,
                    ))
                    .child(div().w(px(120.)).child(field(
                        "SSH port",
                        Input::new(&self.ssh_port).id("conn-ssh-port"),
                        cx,
                    ))),
            )
            .child(field(
                "SSH user",
                Input::new(&self.ssh_user).id("conn-ssh-user"),
                cx,
            ))
            .child(field(
                "Private key file (leave blank to use a password)",
                Input::new(&self.ssh_key).id("conn-ssh-key"),
                cx,
            ))
            .child(field(
                "SSH password or key passphrase",
                Input::new(&self.ssh_password)
                    .id("conn-ssh-password")
                    .mask_toggle(),
                cx,
            ))
            .child(
                field(
                    "Server fingerprint",
                    h_flex()
                        .gap_2()
                        .child(
                            div().flex_1().child(
                                Input::new(&self.ssh_fingerprint).id("conn-ssh-fingerprint"),
                            ),
                        )
                        .child(
                            Button::new("fetch-fingerprint")
                                .cursor_pointer()
                                .label("Get from server")
                                .loading(self.fetching_fingerprint)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.fetch_fingerprint(window, cx);
                                })),
                        ),
                    cx,
                )
                .child(hint(
                    "Compare this with your server's key before saving. \
                     It proves you reached the right machine.",
                    cx,
                )),
            )
    }

    fn render_database(&self, cx: &mut Context<Self>) -> Div {
        // Until one is chosen, every engine is listed; then the chosen one shows in one
        // line until asked to change it.
        let list = !self.chosen || self.picking;
        v_flex()
            .gap_3()
            .when(!self.chosen, |el| {
                el.child(hint("Choose the database you want to connect to.", cx))
            })
            .map(|el| {
                if list {
                    el.child(self.render_engine_list(cx))
                } else {
                    el.child(self.render_engine_current(cx))
                }
            })
            .when(self.chosen, |el| el.child(self.render_name(cx)))
    }

    fn render_server(&self, cx: &mut Context<Self>) -> Div {
        let from_secret = self.host_from_secret();
        let document = self.adapter.family() == Family::Document;
        // An SRV address has no port: DNS gives each server's.
        let port_shown = !(document && self.mongo_srv);
        v_flex()
            .gap_3()
            .when(from_secret, |el| {
                el.child(hint("The host and port come from the secret.", cx))
            })
            .child(
                h_flex()
                    .gap_3()
                    .when(!from_secret, |el| {
                        el.child(field("Host", Input::new(&self.host).id("conn-host"), cx))
                            .when(port_shown, |el| {
                                el.child(div().w(px(90.)).child(field(
                                    "Port",
                                    Input::new(&self.port).id("conn-port"),
                                    cx,
                                )))
                            })
                    })
                    .child(field(
                        "Database",
                        Input::new(&self.database).id("conn-database"),
                        cx,
                    )),
            )
            .when(document, |el| {
                el.child(
                    h_flex()
                        .flex_wrap()
                        .gap_3()
                        .justify_between()
                        .child(
                            Checkbox::new("conn-mongo-srv")
                                .cursor_pointer()
                                .label("SRV address (mongodb+srv): DNS names the servers")
                                .checked(self.mongo_srv)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.mongo_srv = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("conn-mongo-paste")
                                .cursor_pointer()
                                .small()
                                .icon(IconName::Copy)
                                .label("Paste connection string")
                                .tooltip(
                                    "Fill in these fields from a mongodb:// or mongodb+srv:// \
                                     string on the clipboard. The string isn't saved; its \
                                     password goes to the password field.",
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.paste_connection_string(window, cx);
                                })),
                        ),
                )
                .when_some(self.paste_note.clone(), |el, note| {
                    el.child(
                        div()
                            .id("conn-paste-note")
                            .test_support()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(note),
                    )
                })
                .child(
                    field(
                        "Options",
                        Input::new(&self.mongo_options).id("conn-mongo-options"),
                        cx,
                    )
                    .child(hint(
                        "Connection string options, such as authSource or replicaSet. \
                         The password goes in its own field, never here.",
                        cx,
                    )),
                )
            })
    }

    fn render_credentials(&self, cx: &mut Context<Self>) -> Div {
        let kind = self.auth_kind;
        let user_shown = !self.iam_names_the_user(cx);
        v_flex()
            .gap_3()
            .child(
                h_flex().flex_wrap().gap_2().children(
                    self.adapter
                        .auth_kinds()
                        .iter()
                        .map(|&k| self.render_auth(k, cx)),
                ),
            )
            .when(kind != AuthKind::Password, |el| {
                el.child(self.render_aws(kind, cx))
            })
            .child(
                h_flex()
                    .gap_3()
                    .when(user_shown, |el| {
                        el.child(field("User", Input::new(&self.user).id("conn-user"), cx))
                    })
                    .when(kind == AuthKind::Password, |el| {
                        el.child(field(
                            "Password",
                            Input::new(&self.password).id("conn-password").mask_toggle(),
                            cx,
                        ))
                    }),
            )
    }

    fn render_security(&self, cx: &mut Context<Self>) -> Div {
        v_flex()
            .gap_3()
            .when(!self.adapter.is_file(), |el| {
                el.child(field(
                    "Encryption",
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .children(TlsMode::ALL.map(|mode| self.render_tls(mode, cx))),
                    cx,
                ))
                .when(self.source.is_some(), |el| {
                    el.child(
                        hint(
                            "Verified checks the certificate with Amazon's RDS authorities. \
                             Through an SSH tunnel, choose Required: the certificate names \
                             the database, not the tunnel.",
                            cx,
                        )
                        .id("conn-tls-hint")
                        .test_support(),
                    )
                })
                .child(
                    Checkbox::new("use-ssh")
                        .cursor_pointer()
                        .label("Connect through an SSH tunnel")
                        .checked(self.use_ssh)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.use_ssh = *checked;
                            cx.notify();
                        })),
                )
                .when(self.use_ssh, |el| el.child(self.render_ssh(cx)))
            })
            .child(
                Checkbox::new("readonly")
                    .cursor_pointer()
                    .label("Read-only: block writes from the app and from agents")
                    .checked(self.readonly)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.readonly = *checked;
                        cx.notify();
                    })),
            )
    }

    /// What the tabs and the Connections dialog show for the connection.
    fn render_name(&self, cx: &mut Context<Self>) -> Div {
        h_flex()
            .gap_3()
            .items_start()
            .child(field("Name", Input::new(&self.name).id("conn-name"), cx))
            .child(
                field(
                    "Color",
                    h_flex().h_8().gap_1().children(
                        COLORS
                            .iter()
                            .enumerate()
                            .map(|(index, value)| self.render_swatch(index, value, cx))
                            .collect::<Vec<_>>(),
                    ),
                    cx,
                )
                .flex_none(),
            )
    }
}

/// One option in a row of them, with id `<group>-<label>`. The selected one stands out.
fn choice(
    group: &str,
    label: &'static str,
    selected: bool,
    cx: &App,
) -> impl StatefulInteractiveElement + IntoElement + use<> {
    let theme = cx.theme();
    let border = if selected {
        theme.foreground
    } else {
        theme.input
    };
    div()
        .id(SharedString::from(format!("{group}-{label}")))
        .test_support()
        // Shares the row, but wraps to the next line rather than cutting the label.
        .flex_1()
        .min_w(px(128.))
        .px_3()
        .py_1p5()
        .whitespace_nowrap()
        .text_center()
        .text_sm()
        .rounded(theme.radius)
        .border_1()
        .border_color(border)
        .cursor_pointer()
        .child(label)
}

/// How the list of engines names a family.
fn family_label(family: Family) -> &'static str {
    match family {
        Family::Sql => "SQL",
        Family::Document => "Document",
        Family::KeyValue => "Key-value",
    }
}

/// A labelled control. The label is quieter than its section's title.
fn field(label: &'static str, control: impl IntoElement, cx: &App) -> Div {
    v_flex()
        .flex_1()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(control)
}

fn hint(text: &'static str, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// A numbered step of the form: a card with its title above its fields.
fn section(step: usize, title: &'static str, body: Div, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .gap_3()
        .p_4()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_2()
                .child(
                    h_flex()
                        .size(px(20.))
                        .justify_center()
                        .rounded_full()
                        .bg(theme.muted)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(step.to_string()),
                )
                .child(div().text_sm().font_semibold().child(title)),
        )
        .child(body)
}

/// The widest the column the form is laid out in grows: 600 px of fields between 16 px
/// gutters. A narrower window narrows it.
const COLUMN_WIDTH: f32 = 632.;

impl ConnForm {
    /// Typing the details in, or finding them in AWS: a new connection's two ways.
    fn render_mode_switch(&self, cx: &mut Context<Self>) -> Div {
        let option = |id: &'static str, label: &'static str, mode: Mode, cx: &mut Context<Self>| {
            let theme = cx.theme();
            let selected = self.mode == mode;
            div()
                .id(id)
                .test_support()
                .flex_1()
                .py_1()
                .text_center()
                .text_sm()
                .rounded(theme.radius)
                .cursor_pointer()
                .when(selected, |el| {
                    el.bg(theme.background)
                        .font_medium()
                        .border_1()
                        .border_color(theme.border)
                })
                .when(!selected, |el| el.text_color(theme.muted_foreground))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| match mode {
                    Mode::Manual => this.show_manual(cx),
                    Mode::Aws => this.show_aws(window, cx),
                }))
        };
        h_flex()
            .flex_none()
            .p_0p5()
            .gap_0p5()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .child(option("mode-manual", "Enter details", Mode::Manual, cx))
            .child(option("mode-aws", "Find in AWS", Mode::Aws, cx))
    }

    /// The database found in AWS that filled in the form, with a way back to the list.
    fn render_found(&self, found: &Discovered, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        v_flex()
            .gap_2()
            .p_3()
            .rounded(theme.radius_lg)
            .bg(theme.muted)
            .child(
                h_flex()
                    .id("conn-found")
                    .test_support()
                    .gap_3()
                    .when_some(found.adapter, |el, adapter| {
                        el.child(logos::icon(adapter, 20.))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .font_semibold()
                                    .child(found.label.clone()),
                            )
                            .child(div().truncate().text_xs().text_color(muted).child(format!(
                                "Found in AWS · {} · {}",
                                found.engine_label(),
                                found.source.region
                            ))),
                    )
                    .child(
                        Button::new("conn-found-change")
                            .cursor_pointer()
                            .small()
                            .label("Change")
                            .on_click(cx.listener(|this, _, window, cx| this.show_aws(window, cx))),
                    ),
            )
            .when_some(found.note.clone(), |el, note| {
                el.child(
                    div()
                        .id("conn-notice")
                        .test_support()
                        .text_xs()
                        .text_color(muted)
                        .child(note),
                )
            })
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let picking = self.start.is_none() && self.mode == Mode::Aws;
        let can_use = self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.read(cx).can_use(cx));
        let save_shown = !picking && self.start.is_none() && self.chosen;
        let buttons = h_flex()
            .gap_2()
            .when(picking, |el| {
                el.child(
                    Button::new("aws-use")
                        .cursor_pointer()
                        .primary()
                        .label("Use this database")
                        .disabled(!can_use)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(picker) = this.picker.clone() {
                                picker.update(cx, |picker, cx| picker.use_selected(cx));
                            }
                        })),
                )
            })
            .when(save_shown, |el| {
                el.child(
                    Button::new("save")
                        .cursor_pointer()
                        .primary()
                        .label("Save connection")
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                )
            })
            .child(
                Button::new("cancel")
                    .cursor_pointer()
                    .ghost()
                    .label("Cancel")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnFormEvent::Cancel))),
            )
            .when(picking, |el| {
                el.child(div().flex_1()).child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Double-click or Enter also uses it."),
                )
            });
        v_flex()
            .id("conn-footer")
            .test_support()
            .flex_none()
            .w_full()
            .items_center()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(COLUMN_WIDTH))
                    .px_4()
                    .gap_2()
                    .when_some(self.error.clone(), |el, error| {
                        el.child(div().text_sm().text_color(cx.theme().danger).child(error))
                    })
                    .child(buttons),
            )
    }
}

impl Render for ConnForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.chosen;
        let new = self.previous.is_none();
        let title = if new {
            "New connection"
        } else {
            "Edit connection"
        };
        let heading = div().text_xl().font_semibold().child(title);
        let switch = (new && self.start.is_none()).then(|| self.render_mode_switch(cx));
        let column = || v_flex().w_full().max_w(px(COLUMN_WIDTH)).px_4().gap_4();
        let footer = self.render_footer(cx);
        // Finding it in AWS: the list scrolls in its own area, the rest stays put.
        if let (None, Mode::Aws, Some(picker)) = (&self.start, self.mode, self.picker.clone()) {
            return v_flex()
                .size_full()
                .child(
                    v_flex().flex_1().min_h_0().w_full().items_center().child(
                        column()
                            .flex_1()
                            .min_h_0()
                            .pt_8()
                            .pb_4()
                            .child(heading)
                            .children(switch)
                            .child(div().flex_1().min_h_0().child(picker)),
                    ),
                )
                .child(footer);
        }
        // In the order shown; each is numbered by its place.
        let mut sections = vec![("Database", self.render_database(cx))];
        if chosen {
            if self.adapter.is_file() {
                let file = Input::new(&self.database).id("conn-database");
                sections.push(("File", field("Database file", file, cx)));
            } else {
                sections.push(("Server", self.render_server(cx)));
                sections.push(("Credentials", self.render_credentials(cx)));
            }
            sections.push(("Security", self.render_security(cx)));
        }
        let mut sections: Vec<AnyElement> = sections
            .into_iter()
            .enumerate()
            .map(|(index, (title, body))| section(index + 1, title, body, cx).into_any_element())
            .collect();
        if let Some(start) = self.start.clone() {
            sections = vec![start.into_any_element()];
        } else if let Some(found) = self.found.clone() {
            sections.insert(0, self.render_found(&found, cx).into_any_element());
        }
        v_flex()
            .size_full()
            .child(
                div()
                    .id("conn-form")
                    .test_support()
                    .track_scroll(&self.form_scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex().w_full().items_center().child(
                            column()
                                .pt_8()
                                .pb_6()
                                .child(heading)
                                .children(switch)
                                .children(sections),
                        ),
                    ),
            )
            // Outside the scrolling part, so the buttons and any error stay in view.
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    // No glob from gpui_kit: its `test` macro would shadow the built-in `#[test]`.
    use super::{AuthKind, ConnForm, ConnFormEvent, Mode};
    use crate::aws_picker::tests::{found, many};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext, px, size};
    use std::cell::RefCell;
    use std::rc::Rc;
    use tuplebase_core::cloud::Discovery;
    use tuplebase_core::config::{Adapter, Auth, Connection, MongoSettings};
    use tuplebase_core::state::CloudSearch;

    /// A new connection's form in a window of a laptop's size.
    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<ConnForm>) {
        let (handle, form) = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::workspace::bind_keys(cx);
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| ConnForm::new(None, String::new(), CloudSearch::default(), window, cx))
            })
            .expect("open test window")
        });
        cx.update_window(handle, |_, window, _| {
            window.resize(size(px(1280.), px(760.)))
        })
        .unwrap();
        (handle, form)
    }

    fn act<R>(
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

    fn show(form: &Entity<ConnForm>, discovery: Discovery, cx: &mut TestAppContext) {
        cx.update(|cx| {
            let picker = form.read(cx).aws_picker().cloned().expect("AWS step");
            picker.update(cx, |p, cx| p.set_discovery(discovery, cx));
        });
    }

    fn saved(form: &Entity<ConnForm>, cx: &mut TestAppContext) -> Rc<RefCell<Vec<Connection>>> {
        let saved = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let saved = saved.clone();
            cx.subscribe(form, move |_, event, _| {
                if let ConnFormEvent::Save { connection, .. } = event {
                    saved.borrow_mut().push(connection.clone());
                }
            })
            .detach();
        });
        saved
    }

    /// Whether `id` lies in the window, not below its bottom edge.
    fn in_view(window: &gpui_kit::Window, id: &'static str) -> bool {
        let bounds = window.find(id).bounds();
        bounds.bottom() <= window.viewport_size().height && window.find(id).visible()
    }

    #[gpui_kit::test]
    fn the_buttons_stay_in_view_beside_a_long_list_and_after_using_one(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        act(handle, cx, |window, cx| window.click("source-aws", cx));
        show(
            &form,
            Discovery {
                found: many(40),
                ..Discovery::default()
            },
            cx,
        );
        act(handle, cx, |window, _| {
            // The list scrolls in its own area; the button is in the footer below it.
            assert!(in_view(window, "aws-use"));
            assert!(window.within("conn-footer").try_find("aws-use").is_some());
            assert!(window.within("aws-results").try_find("aws-use").is_none());
            let list = window.find("aws-results").bounds();
            assert!(list.bottom() <= window.find("conn-footer").bounds().top());
            let last = window.find("aws-result-39").bounds();
            assert!(
                last.top() >= list.bottom(),
                "the list scrolls to reach the last one"
            );
            assert!(window.find("aws-search").visible());
            assert_ne!(window.find("aws-use").disabled(), Some(true));
        });
        act(handle, cx, |window, cx| window.click("aws-use", cx));
        act(handle, cx, |window, _| {
            // The form shows what was found from the top, and Save is in view.
            assert!(window.try_find("aws-results").is_none());
            assert!(window.find("conn-found").visible());
            assert!(
                window.find("conn-found").bounds().top() < window.find("conn-name").bounds().top()
            );
            assert!(in_view(window, "conn-found"));
            assert!(in_view(window, "save"));
            assert!(window.within("conn-form").try_find("save").is_none());
            assert_eq!(window.find("conn-name").focused(), Some(true));
        });
        cx.update(|cx| assert_eq!(form.read(cx).name.read(cx).value(), "db-02"));
    }

    #[gpui_kit::test]
    fn the_list_keeps_room_in_a_short_window_with_many_profiles(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        cx.update_window(handle, |_, window, _| {
            window.resize(size(px(1000.), px(560.)))
        })
        .unwrap();
        act(handle, cx, |window, cx| window.click("source-aws", cx));
        act(handle, cx, |window, cx| {
            let picker = form.read(cx).aws_picker().cloned().expect("AWS step");
            let profiles = (0..40)
                .map(|i| tuplebase_core::cloud::aws::Profile {
                    name: format!("account-{i:02}-admin"),
                    region: Some("eu-west-1".into()),
                })
                .collect();
            picker.update(cx, |p, cx| p.set_profiles(Ok(profiles), window, cx));
        });
        show(
            &form,
            Discovery {
                found: many(40),
                ..Discovery::default()
            },
            cx,
        );
        act(handle, cx, |window, _| {
            assert!(in_view(window, "aws-use"));
            assert!(in_view(window, "aws-find"));
            assert!(in_view(window, "aws-search"));
            let list = window.find("aws-results").bounds();
            assert!(list.bottom() <= window.find("conn-footer").bounds().top());
            assert!(list.size.height >= px(4. * 34.), "{list:?}");
        });
    }

    #[gpui_kit::test]
    fn moving_with_the_keys_scrolls_the_selection_into_view(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        act(handle, cx, |window, cx| window.click("source-aws", cx));
        show(
            &form,
            Discovery {
                found: many(40),
                ..Discovery::default()
            },
            cx,
        );
        act(handle, cx, |window, cx| window.click("aws-search", cx));
        act(handle, cx, |window, cx| {
            for _ in 0..30 {
                window.press("down", cx);
            }
        });
        let index = cx.update(|cx| {
            let picker = form.read(cx).aws_picker().cloned().unwrap();
            picker.read(cx).selected_index().expect("selected")
        });
        act(handle, cx, |window, _| {
            let list = window.find("aws-results").bounds();
            let row = window
                .find(gpui_kit::SharedString::from(format!("aws-result-{index}")))
                .bounds();
            assert!(
                row.top() >= list.top() && row.bottom() <= list.bottom(),
                "{row:?} in {list:?}"
            );
        });
    }

    #[gpui_kit::test]
    fn switching_between_typing_and_aws_keeps_both_and_change_goes_back(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        act(handle, cx, |window, cx| window.click("adapter-MySQL", cx));
        act(handle, cx, |window, cx| {
            window.click("conn-host", cx);
            window.input("db.internal", cx);
            window.click("mode-aws", cx);
        });
        act(handle, cx, |window, _| {
            assert!(window.try_find("conn-host").is_none());
            assert!(window.find("aws-find").visible());
            // Nothing chosen yet.
            assert_eq!(window.find("aws-use").disabled(), Some(true));
        });
        show(
            &form,
            Discovery {
                found: vec![found("orders", Some(Adapter::Postgres))],
                ..Discovery::default()
            },
            cx,
        );
        act(handle, cx, |window, cx| window.click("mode-manual", cx));
        act(handle, cx, |window, cx| {
            // What was typed is still there, and the list waits as it was left.
            assert_eq!(window.find("conn-host").value(), Some("db.internal"));
            window.click("mode-aws", cx);
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-result-0").visible());
            window.press("enter", cx);
        });
        act(handle, cx, |window, cx| {
            assert_eq!(
                window.find("conn-host").value(),
                Some("orders.abc.eu-west-1.rds.amazonaws.com")
            );
            assert!(window.find("conn-notice").visible());
            window.click("conn-found-change", cx);
        });
        act(handle, cx, |window, _| {
            assert!(window.find("aws-result-0").visible());
            assert!(window.find("aws-chosen").visible());
        });
        cx.update(|cx| assert_eq!(form.read(cx).mode, Mode::Aws));
    }

    /// Chooses an engine, opening the list first when one is already chosen.
    fn pick(handle: AnyWindowHandle, engine: &str, cx: &mut TestAppContext) {
        let id = gpui_kit::SharedString::from(format!("adapter-{engine}"));
        act(handle, cx, |window, cx| {
            if window.try_find("engine-current").is_some() {
                window.click("engine-current", cx);
            }
        });
        act(handle, cx, |window, cx| window.click(id, cx));
    }

    #[gpui_kit::test]
    fn engines_are_listed_by_family_and_found_by_search(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        act(handle, cx, |window, cx| {
            assert!(window.find("engines-SQL").visible());
            assert!(window.find("engines-Document").visible());
            // No key-value engine yet: an empty family isn't shown.
            assert!(window.try_find("engines-Key-value").is_none());
            assert!(window.find("adapter-Amazon DocumentDB").visible());
            window.click("engine-search", cx);
            window.input("mongo", cx);
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("adapter-MongoDB").visible());
            assert!(window.try_find("adapter-PostgreSQL").is_none());
            assert!(window.try_find("engines-SQL").is_none());
            // Enter takes the first engine left.
            window.press("enter", cx);
        });
        act(handle, cx, |window, cx| {
            assert_eq!(form.read(cx).adapter, Adapter::Mongodb);
            // Chosen, the engine shows in one line, and the rest of the form appears.
            assert!(window.find("engine-current").visible());
            assert!(window.try_find("engine-search").is_none());
            assert!(window.find("conn-host").visible());
        });
        // A family's name finds its engines; nothing found says so.
        act(handle, cx, |window, cx| window.click("engine-current", cx));
        act(handle, cx, |window, cx| {
            window.click("engine-search", cx);
            window.press("secondary-a", cx);
            window.input("document", cx);
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("adapter-MongoDB").visible());
            assert!(window.find("adapter-Amazon DocumentDB").visible());
            assert!(window.try_find("adapter-MySQL").is_none());
            window.press("secondary-a", cx);
            window.input("oracle", cx);
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("engines-none").visible());
            window.press("secondary-a", cx);
            window.input("my", cx);
        });
        act(handle, cx, |window, cx| window.click("adapter-MySQL", cx));
        act(handle, cx, |window, cx| {
            assert_eq!(form.read(cx).adapter, Adapter::Mysql);
            assert!(window.try_find("engines-SQL").is_none());
        });
    }

    #[gpui_kit::test]
    fn editing_shows_the_engine_in_one_line_until_asked_to_change_it(cx: &mut TestAppContext) {
        let (handle, form) = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::workspace::bind_keys(cx);
            let conn = Connection {
                name: "orders".into(),
                folder: String::new(),
                adapter: Adapter::Postgres,
                host: "db.example.com".into(),
                port: 5432,
                database: "app".into(),
                user: "app".into(),
                readonly: false,
                color: String::new(),
                tls: Default::default(),
                ssh: None,
                auth: Auth::Password,
                source: None,
                mongo: None,
            };
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| {
                    ConnForm::new(
                        Some(&conn),
                        String::new(),
                        CloudSearch::default(),
                        window,
                        cx,
                    )
                })
            })
            .expect("open test window")
        });
        // Narrow: nothing is cut short; the choices wrap instead.
        cx.update_window(handle, |_, window, _| {
            window.resize(size(px(420.), px(900.)))
        })
        .unwrap();
        act(handle, cx, |window, cx| {
            assert!(window.find("engine-current").visible());
            assert!(window.try_find("engines-SQL").is_none());
            assert!(window.find("auth-Secrets Manager").visible());
            assert!(window.find("auth-IAM token").visible());
            window.click("engine-current", cx);
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("engines-SQL").visible());
            assert!(window.find("adapter-Amazon DocumentDB").visible());
            window.click("adapter-PostgreSQL", cx);
        });
        // Choosing the same engine closes the list and changes nothing.
        act(handle, cx, |window, cx| {
            assert!(window.find("engine-current").visible());
            assert_eq!(form.read(cx).adapter, Adapter::Postgres);
        });
    }

    #[gpui_kit::test]
    fn only_the_engines_ways_to_sign_in_are_offered(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        pick(handle, "PostgreSQL", cx);
        act(handle, cx, |window, cx| {
            for kind in AuthKind::ALL {
                let id = gpui_kit::SharedString::from(format!("auth-{}", kind.label()));
                assert!(window.find(id).visible(), "{kind:?}");
            }
            window.click("auth-IAM token", cx);
        });
        // MongoDB takes no IAM token: the choice goes, and the form goes back to a password.
        pick(handle, "MongoDB", cx);
        act(handle, cx, |window, cx| {
            assert!(window.try_find("auth-IAM token").is_none());
            assert!(window.find("auth-Parameter Store").visible());
            assert!(window.find("conn-password").visible());
            assert_eq!(form.read(cx).auth_kind, AuthKind::Password);
        });
    }

    #[gpui_kit::test]
    fn the_user_example_follows_the_engine(cx: &mut TestAppContext) {
        let (handle, _form) = open(cx);
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        for (engine, user) in [
            ("MySQL", "root"),
            ("MariaDB", "root"),
            ("Redshift", "awsuser"),
            ("PostgreSQL", "postgres"),
        ] {
            pick(handle, engine, cx);
            act(handle, cx, |window, _| {
                assert_eq!(
                    window.find("conn-user").placeholder(),
                    Some(user),
                    "{engine}"
                );
            });
        }
        // A file has no user to ask for.
        pick(handle, "SQLite", cx);
        act(handle, cx, |window, _| {
            assert!(window.try_find("conn-user").is_none());
        });
    }

    #[gpui_kit::test]
    fn a_mongodb_connection_has_srv_and_options(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        let saved = saved(&form, cx);
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        act(handle, cx, |window, cx| {
            assert!(window.try_find("conn-mongo-options").is_none());
            window.click("adapter-MongoDB", cx)
        });
        act(handle, cx, |window, cx| {
            assert_eq!(window.find("conn-user").placeholder(), Some("admin"));
            window.click("conn-name", cx);
            window.input("docs", cx);
            window.click("conn-host", cx);
            window.input("cluster0.example.net", cx);
            window.click("conn-mongo-options", cx);
            window.input("authSource=admin", cx);
            window.click("conn-mongo-srv", cx);
        });
        act(handle, cx, |window, cx| {
            assert!(window.try_find("conn-port").is_none(), "SRV has no port");
            window.click("save", cx);
        });
        let saved = saved.borrow();
        let connection = saved.first().expect("saved");
        assert_eq!(connection.adapter, Adapter::Mongodb);
        assert_eq!(
            connection.mongo,
            Some(MongoSettings {
                srv: true,
                options: "authSource=admin".into()
            })
        );
        let mut config = tuplebase_core::config::Config::default();
        config.upsert(connection.clone(), None).unwrap();
    }

    #[gpui_kit::test]
    fn pasting_a_connection_string_fills_the_fields_and_keeps_only_the_password(
        cx: &mut TestAppContext,
    ) {
        let (handle, form) = open(cx);
        let saved = Rc::new(RefCell::new(None));
        cx.update(|cx| {
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
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        act(handle, cx, |window, cx| window.click("adapter-MongoDB", cx));
        // The secret in an option is left out, and only the first of two hosts kept.
        let uri = "mongodb://ada:s3cr%2Ft@db1.example.com:27018,db2.example.com/app\
                   ?replicaSet=rs0&authSource=admin&password=s3cr%2Ft";
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(uri.to_owned()));
        act(handle, cx, |window, cx| {
            window.click("conn-mongo-paste", cx)
        });
        act(handle, cx, |window, cx| {
            let note = window.find("conn-paste-note");
            assert!(note.visible());
            let read = |input: &Entity<gpui_kit::component::input::InputState>| {
                input.read(cx).value().to_string()
            };
            let form = form.read(cx);
            assert_eq!(read(&form.host), "db1.example.com");
            assert_eq!(read(&form.port), "27018");
            assert_eq!(read(&form.database), "app");
            assert_eq!(read(&form.user), "ada");
            assert_eq!(read(&form.password), "s3cr/t");
            assert_eq!(read(&form.mongo_options), "replicaSet=rs0&authSource=admin");
            assert!(!form.mongo_srv);
            let note = form.paste_note.clone().unwrap_or_default();
            assert!(note.contains("first of 2 hosts"), "{note}");
            assert!(note.contains("Left out password"), "{note}");
            assert!(!note.contains("s3cr"), "{note}");
        });
        act(handle, cx, |window, cx| {
            window.click("conn-name", cx);
            window.input("docs", cx);
            window.click("save", cx);
        });
        let (connection, password) = saved.borrow_mut().take().expect("saved");
        assert_eq!(password.as_deref(), Some("s3cr/t"));
        assert_eq!(connection.port, 27018);
        assert_eq!(
            connection.mongo,
            Some(MongoSettings {
                srv: false,
                options: "replicaSet=rs0&authSource=admin".into()
            })
        );
        // What is saved to the file holds neither the password nor the string.
        let file = serde_json::to_string(&connection).unwrap();
        assert!(!file.contains("s3cr"), "{file}");
        assert!(!file.contains("mongodb://"), "{file}");

        // An SRV string turns SRV on and leaves the port to DNS.
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
            "mongodb+srv://cluster0.example.net/?retryWrites=true".to_owned(),
        ));
        act(handle, cx, |window, cx| {
            window.click("conn-mongo-paste", cx)
        });
        act(handle, cx, |window, cx| {
            assert!(window.try_find("conn-port").is_none());
            assert!(window.try_find("conn-paste-note").is_none());
            let form = form.read(cx);
            assert!(form.mongo_srv);
            assert_eq!(form.mongo_options.read(cx).value(), "retryWrites=true");
            assert_eq!(form.host.read(cx).value(), "cluster0.example.net");
        });

        // Something else on the clipboard says so, and changes nothing.
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
            "postgres://u:hidden@h/db".to_owned(),
        ));
        act(handle, cx, |window, cx| {
            window.click("conn-mongo-paste", cx)
        });
        cx.update(|cx| {
            let form = form.read(cx);
            let error = form.error.clone().unwrap_or_default();
            assert!(error.contains("mongodb://"), "{error}");
            assert!(!error.contains("hidden"), "{error}");
            assert_eq!(form.host.read(cx).value(), "cluster0.example.net");
        });
    }

    #[gpui_kit::test]
    fn serverless_iam_asks_for_no_user(cx: &mut TestAppContext) {
        let (handle, form) = open(cx);
        let saved = saved(&form, cx);
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        act(handle, cx, |window, cx| {
            window.click("adapter-Redshift", cx)
        });
        act(handle, cx, |window, cx| {
            window.click("conn-name", cx);
            window.input("analytics", cx);
            window.click("conn-host", cx);
            window.input("warehouse.abc123.eu-west-1.redshift.amazonaws.com", cx);
            window.click("auth-IAM token", cx);
        });
        act(handle, cx, |window, cx| {
            // A cluster signs in as the user it is asked for.
            assert!(window.find("conn-user").visible());
            window.click("conn-user", cx);
            window.input("analyst", cx);
            form.update(cx, |form, cx| {
                form.host.update(cx, |host, cx| {
                    host.set_value(
                        "analytics.123456789012.eu-west-1.redshift-serverless.amazonaws.com",
                        window,
                        cx,
                    )
                });
                cx.notify();
            });
        });
        act(handle, cx, |window, cx| {
            assert!(window.try_find("conn-user").is_none());
            assert!(window.find("conn-iam-hint").visible());
            window.click("save", cx);
        });
        let saved = saved.borrow();
        let connection = saved.first().expect("saved");
        assert!(matches!(connection.auth, Auth::AwsIam(_)));
        // Not used, so not kept.
        assert_eq!(connection.user, "");
        let mut config = tuplebase_core::config::Config::default();
        config.upsert(connection.clone(), None).unwrap();
    }
}
