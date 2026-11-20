//! The first step of adding a connection: where the database is. Typing the details in
//! goes straight to the form; a cloud provider first finds the database there.

use gpui_kit::component::*;
use gpui_kit::*;
use tuplebase_core::config::Connection;
use tuplebase_core::state::CloudSearch;

use crate::aws_picker::{AwsPicker, AwsPickerEvent};

pub enum ConnSourceEvent {
    /// Fill in the form by hand.
    Manual,
    /// Fill in the form with a database that was found.
    Use {
        connection: Box<Connection>,
        note: Option<String>,
    },
    /// Remember where a search in AWS looked.
    SearchedAws(CloudSearch),
}

pub struct ConnSource {
    /// Where the last search in AWS looked.
    last_aws: CloudSearch,
    aws: Option<Entity<AwsPicker>>,
    _aws_subscription: Option<Subscription>,
}

impl EventEmitter<ConnSourceEvent> for ConnSource {}

impl ConnSource {
    pub fn new(last_aws: CloudSearch) -> Self {
        Self {
            last_aws,
            aws: None,
            _aws_subscription: None,
        }
    }

    /// The AWS step, once chosen.
    #[cfg(test)]
    pub fn aws_picker(&self) -> Option<&Entity<AwsPicker>> {
        self.aws.as_ref()
    }

    fn open_aws(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.last_aws.clone();
        let picker = cx.new(|cx| AwsPicker::new(last, window, cx));
        self._aws_subscription = Some(cx.subscribe(&picker, |this, _, event, cx| match event {
            AwsPickerEvent::Use { connection, note } => cx.emit(ConnSourceEvent::Use {
                connection: connection.clone(),
                note: note.clone(),
            }),
            AwsPickerEvent::Searched(search) => {
                this.last_aws = search.clone();
                cx.emit(ConnSourceEvent::SearchedAws(search.clone()));
            }
            AwsPickerEvent::Back => {
                this.aws = None;
                this._aws_subscription = None;
                cx.notify();
            }
        }));
        self.aws = Some(picker);
        cx.notify();
    }

    fn render_choice(
        &self,
        id: &'static str,
        title: &'static str,
        detail: &'static str,
        cx: &App,
    ) -> impl StatefulInteractiveElement + IntoElement + use<> {
        let theme = cx.theme();
        v_flex()
            .id(id)
            .test_support()
            .flex_1()
            .gap_1()
            .p_4()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.input)
            .cursor_pointer()
            .child(div().text_sm().font_semibold().child(title))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(detail),
            )
    }
}

impl Render for ConnSource {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(picker) = &self.aws {
            return v_flex()
                .gap_3()
                .child(div().text_sm().font_semibold().child("Amazon Web Services"))
                .child(picker.clone());
        }
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child("Where is your database?"),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        self.render_choice(
                            "source-manual",
                            "Enter details manually",
                            "Any server or database file.",
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnSourceEvent::Manual))),
                    )
                    .child(
                        self.render_choice(
                            "source-aws",
                            "Amazon Web Services",
                            "Find it in RDS or Aurora with your AWS profile.",
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.open_aws(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnSource, ConnSourceEvent};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, AppContext as _, Entity, TestAppContext};
    use std::cell::RefCell;
    use std::rc::Rc;
    use tuplebase_core::state::CloudSearch;

    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<ConnSource>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(Default::default(), cx, |_, cx| {
                cx.new(|_| ConnSource::new(CloudSearch::default()))
            })
            .expect("open test window")
        })
    }

    fn act(
        handle: AnyWindowHandle,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App),
    ) {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            f(window, cx)
        })
        .unwrap();
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn manual_goes_on_to_the_form_and_aws_opens_the_picker_until_back(cx: &mut TestAppContext) {
        let (handle, source) = open(cx);
        let manual = Rc::new(RefCell::new(0));
        cx.update(|cx| {
            let manual = manual.clone();
            cx.subscribe(&source, move |_, event, _| {
                if let ConnSourceEvent::Manual = event {
                    *manual.borrow_mut() += 1;
                }
            })
            .detach();
        });
        act(handle, cx, |window, cx| window.click("source-manual", cx));
        assert_eq!(*manual.borrow(), 1);
        act(handle, cx, |window, cx| window.click("source-aws", cx));
        act(handle, cx, |window, cx| {
            assert!(window.try_find("source-manual").is_none());
            assert!(window.find("aws-find").visible());
            window.click("aws-back", cx);
        });
        act(handle, cx, |window, _| {
            assert!(window.find("source-aws").visible());
        });
        cx.update(|cx| assert!(source.read(cx).aws_picker().is_none()));
    }
}
