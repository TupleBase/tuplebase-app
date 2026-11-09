//! Add / edit connection form. Collects values; the workspace persists them.

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{Adapter, Connection, SshConfig, TlsMode};
use tuplebase_core::ssh;

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
}

pub struct ConnForm {
    previous: Option<String>,
    adapter: Adapter,
    pub(crate) folder: String,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
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
    pub fn new(
        existing: Option<&Connection>,
        folder: String,
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
        Self {
            previous: existing.map(|c| c.name.clone()),
            adapter: existing.map_or(Adapter::Postgres, |c| c.adapter),
            folder,
            name,
            host,
            port,
            database,
            user,
            password,
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

    fn save(&mut self, cx: &mut Context<Self>) {
        let text = |input: &Entity<InputState>, cx: &App| input.read(cx).value().trim().to_string();
        let port_text = text(&self.port, cx);
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
        let password = text(&self.password, cx);
        let ssh_password = text(&self.ssh_password, cx);
        cx.emit(ConnFormEvent::Save {
            connection: Connection {
                name: text(&self.name, cx),
                folder: self.folder.clone(),
                adapter: self.adapter,
                host: text(&self.host, cx),
                port,
                database: text(&self.database, cx),
                user: text(&self.user, cx),
                readonly: self.readonly,
                color: self.color.clone(),
                tls: self.tls,
                ssh,
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
        let selected = self.adapter == adapter;
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
            .on_click(cx.listener(move |this, _, _, cx| {
                this.adapter = adapter;
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
        let theme = cx.theme();
        let selected = self.tls == mode;
        let border = if selected {
            theme.foreground
        } else {
            theme.input
        };
        div()
            .id(SharedString::from(format!("tls-{}", mode.label())))
            .test_support()
            .flex_1()
            .py_1p5()
            .text_center()
            .text_sm()
            .rounded(theme.radius)
            .border_1()
            .border_color(border)
            .cursor_pointer()
            .child(mode.label())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tls = mode;
                cx.notify();
            }))
    }

    fn render_ssh(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
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
                    ))
                    .child(div().w(px(120.)).child(field(
                        "SSH port",
                        Input::new(&self.ssh_port).id("conn-ssh-port"),
                    ))),
            )
            .child(field(
                "SSH user",
                Input::new(&self.ssh_user).id("conn-ssh-user"),
            ))
            .child(field(
                "Private key file (leave blank to use a password)",
                Input::new(&self.ssh_key).id("conn-ssh-key"),
            ))
            .child(field(
                "SSH password or key passphrase",
                Input::new(&self.ssh_password)
                    .id("conn-ssh-password")
                    .mask_toggle(),
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
                                .label("Get from server")
                                .loading(self.fetching_fingerprint)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.fetch_fingerprint(window, cx);
                                })),
                        ),
                )
                .child(div().text_xs().text_color(muted).child(
                    "Compare this with your server's key before saving. \
                     It proves you reached the right machine.",
                )),
            )
    }
}

fn field(label: &'static str, control: impl IntoElement) -> Div {
    v_flex()
        .flex_1()
        .gap_1()
        .child(div().text_sm().font_medium().child(label))
        .child(control)
}

impl Render for ConnForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_file = self.adapter.is_file();
        let title = if self.previous.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        div()
            .id("conn-form")
            .test_support()
            .size_full()
            .overflow_y_scroll()
            .child(
                v_flex().w_full().items_center().child(
                    v_flex()
                        .w(px(520.))
                        .pt(px(56.))
                        .pb_8()
                        .gap_4()
                        .child(div().text_xl().font_semibold().child(title))
                        .child(
                            h_flex()
                                .gap_2()
                                .children(Adapter::ALL.map(|a| self.render_adapter(a, cx))),
                        )
                        .child(field("Name", Input::new(&self.name).id("conn-name")))
                        .child(field(
                            "Color",
                            h_flex().gap_1().children(
                                COLORS
                                    .iter()
                                    .enumerate()
                                    .map(|(index, value)| self.render_swatch(index, value, cx))
                                    .collect::<Vec<_>>(),
                            ),
                        ))
                        .when(is_file, |el| {
                            el.child(field(
                                "Database file",
                                Input::new(&self.database).id("conn-database"),
                            ))
                        })
                        .when(!is_file, |el| {
                            el.child(
                                h_flex()
                                    .gap_3()
                                    .child(field("Host", Input::new(&self.host).id("conn-host")))
                                    .child(div().w(px(120.)).child(field(
                                        "Port",
                                        Input::new(&self.port).id("conn-port"),
                                    ))),
                            )
                            .child(
                                h_flex()
                                    .gap_3()
                                    .child(field(
                                        "Database",
                                        Input::new(&self.database).id("conn-database"),
                                    ))
                                    .child(field("User", Input::new(&self.user).id("conn-user"))),
                            )
                            .child(field(
                                "Password",
                                Input::new(&self.password).id("conn-password").mask_toggle(),
                            ))
                            .child(field(
                                "Encryption",
                                h_flex()
                                    .gap_2()
                                    .children(TlsMode::ALL.map(|mode| self.render_tls(mode, cx))),
                            ))
                            .child(
                                Checkbox::new("use-ssh")
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
                                .label("Read-only: block writes from the app and from agents")
                                .checked(self.readonly)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.readonly = *checked;
                                    cx.notify();
                                })),
                        )
                        .when_some(self.error.clone(), |el, error| {
                            el.child(div().text_sm().text_color(cx.theme().danger).child(error))
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("save")
                                        .primary()
                                        .label("Save connection")
                                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                                )
                                .child(Button::new("cancel").ghost().label("Cancel").on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(ConnFormEvent::Cancel)),
                                )),
                        ),
                ),
            )
    }
}
