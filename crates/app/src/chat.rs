//! Chat panel: ask the user's own AI agent about their databases.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::chat::{self, AGENT_NAME, Event};

use crate::runtime::blocking;

const PANEL_WIDTH: f32 = 380.;
/// How often the panel shows what the agent has done so far.
const STREAM_POLL: Duration = Duration::from_millis(150);
/// One click asks these; they double as a check that the agent reaches the server.
const SUGGESTIONS: [&str; 3] = [
    "Which connections can you see?",
    "Is the TupleBase MCP server working?",
    "What tables does each connection have?",
];

pub enum ChatEvent {
    Close,
}

#[derive(Debug, Clone, PartialEq)]
enum Entry {
    User(SharedString),
    /// Markdown.
    Agent(SharedString),
    Tool {
        name: SharedString,
        input: SharedString,
    },
    Error(SharedString),
}

impl From<Event> for Entry {
    fn from(event: Event) -> Self {
        match event {
            Event::Text(text) => Self::Agent(text.into()),
            Event::Tool { name, input } => Self::Tool {
                name: name.into(),
                input: input.into(),
            },
            Event::ToolFailed(error) => Self::Error(error.into()),
        }
    }
}

/// What the agent has done since the panel last looked.
type Inbox = Arc<Mutex<Vec<Event>>>;

pub struct ChatPanel {
    input: Entity<InputState>,
    entries: Vec<Entry>,
    /// Tool calls, by entry, whose arguments are shown.
    expanded: HashSet<usize>,
    /// The agent's name for this conversation, once it has answered.
    session: Option<String>,
    busy: bool,
    /// Counts messages sent and chats cleared, so an answer nobody waits for is dropped.
    turn: usize,
    scroll: ScrollHandle,
    _input_subscription: Subscription,
}

impl EventEmitter<ChatEvent> for ChatPanel {}

impl ChatPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(format!("Ask {AGENT_NAME}…")));
        let _input_subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.send_input(window, cx);
                }
            });
        Self {
            input,
            entries: Vec::new(),
            expanded: HashSet::new(),
            session: None,
            busy: false,
            turn: 0,
            scroll: ScrollHandle::new(),
            _input_subscription,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn send_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() || self.busy {
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.send(text, cx);
    }

    fn send(&mut self, prompt: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.turn += 1;
        self.push(Entry::User(prompt.clone().into()), cx);
        let inbox = Inbox::default();
        let turn = self.turn;
        let session = self.session.clone();

        // Shows the agent's steps as they come, until it has answered.
        let seen = inbox.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(STREAM_POLL).await;
                let waiting = this.update(cx, |this, cx| {
                    let current = this.busy && this.turn == turn;
                    if current {
                        this.take(&seen, cx);
                    }
                    current
                });
                if !matches!(waiting, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let events = inbox.clone();
            let result = blocking(move || {
                chat::ask(&prompt, session.as_deref(), |event| {
                    events.lock().unwrap().push(event);
                })
            })
            .await;
            this.update(cx, |this, cx| {
                if this.turn != turn {
                    return;
                }
                this.take(&inbox, cx);
                this.busy = false;
                match result {
                    Ok(session) => this.session = session,
                    Err(e) => this.push(Entry::Error(format!("{e:#}").into()), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn take(&mut self, inbox: &Inbox, cx: &mut Context<Self>) {
        let events = std::mem::take(&mut *inbox.lock().unwrap());
        for event in events {
            self.push(event.into(), cx);
        }
    }

    fn push(&mut self, entry: Entry, cx: &mut Context<Self>) {
        self.entries.push(entry);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Forgets the conversation. An answer still on its way is dropped when it arrives.
    fn clear(&mut self, cx: &mut Context<Self>) {
        self.turn += 1;
        self.entries.clear();
        self.expanded.clear();
        self.session = None;
        self.busy = false;
        cx.notify();
    }

    fn toggle_tool(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&ix) {
            self.expanded.insert(ix);
        }
        cx.notify();
    }

    fn render_entry(&self, ix: usize, entry: &Entry, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        match entry {
            Entry::User(text) => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(relative(0.85))
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .bg(theme.secondary)
                        .text_sm()
                        .child(text.clone()),
                )
                .into_any_element(),
            Entry::Agent(text) => div()
                .text_sm()
                .child(TextView::markdown(("chat-answer", ix), text.clone()))
                .into_any_element(),
            // One quiet line; the arguments show on a click.
            Entry::Tool { name, input } => {
                let open = self.expanded.contains(&ix);
                let chevron = if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                };
                v_flex()
                    // Sits close to the tool calls around it.
                    .my(px(-4.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .font_family(theme.mono_font_family.clone())
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("chat-tool-{ix}")))
                            .test_support()
                            .gap_1()
                            .child(
                                Icon::new(chevron)
                                    .xsmall()
                                    .when(input.is_empty(), |icon| icon.invisible()),
                            )
                            .child(name.clone())
                            .when(!input.is_empty(), |el| {
                                el.cursor_pointer()
                                    .hover(|style| style.text_color(theme.foreground))
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.toggle_tool(ix, cx)),
                                    )
                            }),
                    )
                    .when(open, |el| {
                        el.child(
                            div()
                                .id(SharedString::from(format!("chat-tool-input-{ix}")))
                                .test_support()
                                .mt_1()
                                .ml_4()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme.secondary)
                                .child(input.clone()),
                        )
                    })
                    .into_any_element()
            }
            Entry::Error(text) => div()
                .text_sm()
                .text_color(theme.danger)
                .child(text.clone())
                .into_any_element(),
        }
    }

    fn render_suggestions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Ask {AGENT_NAME} about your databases. It uses your own account and \
                         reaches them through TupleBase, read-only."
                    )),
            )
            .children(SUGGESTIONS.iter().enumerate().map(|(ix, text)| {
                Button::new(SharedString::from(format!("chat-suggestion-{ix}")))
                    .cursor_pointer()
                    .outline()
                    .small()
                    .label(*text)
                    .on_click(cx.listener(|this, _, _, cx| this.send((*text).to_owned(), cx)))
            }))
    }
}

impl Render for ChatPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, muted) = (cx.theme().border, cx.theme().muted_foreground);
        let entries: Vec<AnyElement> = self
            .entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| self.render_entry(ix, entry, cx))
            .collect();
        v_flex()
            .id("chat-panel")
            .test_support()
            .w(px(PANEL_WIDTH))
            .h_full()
            .flex_none()
            .border_l_1()
            .border_color(border)
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .justify_between()
                    .border_b_1()
                    .border_color(border)
                    .child(div().pl_1().text_sm().font_semibold().child(AGENT_NAME))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("chat-new")
                                    .cursor_pointer()
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("New chat")
                                    .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                            )
                            .child(
                                Button::new("chat-close")
                                    .cursor_pointer()
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip("Close")
                                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ChatEvent::Close))),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("chat-messages")
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .gap_3()
                    .overflow_y_scroll()
                    .when(self.entries.is_empty(), |el| {
                        el.child(self.render_suggestions(cx))
                    })
                    .children(entries)
                    .when(self.busy, |el| {
                        el.child(
                            h_flex()
                                .id("chat-busy")
                                .test_support()
                                .gap_2()
                                .text_sm()
                                .text_color(muted)
                                .child(Spinner::new().small())
                                .child("Thinking…"),
                        )
                    }),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(border)
                    .child(Input::new(&self.input).id("chat-input").small())
                    .child(
                        Button::new("chat-send")
                            .cursor_pointer()
                            .primary()
                            .small()
                            .icon(IconName::ArrowUp)
                            .tooltip("Send")
                            .disabled(self.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.send_input(window, cx)),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
impl ChatPanel {
    pub(crate) fn show(&mut self, event: Event, cx: &mut Context<Self>) {
        self.push(event.into(), cx);
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.busy
    }

    pub(crate) fn asked(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::User(text) => Some(text.to_string()),
                _ => None,
            })
            .collect()
    }
}
