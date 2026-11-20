//! Add / edit connection form. Collects values; the workspace persists them.

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{
    Adapter, Auth, AuthKind, AwsIamAuth, AwsSecretRef, Connection, Source, SshConfig, TlsMode,
};
use tuplebase_core::ssh;
use tuplebase_core::state::CloudSearch;

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

pub struct ConnForm {
    /// The step before the form for a new connection: where the database is.
    start: Option<Entity<ConnSource>>,
    _start_subscription: Option<Subscription>,
    /// What to know about a database that was found, shown above the form.
    notice: Option<SharedString>,
    previous: Option<String>,
    adapter: Adapter,
    /// A new connection asks for the database first and shows the rest once it is chosen.
    chosen: bool,
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
        let database = input("Optional", field(|c| c.database.clone()));
        let user = input("postgres", field(|c| c.user.clone()));
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
            let start = cx.new(|_| ConnSource::new(last_aws));
            let subscription = cx.subscribe_in(&start, window, Self::on_start_event);
            (Some(start), Some(subscription))
        } else {
            (None, None)
        };
        Self {
            start,
            _start_subscription: start_subscription,
            notice: None,
            previous: existing.map(|c| c.name.clone()),
            adapter: existing.map_or(Adapter::Postgres, |c| c.adapter),
            chosen: existing.is_some(),
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
        match event {
            ConnSourceEvent::Manual => self.close_start(cx),
            ConnSourceEvent::Use { connection, note } => {
                self.prefill(connection, window, cx);
                self.notice = note.clone().map(Into::into);
                self.close_start(cx);
            }
            ConnSourceEvent::SearchedAws(search) => {
                cx.emit(ConnFormEvent::SearchedAws(search.clone()));
            }
        }
    }

    fn close_start(&mut self, cx: &mut Context<Self>) {
        self.start = None;
        self._start_subscription = None;
        cx.notify();
    }

    /// The step before the form, while it is shown.
    #[cfg(test)]
    pub fn start(&self) -> Option<&Entity<ConnSource>> {
        self.start.as_ref()
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
        self.adapter = conn.adapter;
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
        let password = visible(&self.password, auth.is_password(), cx);
        let ssh_password = text(&self.ssh_password, cx);
        cx.emit(ConnFormEvent::Save {
            connection: Connection {
                name: text(&self.name, cx),
                folder: self.folder.clone(),
                adapter: self.adapter,
                host: visible(&self.host, host_shown, cx),
                port,
                database: text(&self.database, cx),
                user: text(&self.user, cx),
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
            },
            password: (!password.is_empty()).then_some(password),
            ssh_password: (uses_ssh && !ssh_password.is_empty()).then_some(ssh_password),
            previous: self.previous.clone(),
        });
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
            .flex_1()
            .gap_2()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .child(logos::icon(adapter, 18.))
            .child(div().text_sm().child(adapter.label()))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.adapter = adapter;
                // The first choice reveals the rest of the form, so move on to it.
                if !this.chosen {
                    this.chosen = true;
                    let first = if adapter.is_file() {
                        &this.database
                    } else {
                        &this.host
                    };
                    first.update(cx, |input, cx| input.focus(window, cx));
                }
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
                return column.child(hint(
                    "A token for the user below is made from your AWS profile each time you \
                     connect. The user needs rds-db:connect, and the database IAM \
                     authentication turned on.",
                    cx,
                ));
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
        v_flex()
            .gap_3()
            .when(!self.chosen, |el| {
                el.child(hint("Choose the database you want to connect to.", cx))
            })
            .child(
                h_flex()
                    .gap_2()
                    .children(Adapter::ALL.map(|a| self.render_adapter(a, cx))),
            )
            .when(self.chosen, |el| el.child(self.render_name(cx)))
    }

    fn render_server(&self, cx: &App) -> Div {
        let from_secret = self.host_from_secret();
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
                            .child(div().w(px(90.)).child(field(
                                "Port",
                                Input::new(&self.port).id("conn-port"),
                                cx,
                            )))
                    })
                    .child(field(
                        "Database",
                        Input::new(&self.database).id("conn-database"),
                        cx,
                    )),
            )
    }

    fn render_credentials(&self, cx: &mut Context<Self>) -> Div {
        let kind = self.auth_kind;
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .children(AuthKind::ALL.map(|k| self.render_auth(k, cx))),
            )
            .when(kind != AuthKind::Password, |el| {
                el.child(self.render_aws(kind, cx))
            })
            .child(
                h_flex()
                    .gap_3()
                    .child(field("User", Input::new(&self.user).id("conn-user"), cx))
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

    /// What the sidebar shows for the connection.
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
        .flex_1()
        .py_1p5()
        .text_center()
        .text_sm()
        .rounded(theme.radius)
        .border_1()
        .border_color(border)
        .cursor_pointer()
        .child(label)
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

impl Render for ConnForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chosen = self.chosen;
        let title = if self.previous.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
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
        } else if let Some(notice) = self.notice.clone() {
            let notice = div()
                .id("conn-notice")
                .test_support()
                .p_3()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .text_sm()
                .child(notice);
            sections.insert(0, notice.into_any_element());
        }
        let column = || v_flex().w(px(560.)).gap_4();
        v_flex()
            .size_full()
            .child(
                div()
                    .id("conn-form")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex().w_full().items_center().child(
                            column()
                                .pt_8()
                                .pb_6()
                                .child(div().text_xl().font_semibold().child(title))
                                .children(sections),
                        ),
                    ),
            )
            // Outside the scrolling part, so Save and any error stay in view.
            .child(
                v_flex()
                    .w_full()
                    .items_center()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        column()
                            .gap_2()
                            .when_some(self.error.clone(), |el, error| {
                                el.child(div().text_sm().text_color(cx.theme().danger).child(error))
                            })
                            .child(
                                h_flex()
                                    .gap_2()
                                    .when(chosen, |el| {
                                        el.child(
                                            Button::new("save")
                                                .cursor_pointer()
                                                .primary()
                                                .label("Save connection")
                                                .on_click(
                                                    cx.listener(|this, _, _, cx| this.save(cx)),
                                                ),
                                        )
                                    })
                                    .child(
                                        Button::new("cancel")
                                            .cursor_pointer()
                                            .ghost()
                                            .label("Cancel")
                                            .on_click(cx.listener(|_, _, _, cx| {
                                                cx.emit(ConnFormEvent::Cancel)
                                            })),
                                    ),
                            ),
                    ),
            )
    }
}
