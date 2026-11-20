//! Finding a database in AWS: choose a profile and regions, search, pick one. What it
//! finds fills in the connection form; core does the AWS work (`cloud::aws`).

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tuplebase_core::cloud::aws::{self, Profile};
use tuplebase_core::cloud::{self, Discovered, Discovery};
use tuplebase_core::config::{Connection, Provider};
use tuplebase_core::state::CloudSearch;

use crate::runtime::io;

pub enum AwsPickerEvent {
    /// Fill in the form with this connection; the note says what to know about it.
    Use {
        connection: Box<Connection>,
        note: Option<String>,
    },
    /// Remember where this search looked.
    Searched(CloudSearch),
    Back,
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

pub struct AwsPicker {
    profiles: Profiles,
    /// The chosen profile; empty means the default credentials.
    profile: String,
    /// A profile typed in, which wins over the list.
    typed_profile: Entity<InputState>,
    /// Regions, separated by commas or spaces.
    regions: Entity<InputState>,
    search: Search,
    /// Index into the results.
    selected: Option<usize>,
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
            search: Search::Idle,
            selected: None,
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
        // Preselect the only usable one.
        let usable: Vec<usize> = (0..discovery.found.len())
            .filter(|&i| discovery.found[i].usable())
            .collect();
        self.selected = (usable.len() == 1).then(|| usable[0]);
        self.search = Search::Done(discovery);
        cx.notify();
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
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let found = io(async move {
                let discovery =
                    cloud::discover(Provider::Aws, &search.profile, &search.regions).await;
                Ok(discovery)
            })
            .await;
            this.update(cx, |this, cx| match found {
                Ok(discovery) => this.set_discovery(discovery, cx),
                // Only if the runtime itself failed; discovery reports its own errors.
                Err(e) => this.set_discovery(
                    Discovery {
                        errors: vec![cloud::DiscoverError {
                            region: None,
                            kind: cloud::DiscoverErrorKind::Other,
                            message: format!("{e:#}"),
                            detail: None,
                        }],
                        ..Discovery::default()
                    },
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn use_selected(&mut self, cx: &mut Context<Self>) {
        let Search::Done(discovery) = &self.search else {
            return;
        };
        let Some(found) = self.selected.and_then(|i| discovery.found.get(i)) else {
            return;
        };
        if let Some(connection) = found.connection() {
            cx.emit(AwsPickerEvent::Use {
                connection: Box::new(connection),
                note: found.note.clone(),
            });
        }
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
            .gap_2()
            .child(label("AWS profile", cx))
            .child(h_flex().flex_wrap().gap_2().children(chips))
            .when_some(status, |el, status| {
                el.child(hint(status, cx).id("aws-profiles-status").test_support())
            })
            .child(Input::new(&self.typed_profile).id("aws-typed-profile"))
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
        let engine = match found.adapter {
            Some(adapter) => format!("{} · {}", adapter.label(), found.engine),
            None => found.engine.clone(),
        };
        let endpoint = if found.host.is_empty() {
            String::new()
        } else {
            format!("{}:{}", found.host, found.port)
        };
        let row = v_flex()
            .id(SharedString::from(format!("aws-result-{index}")))
            .test_support()
            .gap_1()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(if selected {
                theme.foreground
            } else {
                theme.border
            })
            .when(!usable, |el| el.opacity(0.6))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_sm().font_semibold().child(found.label.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(engine),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{} · {}", found.source.region, found.status)),
                    ),
            )
            .when(!endpoint.is_empty(), |el| {
                el.child(div().text_xs().child(endpoint))
            })
            .when_some(found.note.clone(), |el, note| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(note),
                )
            });
        if usable {
            row.cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected = Some(index);
                    cx.notify();
                }))
                .into_any_element()
        } else {
            row.into_any_element()
        }
    }

    fn render_search(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let danger = theme.danger;
        let muted = theme.muted_foreground;
        match &self.search {
            Search::Idle => v_flex(),
            Search::Searching => v_flex().child(
                div()
                    .id("aws-loading")
                    .test_support()
                    .text_sm()
                    .text_color(muted)
                    .child("Looking for databases…"),
            ),
            Search::Done(discovery) => {
                let errors = discovery.errors.iter().enumerate().map(|(i, error)| {
                    let place = error
                        .region
                        .as_ref()
                        .map_or(String::new(), |r| format!("{r}: "));
                    v_flex()
                        .gap_0p5()
                        .child(
                            div()
                                .id(SharedString::from(format!("aws-error-{i}")))
                                .test_support()
                                .text_sm()
                                .text_color(danger)
                                .child(format!("{place}{}", error.message)),
                        )
                        .when_some(error.detail.clone(), |el, detail| {
                            el.child(
                                div()
                                    .id(SharedString::from(format!("aws-error-detail-{i}")))
                                    .test_support()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(detail),
                            )
                        })
                });
                let nothing = discovery.found.is_empty() && discovery.errors.is_empty();
                let results: Vec<_> = discovery
                    .found
                    .iter()
                    .enumerate()
                    .map(|(i, found)| self.render_result(i, found, cx))
                    .collect();
                v_flex()
                    .gap_2()
                    .children(errors)
                    .when(!discovery.errors.is_empty(), |el| {
                        el.child(
                            Button::new("aws-retry")
                                .cursor_pointer()
                                .ghost()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, window, cx| this.find(window, cx))),
                        )
                    })
                    .when(nothing, |el| {
                        let regions = discovery.empty_regions.join(", ");
                        el.child(
                            div()
                                .id("aws-empty")
                                .test_support()
                                .text_sm()
                                .text_color(muted)
                                .child(format!("No databases found in {regions}.")),
                        )
                    })
                    .when(!discovery.empty_regions.is_empty() && !nothing, |el| {
                        el.child(hint(
                            format!("Nothing in {}.", discovery.empty_regions.join(", ")),
                            cx,
                        ))
                    })
                    .children(results)
            }
        }
    }
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
        let searching = matches!(self.search, Search::Searching);
        let can_use = self.selected.is_some() && !searching;
        v_flex()
            .id("aws-picker")
            .test_support()
            .gap_4()
            .child(self.render_profiles(cx))
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Regions", cx))
                    .child(Input::new(&self.regions).id("aws-regions"))
                    .child(hint(
                        "Separate several with commas. Leave empty for the profile's region."
                            .into(),
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("aws-find")
                            .cursor_pointer()
                            .primary()
                            .label("Find databases")
                            .loading(searching)
                            .on_click(cx.listener(|this, _, window, cx| this.find(window, cx))),
                    )
                    .child(
                        Button::new("aws-back")
                            .cursor_pointer()
                            .ghost()
                            .label("Back")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(AwsPickerEvent::Back))),
                    ),
            )
            .child(self.render_search(cx))
            .when(matches!(self.search, Search::Done(_)), |el| {
                el.child(
                    Button::new("aws-use")
                        .cursor_pointer()
                        .primary()
                        .label("Use selected")
                        .disabled(!can_use)
                        .on_click(cx.listener(|this, _, _, cx| this.use_selected(cx))),
                )
            })
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
    use tuplebase_core::config::{Adapter, Auth, AwsIamAuth, Provider, Source, TlsMode};
    use tuplebase_core::state::CloudSearch;

    fn open(last: CloudSearch, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<AwsPicker>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
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
        Discovered {
            label: label.into(),
            adapter,
            engine: "postgres".into(),
            host: format!("{label}.abc.eu-west-1.rds.amazonaws.com"),
            port: 5432,
            database: "postgres".into(),
            user: "admin".into(),
            status: "available".into(),
            public: false,
            iam_enabled: true,
            managed_secret: None,
            auth: Auth::AwsIam(AwsIamAuth {
                profile: "prod".into(),
                region: "eu-west-1".into(),
            }),
            source: Source {
                provider: Provider::Aws,
                resource: format!("arn:aws:rds:eu-west-1:123456789012:db:{label}"),
                region: "eu-west-1".into(),
            },
            note: Some("Not reachable from the internet.".into()),
        }
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
        let events = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&picker, move |_, event, _| {
                events.borrow_mut().push(match event {
                    AwsPickerEvent::Use { connection, note } => format!(
                        "use {} {:?} {}",
                        connection.name,
                        connection.tls,
                        note.is_some()
                    ),
                    AwsPickerEvent::Searched(search) => {
                        format!("searched {} {}", search.profile, search.regions.join("+"))
                    }
                    AwsPickerEvent::Back => "back".into(),
                })
            })
            .detach();
        });
        act(handle, cx, |window, cx| window.click("aws-find", cx));
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-loading").visible());
            assert!(window.try_find("aws-use").is_none());
            picker.update(cx, |p, cx| {
                assert!(matches!(p.search, Search::Searching));
                let mut oracle = found("legacy", None);
                oracle.engine = "oracle-ee".into();
                p.set_discovery(
                    Discovery {
                        found: vec![found("orders", Some(Adapter::Postgres)), oracle],
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
            // The unsupported one can't be chosen.
            window.click("aws-result-1", cx);
        });
        // The only usable result is chosen already.
        cx.update(|cx| assert_eq!(picker.read(cx).selected, Some(0)));
        act(handle, cx, |window, cx| window.click("aws-use", cx));
        assert_eq!(
            *events.borrow(),
            [
                "searched prod eu-west-1+us-east-1".to_owned(),
                format!("use orders {:?} true", TlsMode::Require),
            ]
        );
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
        act(handle, cx, |window, cx| {
            assert!(window.find("aws-error-0").visible());
            assert!(window.find("aws-error-detail-0").visible());
            assert!(window.try_find("aws-empty").is_none());
            window.click("aws-retry", cx);
        });
        act(handle, cx, |window, _| {
            assert!(window.find("aws-loading").visible());
        });
    }
}
