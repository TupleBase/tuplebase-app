//! Add / edit connection form. Collects values; the workspace persists them.

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{Adapter, Connection};

use crate::logos;

pub enum ConnFormEvent {
    Save {
        connection: Connection,
        /// `None` when left blank: keeps any stored password.
        password: Option<String>,
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
        let password = text(&self.password, cx);
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
            },
            password: (!password.is_empty()).then_some(password),
            previous: self.previous.clone(),
        });
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
            .child(logos::tile(adapter, 24.))
            .child(div().text_sm().child(adapter.label()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.adapter = adapter;
                cx.notify();
            }))
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
        let title = if self.previous.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        div().id("conn-form").size_full().overflow_y_scroll().child(
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
                    .child(
                        h_flex()
                            .gap_3()
                            .child(field("Host", Input::new(&self.host).id("conn-host")))
                            .child(
                                div()
                                    .w(px(120.))
                                    .child(field("Port", Input::new(&self.port).id("conn-port"))),
                            ),
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
