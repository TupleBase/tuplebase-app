//! The Settings dialog: a list of pages on the left, the chosen page's titled groups of
//! settings on the right, and a search over them all.
//!
//! A new setting is one more item in a group; a new area is one more page.
//!
//! The app draws all of it. `gpui-component`'s own `Settings` draws its list rows and
//! their expand caret with the arrow and takes no cursor from the app, and it renders a
//! page only together with that list, so the pages are laid out here the way it lays
//! them out.

use std::collections::HashSet;

use gpui_kit::component::button::*;
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::group_box::{GroupBox, GroupBoxVariant, GroupBoxVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::agents::AgentState;
use tuplebase_core::config::Appearance;

use super::Workspace;
use crate::dialogs;

/// The pages of Settings, in the order the list shows them.
#[derive(Clone, Copy)]
pub(super) enum SettingsPage {
    General,
    Agents,
}

/// A page no wider than this lays each setting out under its title.
const STACKED_MAX_WIDTH: Pixels = px(480.);

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
        let workspace = cx.entity();
        let settings = cx.new(|cx| SettingsDialog::new(&workspace, page, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            // The pages bring their own titles and padding.
            dialog
                .w(px(760.))
                .p_0()
                .close_button(false)
                .child(dialogs::with_close_button(settings.clone()).h(px(520.)))
        });
    }
}

pub(super) struct SettingsDialog {
    workspace: WeakEntity<Workspace>,
    search: Entity<InputState>,
    page: usize,
    /// The group chosen in the list, or the one at the top of the page as it scrolls.
    /// `None` marks the page itself.
    group: Option<usize>,
    /// Pages whose groups are listed under them.
    open: HashSet<usize>,
    /// One per page, so that each keeps its place.
    scroll: Vec<ScrollHandle>,
    /// Scroll to `group` in the next frame, or to the top without one.
    jump: bool,
    /// Set by the wheel: the list follows the page as it scrolls, until a row is clicked.
    following: bool,
    /// The page's scroll offset when last rendered, to follow only a real move.
    seen_offset: Option<Pixels>,
    _subscriptions: [Subscription; 2],
}

impl SettingsDialog {
    fn new(
        workspace: &Entity<Workspace>,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search..."));
        let searched = cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.following = false;
                this.jump = true;
                cx.notify();
            }
        });
        // The pages show the workspace's settings as they change.
        let changed = cx.observe(workspace, |_, _, cx| cx.notify());
        Self {
            workspace: workspace.downgrade(),
            search,
            page: page as usize,
            group: None,
            open: HashSet::from([SettingsPage::General as usize]),
            scroll: Vec::new(),
            jump: false,
            following: false,
            seen_offset: None,
            _subscriptions: [searched, changed],
        }
    }

    fn scroll(&mut self, page: usize) -> ScrollHandle {
        if self.scroll.len() <= page {
            self.scroll.resize_with(page + 1, ScrollHandle::new);
        }
        self.scroll[page].clone()
    }

    fn select(&mut self, page: usize, group: Option<usize>, cx: &mut Context<Self>) {
        self.page = page;
        self.group = group;
        self.jump = group.is_some();
        self.following = false;
        self.seen_offset = None;
        cx.notify();
    }

    /// Keeps the selection on a page that has matches, as the search narrows them. With
    /// no matches at all it stays, for when the search is cleared.
    fn keep_selection_visible(&mut self, shown: &[Vec<usize>]) {
        let visible = |ix: usize| !shown[ix].is_empty();
        let page = if visible(self.page) {
            Some(self.page)
        } else {
            (0..shown.len()).find(|&ix| visible(ix))
        };
        let Some(page) = page else {
            return;
        };
        if page != self.page {
            self.page = page;
            self.group = None;
            self.seen_offset = None;
        } else if self.group.is_some_and(|ix| !shown[page].contains(&ix)) {
            self.group = None;
        }
    }

    /// Marks in the list the group the user scrolled to, like a table of contents.
    fn follow_scroll(&mut self, page: &Page, shown: &[usize], scroll: &ScrollHandle) {
        let offset = scroll.offset().y;
        let moved = self.seen_offset.is_some_and(|seen| seen != offset);
        self.seen_offset = Some(offset);
        if !self.following || !moved || shown.is_empty() {
            return;
        }
        let at_end = offset <= -scroll.max_offset().y + px(1.);
        self.group = group_at_top(page, shown, self.group, scroll.top_item(), at_end);
    }

    fn render_list(
        &self,
        pages: &[Page],
        shown: &[Vec<usize>],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let rows = (0..pages.len())
            .filter(|&ix| !shown[ix].is_empty())
            .map(|page_ix| {
                let page = &pages[page_ix];
                let groups: Vec<usize> = shown[page_ix]
                    .iter()
                    .copied()
                    .filter(|&ix| shown[page_ix].len() > 1 && page.groups[ix].title.is_some())
                    .collect();
                let has_groups = !groups.is_empty();
                let is_open = has_groups && self.open.contains(&page_ix);
                let active =
                    self.page == page_ix && (self.group.is_none() || shown[page_ix].len() == 1);
                let caret = has_groups.then(|| {
                    caret(page.id, page.title, is_open).on_click(cx.listener(
                        move |this, _, _, cx| {
                            // Only opens or closes the list, without choosing the page.
                            cx.stop_propagation();
                            if !this.open.remove(&page_ix) {
                                this.open.insert(page_ix);
                            }
                            cx.notify();
                        },
                    ))
                });
                let row = list_row(format!("settings-page-{}", page.id), active, cx)
                    .child(Icon::new(page.icon.clone()))
                    .child(row_label(page.title))
                    .children(caret)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open.insert(page_ix);
                        this.select(page_ix, None, cx);
                    }));
                let sub_rows = groups.into_iter().map(|group_ix| {
                    let title = page.groups[group_ix].title.unwrap_or_default();
                    let active = self.page == page_ix && self.group == Some(group_ix);
                    list_row(format!("settings-group-{}-{group_ix}", page.id), active, cx)
                        .child(row_label(title))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select(page_ix, Some(group_ix), cx);
                        }))
                        .test_support()
                });
                v_flex()
                    .w_full()
                    .child(row.test_support())
                    .when(is_open, |this| {
                        this.child(
                            v_flex()
                                .border_l_1()
                                .border_color(theme.sidebar_border)
                                .gap_1()
                                .ml_3p5()
                                .pl_2p5()
                                .py_0p5()
                                .children(sub_rows),
                        )
                    })
            });
        v_flex()
            .id("settings-sidebar")
            .size_full()
            .overflow_hidden()
            .rounded_l(theme.radius_lg)
            .bg(theme.tokens.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(
                div().pt_3().px_3().child(
                    Input::new(&self.search)
                        .id("settings-search")
                        .prefix(IconName::Search),
                ),
            )
            .child(
                v_flex()
                    .id("settings-pages")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .gap_2()
                    .children(rows),
            )
    }
}

impl Render for SettingsDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(workspace) = self.workspace.upgrade() else {
            return div().into_any_element();
        };
        let pages = [general(&workspace, cx), agents(&workspace, cx)];
        let query = self.search.read(cx).value().to_lowercase();
        let shown: Vec<Vec<usize>> = pages.iter().map(|page| page.matching(&query)).collect();
        self.keep_selection_visible(&shown);

        let page_ix = self.page;
        let scroll = self.scroll(page_ix);
        let groups = &shown[page_ix];
        if self.jump {
            self.jump = false;
            self.seen_offset = None;
            match self
                .group
                .and_then(|ix| groups.iter().position(|&g| g == ix))
            {
                Some(visible_ix) => scroll.scroll_to_top_of_item(visible_ix),
                None => scroll.set_offset(Point::default()),
            }
        }
        self.follow_scroll(&pages[page_ix], groups, &scroll);

        let list = self.render_list(&pages, &shown, cx).into_any_element();
        let content = if !groups.is_empty() {
            let wheel = cx.listener(|this, _: &ScrollEvent, _, cx| {
                this.following = true;
                cx.notify();
            });
            let page = pages
                .into_iter()
                .nth(page_ix)
                .expect("the chosen page exists");
            let groups = groups.clone();
            container_query(move |size, _, cx| {
                page.render(
                    page_ix,
                    &groups,
                    &query,
                    size.width <= STACKED_MAX_WIDTH,
                    &scroll,
                    wheel,
                    cx,
                )
            })
            .into_any_element()
        } else if shown.iter().all(Vec::is_empty) {
            no_results().into_any_element()
        } else {
            div().into_any_element()
        };

        h_resizable("settings")
            .child(
                resizable_panel()
                    .size(px(200.))
                    .size_range(px(160.)..px(360.))
                    .child(list),
            )
            .child(resizable_panel().child(content))
            .into_any_element()
    }
}

/// A row of the page list: the hand, the hover and the highlight of the chosen row.
fn list_row(id: impl Into<ElementId>, active: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    h_flex()
        .id(id.into())
        .role(Role::TreeItem)
        .aria_selected(active)
        .w_full()
        .h_7()
        .flex_shrink_0()
        .overflow_x_hidden()
        .p_2()
        .gap_x_2()
        .rounded(theme.radius)
        .text_sm()
        .cursor_pointer()
        .when(!active, |this| {
            this.hover(|this| {
                this.bg(theme.sidebar_accent.opacity(0.8))
                    .text_color(theme.sidebar_accent_foreground)
            })
        })
        .when(active, |this| {
            this.font_medium()
                .bg(theme.tokens.sidebar_accent)
                .text_color(theme.sidebar_accent_foreground)
        })
}

fn row_label(text: &'static str) -> Div {
    h_flex().flex_1().overflow_x_hidden().child(text)
}

/// The button beside a page that lists its groups under it, or hides them.
fn caret(page_id: &str, title: &str, open: bool) -> Button {
    Button::new(SharedString::from(format!("settings-caret-{page_id}")))
        .xsmall()
        .ghost()
        .cursor_pointer()
        .accessibility_label(format!("Toggle {title} submenu"))
        .icon(
            Icon::new(IconName::ChevronRight)
                .size_4()
                .when(open, |this| this.rotate(percentage(0.25))),
        )
}

fn no_results() -> impl IntoElement {
    div()
        .id("settings-no-results")
        .test_support()
        .size_full()
        .child(
            Empty::new().size_full().header(
                EmptyHeader::new()
                    .media(
                        EmptyMedia::new()
                            .with_variant(EmptyMediaVariant::Icon)
                            .child(Icon::new(IconName::Search)),
                    )
                    .title(EmptyTitle::new().child("No matching settings"))
                    .description(EmptyDescription::new().child("Try a different keyword.")),
            ),
        )
}

/// The titled group the list marks for a scroll position: the one at or above the top
/// of the page. At the end of the page the last groups can't reach the top, so a group
/// chosen below the top stays marked, and the last one is marked otherwise.
fn group_at_top(
    page: &Page,
    shown: &[usize],
    chosen: Option<usize>,
    top: usize,
    at_end: bool,
) -> Option<usize> {
    let chosen_pos = chosen.and_then(|ix| shown.iter().position(|&g| g == ix));
    if at_end && chosen_pos.is_some_and(|pos| pos > top) {
        return chosen;
    }
    let top = if at_end { shown.len() - 1 } else { top };
    shown
        .iter()
        .take(top + 1)
        .rev()
        .copied()
        .find(|&ix| page.groups[ix].title.is_some())
}

struct Page {
    /// Names the page's rows in the list.
    id: &'static str,
    title: &'static str,
    icon: IconName,
    description: Option<&'static str>,
    groups: Vec<Group>,
}

impl Page {
    /// The groups with an item matching `query`, which is lower case.
    fn matching(&self, query: &str) -> Vec<usize> {
        (0..self.groups.len())
            .filter(|&ix| self.groups[ix].items.iter().any(|item| item.matches(query)))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        self,
        page_ix: usize,
        shown: &[usize],
        query: &str,
        stacked: bool,
        scroll: &ScrollHandle,
        on_wheel: impl Fn(&ScrollEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let header = v_flex()
            .p_4()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(h_flex().child(self.title))
            .when_some(self.description, |this, description| {
                this.child(
                    Label::new(description)
                        .text_sm()
                        .text_color(theme.muted_foreground),
                )
            });
        let mut groups: Vec<Option<Group>> = self.groups.into_iter().map(Some).collect();
        let groups = shown.iter().filter_map(|&ix| {
            let group = groups[ix].take()?;
            Some(
                // Horizontal padding inside the scrolled area, so focus rings can reach
                // past a group's edges.
                div()
                    .w_full()
                    .px_4()
                    .child(group.render(ix, query, stacked, cx)),
            )
        });
        v_flex()
            .id(("settings-page", page_ix))
            .size_full()
            .child(header)
            .child(
                div().relative().flex_1().min_h_0().w_full().child(
                    v_flex()
                        .id("settings-page-groups")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(scroll)
                        .on_scroll(on_wheel)
                        .children(groups)
                        .vertical_scrollbar(scroll),
                ),
            )
            .into_any_element()
    }
}

struct Group {
    title: Option<&'static str>,
    items: Vec<Item>,
}

impl Group {
    fn new() -> Self {
        Self {
            title: None,
            items: Vec::new(),
        }
    }

    fn title(mut self, title: &'static str) -> Self {
        self.title = Some(title);
        self
    }

    fn item(mut self, item: Item) -> Self {
        self.items.push(item);
        self
    }

    fn render(self, ix: usize, query: &str, stacked: bool, cx: &App) -> GroupBox {
        GroupBox::new()
            .id(SharedString::from(format!("group-{ix}")))
            .with_variant(GroupBoxVariant::Outline)
            .when_some(self.title, |this, title| {
                this.title(v_flex().gap_1().child(title))
            })
            .gap_4()
            .children(
                self.items
                    .into_iter()
                    .enumerate()
                    .filter(|(_, item)| item.matches(query))
                    .map(|(item_ix, item)| item.render(item_ix, stacked, cx)),
            )
            .py_4()
    }
}

struct Item {
    /// `None` for a note, which shows only while nothing is searched.
    title: Option<SharedString>,
    description: Option<&'static str>,
    /// The description and field go under the title rather than beside it.
    stacked: bool,
    field: AnyElement,
}

impl Item {
    fn new(title: impl Into<SharedString>, field: impl IntoElement) -> Self {
        Self {
            title: Some(title.into()),
            description: None,
            stacked: false,
            field: field.into_any_element(),
        }
    }

    fn note(content: impl IntoElement) -> Self {
        Self {
            title: None,
            description: None,
            stacked: false,
            field: content.into_any_element(),
        }
    }

    fn description(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }

    fn stacked(mut self) -> Self {
        self.stacked = true;
        self
    }

    /// Whether the item shows for `query`, which is lower case.
    fn matches(&self, query: &str) -> bool {
        match &self.title {
            Some(title) => {
                title.to_lowercase().contains(query)
                    || self
                        .description
                        .is_some_and(|text| text.to_lowercase().contains(query))
            }
            None => query.is_empty(),
        }
    }

    fn render(self, ix: usize, stacked: bool, cx: &App) -> Stateful<Div> {
        let row = div().id(SharedString::from(format!("item-{ix}"))).w_full();
        let Some(title) = self.title else {
            return row.child(self.field);
        };
        let stacked = stacked || self.stacked;
        row.child(
            div()
                .w_full()
                .map(|this| {
                    if stacked {
                        this.v_flex()
                    } else {
                        this.h_flex().justify_between().items_center()
                    }
                })
                .gap_3()
                .child(
                    v_flex()
                        .map(|this| {
                            if stacked {
                                this.w_full()
                            } else {
                                this.flex_1().max_w_3_5()
                            }
                        })
                        .child(Label::new(title).text_sm())
                        .when_some(self.description, |this, description| {
                            this.child(
                                div()
                                    .size_full()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(description),
                            )
                        }),
                )
                .child(div().id("field").child(self.field)),
        )
    }
}

fn general(workspace: &Entity<Workspace>, cx: &App) -> Page {
    const CHECK_UPDATES: &str = "Check for updates at startup";
    let config = &workspace.read(cx).config;
    let chosen = config.appearance;
    let appearance =
        ButtonGroup::new("appearance")
            .outline()
            .small()
            .children(Appearance::ALL.map(|option| {
                let workspace = workspace.clone();
                Button::new(SharedString::from(format!(
                    "appearance-{}",
                    option.label().to_lowercase()
                )))
                .cursor_pointer()
                .label(option.label())
                .selected(option == chosen)
                .on_click(move |_, window, cx| {
                    workspace.update(cx, |this, cx| this.set_appearance(option, window, cx));
                })
            }));
    let updates = {
        let workspace = workspace.clone();
        Switch::new("check-updates")
            .cursor_pointer()
            .accessibility_label(CHECK_UPDATES)
            .checked(config.check_for_updates)
            .on_click(move |checked: &bool, _, cx| {
                workspace.update(cx, |this, cx| this.set_check_for_updates(*checked, cx));
            })
    };
    let clear_history = {
        let workspace = workspace.clone();
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
    };
    let version = div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(concat!("Version ", env!("CARGO_PKG_VERSION")));
    Page {
        id: "general",
        title: "General",
        icon: IconName::Settings,
        description: None,
        groups: vec![
            Group::new().title("Appearance").item(
                Item::new("Theme", appearance)
                    .description("System follows your computer's light or dark setting.")
                    // Under the description, so the three buttons never run past the card.
                    .stacked(),
            ),
            Group::new()
                .title("Updates")
                .item(Item::new(CHECK_UPDATES, updates).description(
                    "Asks GitHub whether a newer version exists. \
                     Sends no connection details or queries.",
                )),
            Group::new().title("Query history").item(
                Item::new("Clear query history", clear_history)
                    .description("Your queries and unsaved SQL are kept on this computer only."),
            ),
            Group::new()
                .title("About")
                .item(Item::new(tuplebase_core::PRODUCT_NAME, version)),
        ],
    }
}

fn agents(workspace: &Entity<Workspace>, cx: &App) -> Page {
    let statuses = workspace.read(cx).agents.clone();
    let looked = statuses.is_some();
    let statuses = statuses.unwrap_or_default();
    let mut group = Group::new();
    if statuses.is_empty() {
        let note = if looked {
            "No supported agents found on this computer."
        } else {
            "Looking for agents…"
        };
        group = group.item(Item::note(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(note),
        ));
    }
    for status in statuses {
        let name = status.name;
        let (text, failed, can_add): (SharedString, bool, bool) = match status.state {
            AgentState::Added => ("Added".into(), false, false),
            AgentState::NotAdded => ("Not added".into(), false, true),
            AgentState::Failed(reason) => (reason.into(), true, true),
        };
        let theme = cx.theme();
        let color = if failed {
            theme.danger
        } else {
            theme.muted_foreground
        };
        let workspace = workspace.downgrade();
        let field = h_flex()
            .gap_3()
            .child(
                div()
                    .max_w(px(280.))
                    .text_sm()
                    .text_right()
                    .text_color(color)
                    .child(text),
            )
            .when(can_add, |row| {
                row.child(
                    Button::new(SharedString::from(format!("add-mcp-{name}")))
                        .cursor_pointer()
                        .small()
                        .label(if failed { "Try again" } else { "Add" })
                        .accessibility_label(format!("Add to {name}"))
                        .on_click(move |_, _, cx| {
                            workspace
                                .update(cx, |this, cx| this.add_mcp_to_agent(name, cx))
                                .ok();
                        }),
                )
            });
        group = group.item(Item::new(name, field));
    }
    Page {
        id: "agents",
        title: "AI agents",
        icon: IconName::Bot,
        description: Some(
            "Adds the MCP server to your agents so they can query saved connections. \
             They stay read-only unless you allow writes.",
        ),
        groups: vec![group],
    }
}

#[cfg(test)]
mod tests {
    // No globs from gpui_kit here: its `test` macro would shadow the built-in `#[test]`.
    use super::super::tests::{open, window_action};
    use super::{SettingsPage, caret, list_row};
    use crate::workspace::Workspace;
    use gpui_kit::component::button::Button;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, CursorStyle, Entity, Styled as _, TestAppContext};
    use tuplebase_core::agents::{AgentState, AgentStatus};
    use tuplebase_core::config::Config;

    fn open_settings(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Workspace>) {
        let (handle, workspace) = open(Config::default(), cx);
        window_action(handle, cx, |window, cx| {
            workspace.update(cx, |this, cx| {
                this.open_settings(SettingsPage::General, window, cx)
            });
        });
        workspace.update(cx, |this, cx| {
            this.agents = Some(vec![AgentStatus {
                name: "Cursor",
                state: AgentState::NotAdded,
            }]);
            cx.notify();
        });
        (handle, workspace)
    }

    fn search(handle: AnyWindowHandle, text: &str, cx: &mut TestAppContext) {
        window_action(handle, cx, |window, cx| window.click("settings-search", cx));
        window_action(handle, cx, |window, cx| {
            window.press("secondary-a", cx);
            window.press("backspace", cx);
            if !text.is_empty() {
                window.input(text, cx);
            }
        });
    }

    /// The test window doesn't report the cursor it shows, so this checks the style the
    /// list's rows and caret ask for.
    #[gpui_kit::test]
    fn the_page_list_shows_the_hand(cx: &mut TestAppContext) {
        let _ = open(Config::default(), cx);
        let hand = Some(CursorStyle::PointingHand);
        cx.update(|cx| {
            assert_eq!(list_row("page", false, cx).style().mouse_cursor, hand);
            assert_eq!(list_row("page", true, cx).style().mouse_cursor, hand);
        });
        let mut caret: Button = caret("general", "General", true);
        assert_eq!(caret.style().mouse_cursor, hand);
    }

    #[gpui_kit::test]
    fn clicking_each_page_shows_it_and_marks_it(cx: &mut TestAppContext) {
        let (handle, _) = open_settings(cx);
        window_action(handle, cx, |window, cx| {
            assert!(window.find("check-updates").visible());
            assert!(window.try_find("add-mcp-Cursor").is_none());
            assert_eq!(window.find("settings-page-general").selected(), Some(true));
            assert_eq!(window.find("settings-page-agents").selected(), Some(false));
            window.click("settings-page-agents", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("add-mcp-Cursor").visible());
            assert!(window.try_find("check-updates").is_none());
            assert_eq!(window.find("settings-page-general").selected(), Some(false));
            assert_eq!(window.find("settings-page-agents").selected(), Some(true));
            window.click("settings-page-general", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("check-updates").visible());
            assert!(window.try_find("add-mcp-Cursor").is_none());
            assert_eq!(window.find("settings-page-general").selected(), Some(true));
        });
    }

    #[gpui_kit::test]
    fn a_group_in_the_list_is_marked_instead_of_its_page(cx: &mut TestAppContext) {
        let (handle, _) = open_settings(cx);
        window_action(handle, cx, |window, cx| {
            assert_eq!(
                window.find("settings-group-general-3").selected(),
                Some(false)
            );
            window.click("settings-group-general-3", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert_eq!(
                window.find("settings-group-general-3").selected(),
                Some(true)
            );
            assert_eq!(window.find("settings-page-general").selected(), Some(false));
            // Choosing the page again marks the page.
            window.click("settings-page-general", cx);
        });
        window_action(handle, cx, |window, _| {
            assert_eq!(window.find("settings-page-general").selected(), Some(true));
            assert_eq!(
                window.find("settings-group-general-3").selected(),
                Some(false)
            );
        });
    }

    #[gpui_kit::test]
    fn the_caret_hides_and_shows_a_pages_groups_without_choosing_it(cx: &mut TestAppContext) {
        let (handle, _) = open_settings(cx);
        window_action(handle, cx, |window, cx| {
            window.click("settings-page-agents", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.find("settings-group-general-0").visible());
            window.click("settings-caret-general", cx);
        });
        window_action(handle, cx, |window, cx| {
            assert!(window.try_find("settings-group-general-0").is_none());
            // Still on the agents page.
            assert_eq!(window.find("settings-page-agents").selected(), Some(true));
            window.click("settings-caret-general", cx);
        });
        window_action(handle, cx, |window, _| {
            assert!(window.find("settings-group-general-0").visible());
        });
    }

    #[gpui_kit::test]
    fn the_search_narrows_pages_and_groups(cx: &mut TestAppContext) {
        let (handle, _) = open_settings(cx);
        search(handle, "clear", cx);
        window_action(handle, cx, |window, _| {
            assert!(window.find("clear-history").visible());
            assert!(window.try_find("check-updates").is_none());
            assert!(window.try_find("settings-page-agents").is_none());
            // One group left: the page lists none under it.
            assert!(window.try_find("settings-group-general-2").is_none());
        });
        search(handle, "cursor", cx);
        window_action(handle, cx, |window, _| {
            // The page with the only match is shown.
            assert!(window.find("add-mcp-Cursor").visible());
            assert!(window.try_find("settings-page-general").is_none());
        });
        search(handle, "no such setting", cx);
        window_action(handle, cx, |window, _| {
            assert!(window.find("settings-no-results").visible());
        });
        search(handle, "", cx);
        window_action(handle, cx, |window, _| {
            assert!(window.try_find("settings-no-results").is_none());
            assert!(window.find("settings-page-general").visible());
            assert!(window.find("settings-page-agents").visible());
            // Typing "no such setting" passed through "n", which only General matches.
            assert!(window.find("check-updates").visible());
        });
    }
}
