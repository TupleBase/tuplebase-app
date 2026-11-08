//! Add / edit connection form. Collects values; the workspace persists them.

use gpui_kit::component::button::*;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::config::{Adapter, Connection};

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
    adapter: Entity<SelectState<Vec<&'static str>>>,
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
    /// `existing` pre-fills the form for editing.
    pub fn new(existing: Option<&Connection>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let labels: Vec<&'static str> = Adapter::ALL.iter().map(|a| a.label()).collect();
        let adapter_ix = existing
            .and_then(|c| Adapter::ALL.iter().position(|a| *a == c.adapter))
            .unwrap_or(0);
        let mut input = |placeholder: &str, value: String| {
            let placeholder = placeholder.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let field = |f: fn(&Connection) -> String| existing.map(f).unwrap_or_default();
        let name = input("my-database", field(|c| c.name.clone()));
        let host = input("localhost", field(|c| c.host.clone()));
        let port = input("default port", field(|c| c.port.to_string()));
        let database = input("optional", field(|c| c.database.clone()));
        let user = input("user", field(|c| c.user.clone()));
        let password = input(
            if existing.is_some() {
                "unchanged"
            } else {
                "password"
            },
            String::new(),
        );
        Self {
            previous: existing.map(|c| c.name.clone()),
            adapter: cx.new(|cx| {
                SelectState::new(
                    labels,
                    Some(IndexPath::default().row(adapter_ix)),
                    window,
                    cx,
                )
            }),
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
        let adapter = self
            .adapter
            .read(cx)
            .selected_index(cx)
            .map_or(Adapter::Postgres, |ix| Adapter::ALL[ix.row]);
        let port_text = text(&self.port, cx);
        let port = if port_text.is_empty() {
            adapter.default_port()
        } else {
            match port_text.parse() {
                Ok(port) => port,
                Err(_) => {
                    self.error = Some("Port must be a number".into());
                    cx.notify();
                    return;
                }
            }
        };
        let password = text(&self.password, cx);
        cx.emit(ConnFormEvent::Save {
            connection: Connection {
                name: text(&self.name, cx),
                adapter,
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
}

fn row(label: &'static str, control: impl IntoElement) -> Div {
    h_flex()
        .gap_3()
        .child(div().w(px(110.)).child(label))
        .child(div().flex_1().child(control))
}

impl Render for ConnForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.previous.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        v_flex()
            .p_6()
            .gap_3()
            .max_w(px(560.))
            .child(div().text_lg().font_semibold().child(title))
            .child(row("Type", Select::new(&self.adapter)))
            .child(row("Name", Input::new(&self.name).id("conn-name")))
            .child(row("Host", Input::new(&self.host).id("conn-host")))
            .child(row("Port", Input::new(&self.port).id("conn-port")))
            .child(row(
                "Database",
                Input::new(&self.database).id("conn-database"),
            ))
            .child(row("User", Input::new(&self.user).id("conn-user")))
            .child(row(
                "Password",
                Input::new(&self.password).id("conn-password").mask_toggle(),
            ))
            .child(row(
                "",
                Checkbox::new("readonly")
                    .label("Read-only (block writes)")
                    .checked(self.readonly)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.readonly = *checked;
                        cx.notify();
                    })),
            ))
            .when_some(self.error.clone(), |el, error| {
                el.child(div().text_color(cx.theme().danger).child(error))
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnFormEvent::Cancel))),
                    ),
            )
    }
}
