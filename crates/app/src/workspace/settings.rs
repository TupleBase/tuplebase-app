//! The Settings dialog: a sidebar of pages, each with titled groups of settings.
//!
//! A new setting is one more item in a group; a new area is one more page.

use gpui_kit::component::button::*;
use gpui_kit::component::group_box::GroupBoxVariant;
use gpui_kit::component::setting::{
    SelectIndex, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::*;
use tuplebase_core::agents::AgentState;

use super::Workspace;

/// The pages of Settings, in the order `open_settings` lists them.
#[derive(Clone, Copy)]
pub(super) enum SettingsPage {
    General,
    Agents,
}

impl Workspace {
    /// Shows Settings in a dialog, leaving whatever is on screen in place underneath.
    pub(super) fn open_settings(
        &self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.refresh_agents(cx);
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let Some(workspace) = this.upgrade() else {
                return dialog;
            };
            // The dialog is rounded and the sidebar fills its left edge.
            let sidebar = StyleRefinement::default().rounded_l(cx.theme().radius_lg);
            // The pages bring their own titles and padding.
            dialog.w(px(760.)).p_0().child(
                div().h(px(520.)).child(
                    Settings::new("settings")
                        .with_group_variant(GroupBoxVariant::Outline)
                        .sidebar_width(px(200.))
                        .sidebar_style(&sidebar)
                        .default_selected_index(SelectIndex {
                            page_ix: page as usize,
                            group_ix: None,
                        })
                        .pages([general(&workspace), agents(&workspace, cx)]),
                ),
            )
        });
    }
}

fn general(workspace: &Entity<Workspace>) -> SettingPage {
    const CHECK_UPDATES: &str = "Check for updates at startup";
    let updates = workspace.clone();
    let history = workspace.clone();
    SettingPage::new("General")
        .icon(IconName::Settings)
        .default_open(true)
        .resettable(false)
        .group(
            SettingGroup::new().title("Updates").item(
                SettingItem::new(
                    CHECK_UPDATES,
                    SettingField::render(move |_, _, cx| {
                        let workspace = updates.clone();
                        Switch::new("check-updates")
                            .accessibility_label(CHECK_UPDATES)
                            .checked(workspace.read(cx).config.check_for_updates)
                            .on_click(move |checked: &bool, _, cx| {
                                workspace.update(cx, |this, cx| {
                                    this.set_check_for_updates(*checked, cx);
                                });
                            })
                    }),
                )
                .description(
                    "Asks GitHub whether a newer version exists. \
                     Sends no connection details or queries.",
                ),
            ),
        )
        .group(
            SettingGroup::new().title("Query history").item(
                SettingItem::new(
                    "Clear query history",
                    SettingField::render(move |_, _, _| {
                        let workspace = history.clone();
                        Button::new("clear-history")
                            .cursor_pointer()
                            .small()
                            .label("Clear")
                            .on_click(move |_, _, cx| {
                                workspace.update(cx, |this, cx| {
                                    this.session.history.clear();
                                    this.schedule_session_save(cx);
                                    cx.notify();
                                });
                            })
                    }),
                )
                .description("Your queries and unsaved SQL are kept on this computer only."),
            ),
        )
        .group(SettingGroup::new().title("About").item(SettingItem::new(
            tuplebase_core::PRODUCT_NAME,
            SettingField::render(|_, _, cx| {
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(concat!("Version ", env!("CARGO_PKG_VERSION")))
            }),
        )))
}

fn agents(workspace: &Entity<Workspace>, cx: &App) -> SettingPage {
    let statuses = workspace.read(cx).agents.clone();
    let looked = statuses.is_some();
    let statuses = statuses.unwrap_or_default();
    let can_add = statuses.iter().any(|s| s.state != AgentState::Added);
    let mut group = SettingGroup::new();
    if statuses.is_empty() {
        let note = if looked {
            "No supported agents found on this computer."
        } else {
            "Looking for agents…"
        };
        group = group.item(SettingItem::render(move |_, _, cx| {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(note)
        }));
    }
    group = group.items(statuses.into_iter().map(|status| {
        let (text, failed): (SharedString, bool) = match status.state {
            AgentState::Added => ("Added".into(), false),
            AgentState::NotAdded => ("Not added".into(), false),
            AgentState::Failed(reason) => (reason.into(), true),
        };
        SettingItem::new(
            status.name,
            SettingField::render(move |_, _, cx| {
                let theme = cx.theme();
                let color = if failed {
                    theme.danger
                } else {
                    theme.muted_foreground
                };
                div()
                    .max_w(px(280.))
                    .text_sm()
                    .text_right()
                    .text_color(color)
                    .child(text.clone())
            }),
        )
    }));
    if can_add {
        let workspace = workspace.downgrade();
        group = group.footer(move |_, _| {
            let workspace = workspace.clone();
            h_flex().child(
                Button::new("add-mcp")
                    .cursor_pointer()
                    .small()
                    .label("Add to all agents")
                    .on_click(move |_, window, cx| {
                        workspace
                            .update(cx, |this, cx| this.add_mcp_to_agents(window, cx))
                            .ok();
                    }),
            )
        });
    }
    SettingPage::new("AI agents")
        .icon(IconName::Bot)
        .resettable(false)
        .description(
            "Adds the MCP server to your agents so they can query saved connections. \
             They stay read-only unless you allow writes.",
        )
        .group(group)
}
