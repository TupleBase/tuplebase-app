//! The bar across the top of the window: the Connections button, a tab per open
//! connection, and on the right updates, the chat, Settings and the MCP status.
//!
//! On macOS the window's traffic lights sit at its left end, and its empty part moves
//! the window, as a title bar would.

use std::sync::atomic::Ordering;

use gpui_kit::component::button::*;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{CONNECTIONS_KEY, OpenTab, SettingsPage, UpdateState, Workspace, mcp_status_text};
use crate::connection_tab::Light;
use crate::logos;

pub(super) const HEIGHT: f32 = 42.;
/// Room for the traffic lights on macOS, left of the bar's first button.
const TRAFFIC_LIGHTS_WIDTH: f32 = 80.;
/// Tabs keep their names readable, and many of them scroll rather than shrink.
const TAB_MIN_WIDTH: f32 = 120.;
const TAB_MAX_WIDTH: f32 = 240.;

impl Workspace {
    pub(super) fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, border, muted) = (
            theme.title_bar,
            theme.title_bar_border,
            theme.muted_foreground,
        );
        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .map(|open| self.render_tab(open, cx).into_any_element())
            .collect();
        h_flex()
            .id("top-bar")
            .flex_none()
            .w_full()
            .h(px(HEIGHT))
            .bg(bg)
            .border_b_1()
            .border_color(border)
            .pl(px(if cfg!(target_os = "macos") {
                TRAFFIC_LIGHTS_WIDTH
            } else {
                8.
            }))
            .child(
                Button::new("open-connections")
                    .cursor_pointer()
                    .outline()
                    .small()
                    .flex_none()
                    .label("Connections")
                    .when_some(Keystroke::parse(CONNECTIONS_KEY).ok(), |button, key| {
                        button.child(Kbd::new(key).text_color(muted))
                    })
                    .tooltip("Open a saved connection")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_connections(window, cx);
                    })),
            )
            .child(div().flex_none().w_px().h(px(26.)).mx_2().bg(border))
            .child(
                h_flex()
                    .id("tabs")
                    .h_full()
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tabs)
                    .child(
                        Button::new("new-tab")
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .flex_none()
                            .mx_1()
                            .icon(IconName::Plus)
                            .tooltip("Open another connection")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_connections(window, cx);
                            })),
                    ),
            )
            // Moves the window. It must stay empty.
            .child(drag_area().flex_1().min_w(px(24.)).h_full())
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .pr_2()
                    .children(self.render_update(cx))
                    .child(
                        Button::new("toggle-chat")
                            .cursor_pointer()
                            .outline()
                            .small()
                            .selected(self.chat_open)
                            .icon(logos::agent())
                            .label("Ask Claude")
                            .tooltip("Ask Claude Code about your databases")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_chat(window, cx);
                            })),
                    )
                    .child(
                        Button::new("open-settings")
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .icon(IconName::Settings)
                            .tooltip("Settings")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(SettingsPage::General, window, cx);
                            })),
                    )
                    .child(self.render_mcp_status(cx)),
            )
    }

    fn render_tab(&self, open: &OpenTab, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = open.tab.read(cx);
        let (adapter, light) = (tab.connection().adapter, tab.light());
        let name = open.name.clone();
        let active = self.active.as_deref() == Some(name.as_str());
        let theme = cx.theme();
        let dot = match light {
            Light::Connecting => theme.warning,
            Light::Connected => theme.success,
            Light::Off => theme.muted_foreground,
        };
        let (border, hover, primary) =
            (theme.title_bar_border, theme.secondary_hover, theme.primary);
        let (fg, active_bg, muted) = (theme.foreground, theme.background, theme.muted_foreground);
        let (shown, closing) = (name.clone(), name.clone());
        h_flex()
            .id(SharedString::from(format!("tab-{name}")))
            .test_support()
            .relative()
            .flex_none()
            .h_full()
            .min_w(px(TAB_MIN_WIDTH))
            .max_w(px(TAB_MAX_WIDTH))
            .gap_2()
            .pl_3()
            .pr_1()
            .border_r_1()
            .border_color(border)
            .cursor_pointer()
            .text_sm()
            .map(|el| {
                if active {
                    el.bg(active_bg).text_color(fg).font_semibold()
                } else {
                    el.text_color(muted).hover(move |s| s.bg(hover))
                }
            })
            .when(active, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(2.))
                        .bg(primary),
                )
            })
            .child(logos::icon(adapter, 18.))
            .child(div().flex_1().min_w_0().truncate().child(name.clone()))
            .child(
                div()
                    .id(SharedString::from(format!("tab-status-{name}")))
                    .test_support()
                    .flex_none()
                    .size(px(8.))
                    .rounded_full()
                    .bg(dot),
            )
            .child(
                Button::new(SharedString::from(format!("tab-close-{name}")))
                    .cursor_pointer()
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Disconnect and close")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_tab(closing.clone(), cx);
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.activate_tab(&shown, window, cx);
            }))
    }

    fn render_update(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (danger, muted, track, fill) = {
            let theme = cx.theme();
            (
                theme.danger,
                theme.muted_foreground,
                theme.border,
                theme.primary,
            )
        };
        Some(match self.update_state.as_ref()? {
            UpdateState::Available { version, error } => {
                let label = format!("Update to {version}");
                let version = version.clone();
                h_flex()
                    .gap_2()
                    .when_some(error.clone(), |el, error| {
                        el.child(
                            div()
                                .max_w(px(240.))
                                .truncate()
                                .text_xs()
                                .text_color(danger)
                                .child(error.clone()),
                        )
                    })
                    .child(
                        Button::new("download-update")
                            .cursor_pointer()
                            .primary()
                            .small()
                            .label(label)
                            .tooltip("Downloads in the background while you keep working")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.download_update(version.clone(), cx);
                            })),
                    )
                    .into_any_element()
            }
            UpdateState::Downloading { version, percent } => {
                let percent = percent.load(Ordering::Relaxed);
                let label = if percent == 0 {
                    format!("Downloading {version}…")
                } else {
                    format!("Downloading {version}… {percent}%")
                };
                v_flex()
                    .id("update-progress")
                    .test_support()
                    .w(px(160.))
                    .gap_1()
                    .child(div().text_xs().text_color(muted).truncate().child(label))
                    .child(
                        div().h(px(4.)).w_full().rounded_full().bg(track).child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(fill)
                                .w(relative(f32::from(percent) / 100.)),
                        ),
                    )
                    .into_any_element()
            }
            UpdateState::Ready { download, .. } => Button::new("restart-to-update")
                .cursor_pointer()
                .primary()
                .small()
                .label("Restart to update")
                .tooltip(format!(
                    "Closes TupleBase, installs {} and reopens",
                    download.version
                ))
                .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx)))
                .into_any_element(),
        })
    }

    fn render_mcp_status(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let dot = if self.mcp_servers.is_empty() {
            cx.theme().muted_foreground
        } else {
            cx.theme().success
        };
        Button::new("mcp-status")
            .cursor_pointer()
            .ghost()
            .small()
            .tooltip(mcp_status_text(&self.mcp_servers))
            .child(
                h_flex()
                    .gap_1p5()
                    .child(div().size(px(8.)).rounded_full().bg(dot))
                    .child("MCP"),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_settings(SettingsPage::Agents, window, cx);
            }))
    }
}

/// Empty region that moves the window, as a title bar does. It must stay empty: a
/// button inside it would start a window move instead of a click.
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
