//! Finding a database in AWS: choose a profile and regions, search, pick one. What it
//! finds fills in the connection form; core does the AWS work (`cloud::aws`).
//!
//! The controls stay at the top and the list scrolls in its own area below them, so the
//! form's buttons under it stay in view. A search box filters the list; Up, Down and
//! Enter in it move through the list and use the selected database.

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::cloud::aws::{self, Profile};
use tuplebase_core::cloud::{self, DiscoverError, Discovered, Discovery};
use tuplebase_core::config::{Auth, Provider};
use tuplebase_core::state::CloudSearch;

use crate::logos;
use crate::runtime::io;
use crate::tree_nav;

/// Where the search field gives Up, Down, Enter and Escape to the list.
const SEARCH_CONTEXT: &str = "AwsSearch";
/// One line per database.
const ROW_HEIGHT: f32 = 34.;

actions!(aws_picker, [ResultUp, ResultDown, UseResult, ClearSearch]);

/// Up and Down in the search field move through the list, Enter uses the selected
/// database and Escape empties the search. Bound after `gpui_kit::init`, so they win over
/// the field's own bindings.
pub(crate) fn key_bindings() -> [KeyBinding; 4] {
    let search = Some("AwsSearch > Input");
    [
        KeyBinding::new("up", ResultUp, search),
        KeyBinding::new("down", ResultDown, search),
        KeyBinding::new("enter", UseResult, search),
        KeyBinding::new("escape", ClearSearch, search),
    ]
}

pub enum AwsPickerEvent {
    /// Fill in the form with this database.
    Use(Box<Discovered>),
    /// Remember where this search looked.
    Searched(CloudSearch),
}

/// The profiles on this machine.
enum Profiles {
    Loading,
    Loaded(Vec<Profile>),
    Failed(SharedString),
}

enum Search {
    Idle,
    Searching,
    Done(Discovery),
}

/// A line of the list: a region's heading, or a database by its index in the results.
#[derive(Debug, Clone, PartialEq)]
enum Line {
    Region(String, usize),
    Found(usize),
}

pub struct AwsPicker {
    profiles: Profiles,
    /// The chosen profile; empty means the default credentials.
    profile: String,
    /// A profile typed in, which wins over the list.
    typed_profile: Entity<InputState>,
    /// Regions, separated by commas or spaces.
    regions: Entity<InputState>,
    /// Filters the results.
    query: Entity<InputState>,
    search: Search,
    /// Index into the results, whatever the search shows.
    selected: Option<usize>,
    /// Whether the notices about places that couldn't be searched are open.
    notices_open: bool,
    list_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AwsPickerEvent> for AwsPicker {}

impl AwsPicker {
    /// Starts where the last search looked, and reads the profiles on this machine.
    pub fn new(last: CloudSearch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let typed_profile =
            cx.new(|cx| InputState::new(window, cx).placeholder("Or type a profile name"));
        let regions = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("eu-west-1, us-east-1")
                .default_value(last.regions.join(", "))
        });
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search by name, engine, region or endpoint")
        });
        // Enter in the profile or regions field searches.
        let find_on_enter = |this: &mut Self,
                             _: &Entity<InputState>,
                             event: &InputEvent,
                             window: &mut Window,
                             cx: &mut Context<Self>| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.find(window, cx);
            }
        };
        let subscriptions = vec![
            cx.subscribe_in(&typed_profile, window, find_on_enter),
            cx.subscribe_in(&regions, window, find_on_enter),
            cx.subscribe_in(&query, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.query_changed(cx);
                }
            }),
        ];
        cx.spawn_in(window, async move |this, cx| {
            let result = io(aws::profiles()).await.map_err(|e| format!("{e:#}"));
            this.update_in(cx, |this, window, cx| this.set_profiles(result, window, cx))
                .ok();
        })
        .detach();
        Self {
            profiles: Profiles::Loading,
            profile: last.profile,
            typed_profile,
            regions,
            query,
            search: Search::Idle,
            selected: None,
            notices_open: false,
            list_scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Shows the profiles once they are read. With no regions yet, takes the chosen
    /// profile's.
    pub fn set_profiles(
        &mut self,
        result: Result<Vec<Profile>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.profiles = match result {
            Ok(profiles) => Profiles::Loaded(profiles),
            Err(e) => Profiles::Failed(e.into()),
        };
        // A remembered profile that is gone is still offered: AWS reports it if it fails.
        self.fill_region(window, cx);
        cx.notify();
    }

    /// Shows what a search found.
    pub fn set_discovery(&mut self, discovery: Discovery, cx: &mut Context<Self>) {
        self.search = Search::Done(discovery);
        self.notices_open = false;
        self.selected = None;
        self.select_first_shown(cx);
        cx.notify();
    }

    /// Whether a database is chosen and can be used.
    pub fn can_use(&self, cx: &App) -> bool {
        self.chosen(cx).is_some()
    }

    /// The chosen database, if the search shows it and it can be used.
    fn chosen(&self, cx: &App) -> Option<&Discovered> {
        let Search::Done(discovery) = &self.search else {
            return None;
        };
        self.selected
            .filter(|i| self.choosable(cx).contains(i))
            .and_then(|i| discovery.found.get(i))
    }

    /// Fills in the form with the chosen database.
    pub fn use_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(found) = self.chosen(cx) {
            cx.emit(AwsPickerEvent::Use(Box::new(found.clone())));
        }
    }

    /// Puts the keys in the search field, once there are results to search.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.search, Search::Done(_)) {
            self.query.update(cx, |input, cx| input.focus(window, cx));
        } else {
            self.regions.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// The selected result's index, for tests.
    #[cfg(test)]
    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// Whether one of its fields has the keys, or nothing does.
    fn has_focus(&self, window: &Window, cx: &App) -> bool {
        window.focused(cx).is_none()
            || [&self.typed_profile, &self.regions, &self.query]
                .iter()
                .any(|input| input.read(cx).focus_handle(cx).is_focused(window))
    }

    fn query_text(&self, cx: &App) -> String {
        self.query.read(cx).value().trim().to_owned()
    }

    /// The results the search shows, by index, by region then name.
    fn shown(&self, cx: &App) -> Vec<usize> {
        let Search::Done(discovery) = &self.search else {
            return Vec::new();
        };
        let query = self.query_text(cx);
        let mut shown: Vec<usize> = (0..discovery.found.len())
            .filter(|&i| discovery.found[i].matches(&query))
            .collect();
        shown.sort_by(|&a, &b| {
            let (a, b) = (&discovery.found[a], &discovery.found[b]);
            (&a.source.region, &a.label).cmp(&(&b.source.region, &b.label))
        });
        shown
    }

    /// The shown results that can be chosen, in the list's order.
    fn choosable(&self, cx: &App) -> Vec<usize> {
        let Search::Done(discovery) = &self.search else {
            return Vec::new();
        };
        self.shown(cx)
            .into_iter()
            .filter(|&i| discovery.found[i].usable())
            .collect()
    }

    /// The list's lines: with more than one region shown, each region's results under
    /// its heading.
    fn lines(&self, shown: &[usize]) -> Vec<Line> {
        let Search::Done(discovery) = &self.search else {
            return Vec::new();
        };
        let region = |i: usize| &discovery.found[i].source.region;
        let grouped = shown.windows(2).any(|w| region(w[0]) != region(w[1]));
        let mut lines = Vec::new();
        for (n, &i) in shown.iter().enumerate() {
            if grouped && (n == 0 || region(shown[n - 1]) != region(i)) {
                let count = shown.iter().filter(|&&j| region(j) == region(i)).count();
                lines.push(Line::Region(region(i).clone(), count));
            }
            lines.push(Line::Found(i));
        }
        lines
    }

    /// Keeps a shown selection; otherwise selects the first database the search shows.
    fn select_first_shown(&mut self, cx: &App) {
        let choosable = self.choosable(cx);
        if self.selected.is_some_and(|i| choosable.contains(&i)) {
            return;
        }
        self.selected = choosable.first().copied();
        self.list_scroll.set_offset(point(px(0.), px(0.)));
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        self.select_first_shown(cx);
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let choosable = self.choosable(cx);
        let current = self
            .selected
            .and_then(|i| choosable.iter().position(|&j| j == i));
        let Some(next) = tree_nav::step(choosable.len(), current, delta) else {
            return;
        };
        let index = choosable[next];
        self.selected = Some(index);
        let lines = self.lines(&self.shown(cx));
        if let Some(line) = lines.iter().position(|l| *l == Line::Found(index)) {
            self.list_scroll.scroll_to_item(line);
        }
        cx.notify();
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.query_text(cx).is_empty() {
            // Nothing to clear: let Escape go on.
            cx.propagate();
            return;
        }
        self.query
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.query_changed(cx);
    }

    fn profile_region(&self, name: &str) -> Option<String> {
        let Profiles::Loaded(profiles) = &self.profiles else {
            return None;
        };
        let name = if name.is_empty() { "default" } else { name };
        profiles
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.region.clone())
    }

    /// Fills an empty regions field with the chosen profile's region.
    fn fill_region(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.regions.read(cx).value().trim().is_empty() {
            return;
        }
        if let Some(region) = self.profile_region(&self.chosen_profile(cx)) {
            self.regions
                .update(cx, |input, cx| input.set_value(region, window, cx));
        }
    }

    fn chosen_profile(&self, cx: &App) -> String {
        let typed = self.typed_profile.read(cx).value().trim().to_string();
        if typed.is_empty() {
            self.profile.clone()
        } else {
            typed
        }
    }

    fn region_list(&self, cx: &App) -> Vec<String> {
        let text = self.regions.read(cx).value().to_string();
        let mut regions: Vec<String> = Vec::new();
        for region in text.split([',', ' ', '\n']).map(str::trim) {
            if !region.is_empty() && !regions.iter().any(|r| r == region) {
                regions.push(region.to_owned());
            }
        }
        regions
    }

    fn choose_profile(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let old_region = self.profile_region(&self.chosen_profile(cx));
        self.profile = name;
        self.typed_profile
            .update(cx, |input, cx| input.set_value("", window, cx));
        // Follow the profile's region unless the user chose others.
        let regions = self.regions.read(cx).value().trim().to_string();
        if old_region.as_deref() == Some(regions.as_str()) {
            self.regions
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.fill_region(window, cx);
        cx.notify();
    }

    fn find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let search = CloudSearch {
            profile: self.chosen_profile(cx),
            regions: self.region_list(cx),
        };
        cx.emit(AwsPickerEvent::Searched(search.clone()));
        self.search = Search::Searching;
        self.selected = None;
        // A filter for the last results could hide all of the new ones.
        self.query
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let found = io(async move {
                let discovery =
                    cloud::discover(Provider::Aws, &search.profile, &search.regions).await;
                Ok(discovery)
            })
            .await;
            this.update_in(cx, |this, window, cx| {
                match found {
                    Ok(discovery) => this.set_discovery(discovery, cx),
                    // Only if the runtime itself failed; discovery reports its own errors.
                    Err(e) => this.set_discovery(
                        Discovery {
                            errors: vec![DiscoverError {
                                region: None,
                                kind: cloud::DiscoverErrorKind::Other,
                                message: format!("{e:#}"),
                                detail: None,
                            }],
                            ..Discovery::default()
                        },
                        cx,
                    ),
                }
                // Ready to type a search, or Enter to use the first one, unless the keys
                // went somewhere else meanwhile.
                if this.has_focus(window, cx) {
                    this.focus(window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_profiles(&self, cx: &mut Context<Self>) -> Div {
        let chosen = self.profile.clone();
        let mut names = vec![String::new()];
        if let Profiles::Loaded(profiles) = &self.profiles {
            names.extend(profiles.iter().map(|p| p.name.clone()));
        }
        // A remembered profile stays visible even if the files no longer list it.
        if !names.contains(&chosen) {
            names.push(chosen.clone());
        }
        let theme = cx.theme();
        let (input_border, foreground) = (theme.input, theme.foreground);
        let chips = names.into_iter().map(|name| {
            let selected = name == chosen;
            let label = if name.is_empty() {
                "Default credentials".to_owned()
            } else {
                name.clone()
            };
            div()
                .id(SharedString::from(format!("aws-profile-{name}")))
                .test_support()
                .px_2()
                .py_1()
                .text_sm()
                .rounded(cx.theme().radius)
                .border_1()
                .border_color(if selected { foreground } else { input_border })
                .cursor_pointer()
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose_profile(name.clone(), window, cx);
                }))
        });
        let status = match &self.profiles {
            Profiles::Loading => Some("Reading your AWS profiles…".to_owned()),
            Profiles::Loaded(profiles) if profiles.is_empty() => Some(
                "No profiles in ~/.aws/config or ~/.aws/credentials: use the default \
                 credentials or type a profile name."
                    .to_owned(),
            ),
            Profiles::Loaded(_) => None,
            Profiles::Failed(e) => Some(format!("Couldn't read your AWS profiles: {e}")),
        };
        v_flex()
            .gap_1p5()
            .child(label("AWS profile", cx))
            .child(
                h_flex().flex_wrap().gap_2().children(chips).child(
                    div()
                        .w(px(200.))
                        .child(Input::new(&self.typed_profile).id("aws-typed-profile")),
                ),
            )
            .when_some(status, |el, status| {
                el.child(hint(status, cx).id("aws-profiles-status").test_support())
            })
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> Div {
        let searching = matches!(self.search, Search::Searching);
        v_flex()
            .flex_none()
            .gap_3()
            .child(self.render_profiles(cx))
            .child(
                v_flex()
                    .gap_1p5()
                    .child(label("Regions", cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.regions).id("aws-regions")),
                            )
                            .child(
                                Button::new("aws-find")
                                    .cursor_pointer()
                                    .primary()
                                    .label("Find databases")
                                    .loading(searching)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.find(window, cx)),
                                    ),
                            ),
                    )
                    .child(hint(
                        "Separate several with commas. Leave empty for the profile's region."
                            .into(),
                        cx,
                    )),
            )
    }

    fn render_line(
        &self,
        line: &Line,
        discovery: &Discovery,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match line {
            Line::Region(region, count) => h_flex()
                .id(SharedString::from(format!("aws-region-{region}")))
                .test_support()
                .flex_none()
                .h(px(24.))
                .px_2()
                .gap_2()
                .text_xs()
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(region.clone())
                .child(format!("{count}"))
                .into_any_element(),
            Line::Found(index) => self.render_result(*index, &discovery.found[*index], cx),
        }
    }

    fn render_result(
        &self,
        index: usize,
        found: &Discovered,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let selected = self.selected == Some(index);
        let usable = found.usable();
        let (selected_bg, hover, muted) =
            (theme.sidebar_accent, theme.accent, theme.muted_foreground);
        let available = found.status == "available";
        let status_color = if available {
            theme.success
        } else {
            theme.warning
        };
        // What stands out: why it can't be used, or a status other than available.
        let status = if found.adapter.is_none() {
            "not supported".to_owned()
        } else if available {
            String::new()
        } else {
            found.status.clone()
        };
        let badge = match found.adapter {
            Some(adapter) => logos::icon(adapter, 16.),
            None => div()
                .flex_none()
                .size(px(16.))
                .rounded(px(3.5))
                .bg(theme.muted),
        };
        let row = h_flex()
            .id(SharedString::from(format!("aws-result-{index}")))
            .test_support()
            .flex_none()
            .h(px(ROW_HEIGHT))
            .px_2()
            .gap_2()
            .rounded(theme.radius)
            .when(selected, |el| el.bg(selected_bg))
            .when(!usable, |el| el.opacity(0.55))
            .child(badge)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_medium()
                    .child(found.label.clone()),
            )
            .child(
                div()
                    .w(px(176.))
                    .flex_none()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(found.engine_label()),
            )
            .child(
                div()
                    .w(px(92.))
                    .flex_none()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(found.source.region.clone()),
            )
            .child(
                h_flex()
                    .w(px(86.))
                    .flex_none()
                    .gap_1p5()
                    .text_xs()
                    .text_color(muted)
                    .child(
                        div()
                            .flex_none()
                            .size(px(6.))
                            .rounded_full()
                            .bg(status_color),
                    )
                    .child(div().truncate().child(status)),
            );
        if usable {
            row.cursor_pointer()
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                // A click selects; a second one uses it.
                .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                    this.selected = Some(index);
                    cx.notify();
                    if event.click_count() >= 2 {
                        this.use_selected(cx);
                    }
                }))
                .into_any_element()
        } else {
            row.into_any_element()
        }
    }

    /// What to know about the chosen database before using it, in one line.
    fn render_chosen(&self, cx: &App) -> Option<AnyElement> {
        let found = self.chosen(cx)?;
        let sign_in = match &found.auth {
            Auth::Password => "password",
            Auth::AwsSecret(_) => "Secrets Manager",
            Auth::AwsParameter(_) => "Parameter Store",
            Auth::AwsIam(_) => "IAM",
        };
        let reach = if found.public {
            "public"
        } else {
            "private: needs an SSH tunnel"
        };
        Some(
            div()
                .id("aws-chosen")
                .test_support()
                .flex_none()
                .truncate()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "{}:{} · {sign_in} · {reach}",
                    found.host, found.port
                ))
                .into_any_element(),
        )
    }

    /// Places that couldn't be searched, as one quiet line that opens to the details,
    /// with the regions that hold nothing. Loud only when nothing was found at all.
    fn render_notices(&self, discovery: &Discovery, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let (muted, warning) = (theme.muted_foreground, theme.warning);
        let empty = nothing_in(discovery);
        let nothing = (!empty.is_empty()).then(|| format!("Nothing in {}.", empty.join(", ")));
        if discovery.errors.is_empty() {
            return nothing.map(|text| {
                hint(text, cx)
                    .id("aws-nothing")
                    .test_support()
                    .into_any_element()
            });
        }
        let count = discovery.errors.len();
        let summary = format!(
            "{count} {} couldn't be searched",
            if count == 1 { "place" } else { "places" }
        );
        let open = self.notices_open;
        let details = discovery
            .errors
            .iter()
            .enumerate()
            .map(|(i, error)| render_error(i, error, muted, cx));
        Some(
            v_flex()
                .flex_none()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(muted)
                        .child(
                            h_flex()
                                .id("aws-notices")
                                .test_support()
                                .gap_1p5()
                                .cursor_pointer()
                                .child(div().size(px(6.)).rounded_full().bg(warning))
                                .child(summary)
                                .child(div().underline().child(if open { "Hide" } else { "Show" }))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notices_open = !this.notices_open;
                                    cx.notify();
                                })),
                        )
                        .when_some(nothing, |el, nothing| {
                            el.child(div().id("aws-nothing").test_support().child(nothing))
                        })
                        .child(div().flex_1())
                        .child(
                            Button::new("aws-retry")
                                .cursor_pointer()
                                .ghost()
                                .xsmall()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, window, cx| this.find(window, cx))),
                        ),
                )
                .when(open, |el| {
                    el.child(
                        v_flex()
                            .gap_1()
                            .pl_3()
                            .border_l_2()
                            .border_color(theme.border)
                            .children(details),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (danger, muted, border, radius) = (
            theme.danger,
            theme.muted_foreground,
            theme.border,
            theme.radius,
        );
        let area = v_flex().flex_1().min_h_0().gap_2();
        let discovery = match &self.search {
            Search::Idle => {
                return area.child(hint(
                    "Choose a profile and regions, then Find databases.".into(),
                    cx,
                ));
            }
            Search::Searching => {
                return area.child(
                    div()
                        .id("aws-loading")
                        .test_support()
                        .text_sm()
                        .text_color(muted)
                        .child("Looking for databases…"),
                );
            }
            Search::Done(discovery) => discovery,
        };
        if discovery.found.is_empty() {
            // The errors are all there is to say, so they stand out.
            let errors = discovery
                .errors
                .iter()
                .enumerate()
                .map(|(i, error)| render_error(i, error, danger, cx));
            let regions = discovery.empty_regions.join(", ");
            return area
                .children(errors)
                .when(!discovery.errors.is_empty(), |el| {
                    el.child(
                        h_flex().child(
                            Button::new("aws-retry")
                                .cursor_pointer()
                                .ghost()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, window, cx| this.find(window, cx))),
                        ),
                    )
                })
                .when(discovery.errors.is_empty(), |el| {
                    el.child(
                        div()
                            .id("aws-empty")
                            .test_support()
                            .text_sm()
                            .text_color(muted)
                            .child(format!("No databases found in {regions}.")),
                    )
                });
        }
        let shown = self.shown(cx);
        let count = if shown.len() == discovery.found.len() {
            format!("{} found", discovery.found.len())
        } else {
            format!("{} found · {} shown", discovery.found.len(), shown.len())
        };
        let lines: Vec<AnyElement> = self
            .lines(&shown)
            .iter()
            .map(|line| self.render_line(line, discovery, cx))
            .collect();
        area.child(
            h_flex()
                .flex_none()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .key_context(SEARCH_CONTEXT)
                        .on_action(
                            cx.listener(|this, _: &ResultUp, _, cx| this.move_selection(-1, cx)),
                        )
                        .on_action(
                            cx.listener(|this, _: &ResultDown, _, cx| this.move_selection(1, cx)),
                        )
                        .on_action(cx.listener(|this, _: &UseResult, _, cx| this.use_selected(cx)))
                        .on_action(cx.listener(|this, _: &ClearSearch, window, cx| {
                            this.clear_search(window, cx)
                        }))
                        .child(
                            Input::new(&self.query)
                                .id("aws-search")
                                .prefix(Icon::new(IconName::Search).small()),
                        ),
                )
                .child(
                    div()
                        .id("aws-count")
                        .test_support()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(count),
                ),
        )
        .when_some(self.render_notices(discovery, cx), |el, notices| {
            el.child(notices)
        })
        .child(
            v_flex()
                .id("aws-results")
                .test_support()
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h(px(ROW_HEIGHT * 4. + 8.))
                .p_1()
                .gap_px()
                .rounded(radius)
                .border_1()
                .border_color(border)
                .overflow_y_scroll()
                .children(lines)
                .when(shown.is_empty(), |el| {
                    el.child(
                        div()
                            .id("aws-no-match")
                            .test_support()
                            .p_2()
                            .text_sm()
                            .text_color(muted)
                            .child("No database matches the search."),
                    )
                }),
        )
        .when_some(self.render_chosen(cx), |el, chosen| el.child(chosen))
    }
}

/// The regions searched that hold nothing. A region where a service couldn't be searched
/// may hold what it would have listed, so it isn't one.
fn nothing_in(discovery: &Discovery) -> Vec<&str> {
    discovery
        .empty_regions
        .iter()
        .filter(|r| {
            !discovery
                .errors
                .iter()
                .any(|e| e.region.as_deref() == Some(r.as_str()))
        })
        .map(String::as_str)
        .collect()
}

fn render_error(index: usize, error: &DiscoverError, color: Hsla, cx: &App) -> Div {
    // The message names the region itself.
    v_flex()
        .gap_0p5()
        .child(
            div()
                .id(SharedString::from(format!("aws-error-{index}")))
                .test_support()
                .text_sm()
                .text_color(color)
                .child(error.message.clone()),
        )
        .when_some(error.detail.clone(), |el, detail| {
            el.child(
                div()
                    .id(SharedString::from(format!("aws-error-detail-{index}")))
                    .test_support()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(detail),
            )
        })
}

fn label(text: &'static str, cx: &App) -> Div {
    div()
        .text_xs()
        .font_medium()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn hint(text: String, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

impl Render for AwsPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("aws-picker")
            .test_support()
            .size_full()
            .min_h_0()
            .gap_4()
            .child(self.render_controls(cx))
            .child(self.render_results(cx))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    // No glob from gpui_kit: its `test` macro would shadow the built-in `#[test]`.
    use super::{AwsPicker, AwsPickerEvent, Search};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
    use std::cell::RefCell;
    use std::rc::Rc;
    use tuplebase_core::cloud::aws::Profile;
    use tuplebase_core::cloud::{DiscoverError, DiscoverErrorKind, Discovered, Discovery};
    use tuplebase_core::config::{Adapter, Auth, AwsIamAuth, Provider, Source};
    use tuplebase_core::state::CloudSearch;

    fn open(last: CloudSearch, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<AwsPicker>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys(super::key_bindings());
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                cx.new(|cx| AwsPicker::new(last, window, cx))
            })
            .expect("open test window")
        })
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

    /// A database a search could find.
    pub(crate) fn found(label: &str, adapter: Option<Adapter>) -> Discovered {
        found_in(label, adapter, "eu-west-1")
    }

    pub(crate) fn found_in(label: &str, adapter: Option<Adapter>, region: &str) -> Discovered {
        Discovered {
            label: label.into(),
            adapter,
            engine: "postgres".into(),
            version: "16.3".into(),
            host: format!("{label}.abc.{region}.rds.amazonaws.com"),
            port: 5432,
            database: "postgres".into(),
            user: "admin".into(),
            status: "available".into(),
            public: false,
            iam_enabled: true,
            managed_secret: None,
            auth: Auth::AwsIam(AwsIamAuth {
                profile: "prod".into(),
                region: region.into(),
            }),
            source: Source {
                provider: Provider::Aws,
                resource: format!("arn:aws:rds:{region}:123456789012:db:{label}"),
                region: region.into(),
            },
            note: Some("Not reachable from the internet.".into()),
        }
    }

    /// `count` databases over three regions, with a MySQL one and an unsupported one.
    pub(crate) fn many(count: usize) -> Vec<Discovered> {
        let regions = ["eu-west-1", "us-east-1", "ap-south-1"];
        (0..count)
            .map(|i| {
                let mut db = found_in(
                    &format!("db-{i:02}"),
                    Some(Adapter::Postgres),
                    regions[i % 3],
                );
                if i == 4 {
                    db.label = "billing".into();
                    db.engine = "mysql".into();
                    db.adapter = Some(Adapter::Mysql);
                    db.version = "8.0.36".into();
                }
                if i == 7 {
                    db.engine = "oracle-ee".into();
                    db.adapter = None;
                }
                db
            })
            .collect()
    }

    fn profiles() -> Vec<Profile> {
        vec![
            Profile {
                name: "default".into(),
                region: Some("us-east-1".into()),
            },
            Profile {
                name: "prod".into(),
                region: Some("eu-west-1".into()),
            },
        ]
    }

    fn regions(picker: &Entity<AwsPicker>, cx: &mut TestAppContext) -> String {
        cx.update(|cx| picker.read(cx).regions.read(cx).value().to_string())
    }

    /// The labels the list shows, in order.
    fn shown(picker: &Entity<AwsPicker>, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            let picker = picker.read(cx);
            let Search::Done(discovery) = &picker.search else {
                return Vec::new();
            };
            picker
                .shown(cx)
                .into_iter()
                .map(|i| discovery.found[i].label.clone())
                .collect()
        })
    }

    fn selected(picker: &Entity<AwsPicker>, cx: &mut TestAppContext) -> Option<String> {
        cx.update(|cx| picker.read(cx).chosen(cx).map(|f| f.label.clone()))
    }

    fn uses(picker: &Entity<AwsPicker>, cx: &mut TestAppContext) -> Rc<RefCell<Vec<String>>> {
        let events = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(picker, move |_, event, _| {
                events.borrow_mut().push(match event {
                    AwsPickerEvent::Use(found) => format!("use {}", found.label),
                    AwsPickerEvent::Searched(search) => {
                        format!("searched {} {}", search.profile, search.regions.join("+"))
                    }
                })
            })
            .detach();
        });
        events
    }

    fn show(picker: &Entity<AwsPicker>, discovery: Discovery, cx: &mut TestAppContext) {
        cx.update(|cx| picker.update(cx, |p, cx| p.set_discovery(discovery, cx)));
    }

    #[gpui_kit::test]
    fn profiles_are_offered_and_bring_their_region(cx: &mut TestAppContext) {
        let (handle, picker) = open(CloudSearch::default(), cx);
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-profiles-status").visible());
            picker.update(cx, |p, cx| p.set_profiles(Ok(profiles()), window, cx));
        });
        assert_eq!(regions(&picker, cx), "us-east-1");
        act(handle, cx, |window, cx| {
            assert!(window.try_find("aws-profiles-status").is_none());
            window.click("aws-profile-prod", cx);
        });
        assert_eq!(regions(&picker, cx), "eu-west-1");
        cx.update(|cx| assert_eq!(picker.read(cx).profile, "prod"));
    }

    #[gpui_kit::test]
    fn no_profile_files_still_allow_the_default_credentials(cx: &mut TestAppContext) {
        let last = CloudSearch {
            profile: String::new(),
            regions: vec!["ap-south-1".into()],
        };
        let (handle, picker) = open(last, cx);
        act(handle, cx, |window, cx| {
            picker.update(cx, |p, cx| p.set_profiles(Ok(Vec::new()), window, cx));
        });
        act(handle, cx, |window, _| {
            assert!(window.find("aws-profiles-status").visible());
            assert!(window.find("aws-profile-").visible());
        });
        // The remembered regions are kept.
        assert_eq!(regions(&picker, cx), "ap-south-1");
    }

    #[gpui_kit::test]
    fn a_search_shows_loading_then_results_and_uses_the_chosen_one(cx: &mut TestAppContext) {
        let last = CloudSearch {
            profile: "prod".into(),
            regions: vec!["eu-west-1".into(), "us-east-1".into()],
        };
        let (handle, picker) = open(last, cx);
        let events = uses(&picker, cx);
        act(handle, cx, |window, cx| window.click("aws-find", cx));
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-loading").visible());
            picker.update(cx, |p, cx| {
                assert!(matches!(p.search, Search::Searching));
                assert!(!p.can_use(cx));
                let mut oracle = found("legacy", None);
                oracle.engine = "oracle-ee".into();
                p.set_discovery(
                    Discovery {
                        found: vec![oracle, found("orders", Some(Adapter::Postgres))],
                        empty_regions: vec!["us-east-1".into()],
                        errors: Vec::new(),
                    },
                    cx,
                );
            });
        });
        act(handle, cx, |window, cx| {
            assert!(window.try_find("aws-loading").is_none());
            assert!(window.find("aws-result-0").visible());
            assert!(window.find("aws-result-1").visible());
            assert!(window.find("aws-nothing").visible());
            // The unsupported one can't be chosen.
            window.click("aws-result-0", cx);
        });
        // The first usable one is chosen already.
        assert_eq!(selected(&picker, cx).as_deref(), Some("orders"));
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-chosen").visible());
            window.double_click("aws-result-1", cx);
        });
        assert_eq!(
            *events.borrow(),
            ["searched prod eu-west-1+us-east-1", "use orders"]
        );
    }

    #[gpui_kit::test]
    fn the_search_box_filters_by_name_engine_region_and_endpoint(cx: &mut TestAppContext) {
        let (handle, picker) = open(CloudSearch::default(), cx);
        show(
            &picker,
            Discovery {
                found: many(24),
                ..Discovery::default()
            },
            cx,
        );
        assert_eq!(shown(&picker, cx).len(), 24);
        // By region, then name, each region under its heading.
        assert_eq!(shown(&picker, cx)[..2], ["db-02", "db-05"]);
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-region-ap-south-1").visible());
            window.click("aws-search", cx);
            window.input("MYSQL", cx);
        });
        assert_eq!(shown(&picker, cx), ["billing"]);
        // The selection follows the search.
        assert_eq!(selected(&picker, cx).as_deref(), Some("billing"));
        act(handle, cx, |window, cx| {
            // One region shown: no headings.
            assert!(window.try_find("aws-region-us-east-1").is_none());
            assert!(window.try_find("aws-result-0").is_none());
            picker.update(cx, |p, cx| {
                p.query
                    .update(cx, |q, cx| q.set_value("eu-west db-1", window, cx))
            });
        });
        assert_eq!(shown(&picker, cx), ["db-12", "db-15", "db-18"]);
        act(handle, cx, |window, cx| {
            picker.update(cx, |p, cx| {
                p.query
                    .update(cx, |q, cx| q.set_value("db-21.abc.", window, cx))
            })
        });
        assert_eq!(shown(&picker, cx), ["db-21"]);
        act(handle, cx, |window, cx| {
            picker.update(cx, |p, cx| {
                p.query
                    .update(cx, |q, cx| q.set_value("nothing-like-it", window, cx))
            });
        });
        act(handle, cx, |window, _| {
            assert!(window.find("aws-no-match").visible());
        });
        assert_eq!(selected(&picker, cx), None);
        // A new search starts unfiltered.
        act(handle, cx, |window, cx| window.click("aws-find", cx));
        cx.update(|cx| assert_eq!(picker.read(cx).query_text(cx), ""));
    }

    #[gpui_kit::test]
    fn up_down_and_enter_in_the_search_choose_and_use_and_escape_clears(cx: &mut TestAppContext) {
        let (handle, picker) = open(CloudSearch::default(), cx);
        let events = uses(&picker, cx);
        show(
            &picker,
            Discovery {
                found: many(9),
                ..Discovery::default()
            },
            cx,
        );
        // ap-south-1: db-02 db-05 db-08; eu-west-1: db-00 db-03 db-06; us-east-1:
        // billing db-01 db-07 (unsupported).
        assert_eq!(selected(&picker, cx).as_deref(), Some("db-02"));
        act(handle, cx, |window, cx| window.click("aws-search", cx));
        act(handle, cx, |window, cx| {
            window.press("down", cx);
            window.press("down", cx);
            window.press("down", cx);
        });
        assert_eq!(selected(&picker, cx).as_deref(), Some("db-00"));
        act(handle, cx, |window, cx| window.press("up", cx));
        assert_eq!(selected(&picker, cx).as_deref(), Some("db-08"));
        act(handle, cx, |window, cx| {
            for _ in 0..10 {
                window.press("down", cx);
            }
        });
        // The unsupported one, last, is passed over.
        assert_eq!(selected(&picker, cx).as_deref(), Some("db-01"));
        act(handle, cx, |window, cx| window.input("bill", cx));
        assert_eq!(shown(&picker, cx), ["billing"]);
        assert_eq!(selected(&picker, cx).as_deref(), Some("billing"));
        act(handle, cx, |window, cx| window.press("enter", cx));
        assert_eq!(*events.borrow(), ["use billing"]);
        act(handle, cx, |window, cx| window.press("escape", cx));
        assert_eq!(shown(&picker, cx).len(), 9);
        cx.update(|cx| assert_eq!(picker.read(cx).query_text(cx), ""));
    }

    #[gpui_kit::test]
    fn places_that_couldnt_be_searched_are_one_quiet_line_beside_results(cx: &mut TestAppContext) {
        let (handle, picker) = open(CloudSearch::default(), cx);
        let refused = |region: &str| DiscoverError {
            region: Some(region.into()),
            kind: DiscoverErrorKind::AccessDenied,
            message: format!("Not allowed to list Redshift clusters in {region}."),
            detail: None,
        };
        show(
            &picker,
            Discovery {
                found: vec![found("orders", Some(Adapter::Postgres))],
                // RDS answered in both, but Redshift didn't in ap-south-1.
                empty_regions: vec!["us-east-1".into(), "ap-south-1".into()],
                errors: vec![refused("eu-west-1"), refused("ap-south-1")],
            },
            cx,
        );
        cx.update(|cx| {
            let picker = picker.read(cx);
            let Search::Done(discovery) = &picker.search else {
                panic!("searched")
            };
            assert_eq!(super::nothing_in(discovery), ["us-east-1"]);
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-result-0").visible());
            assert!(window.find("aws-notices").visible());
            assert!(window.find("aws-nothing").visible());
            assert!(window.try_find("aws-error-0").is_none());
            window.click("aws-notices", cx);
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-error-0").visible());
            assert!(window.find("aws-error-1").visible());
            window.click("aws-notices", cx);
        });
        act(handle, cx, |window, _| {
            assert!(window.try_find("aws-error-0").is_none());
            assert!(window.find("aws-retry").visible());
        });
    }

    #[gpui_kit::test]
    fn an_empty_search_and_a_failed_one_say_so(cx: &mut TestAppContext) {
        let (handle, picker) = open(CloudSearch::default(), cx);
        act(handle, cx, |_, cx| {
            picker.update(cx, |p, cx| {
                p.set_discovery(
                    Discovery {
                        empty_regions: vec!["eu-west-1".into()],
                        ..Discovery::default()
                    },
                    cx,
                )
            });
        });
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-empty").visible());
            assert!(window.try_find("aws-retry").is_none());
            picker.update(cx, |p, cx| {
                p.set_discovery(
                    Discovery {
                        errors: vec![DiscoverError {
                            region: None,
                            kind: DiscoverErrorKind::SignIn,
                            message: "Run `aws sso login --profile prod`, then Retry.".into(),
                            detail: Some("failed to read the SSO cache".into()),
                        }],
                        ..Discovery::default()
                    },
                    cx,
                )
            });
        });
        // With nothing found, the error is all there is to say: it stays open.
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-error-0").visible());
            assert!(window.find("aws-error-detail-0").visible());
            assert!(window.try_find("aws-empty").is_none());
            assert!(window.try_find("aws-notices").is_none());
            window.click("aws-retry", cx);
        });
        act(handle, cx, |window, _| {
            assert!(window.find("aws-loading").visible());
        });
    }
}
