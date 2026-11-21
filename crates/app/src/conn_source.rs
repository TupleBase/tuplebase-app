//! The first step of adding a connection: where the database is. Typing the details in
//! goes straight to the form; a cloud provider first finds the database there.

use gpui_kit::component::*;
use gpui_kit::*;

pub enum ConnSourceEvent {
    /// Fill in the form by hand.
    Manual,
    /// Find the database in AWS first.
    Aws,
}

/// The two ways to start. Once one is chosen the form shows a switch between them, so
/// this step is only seen once.
pub struct ConnSource;

impl EventEmitter<ConnSourceEvent> for ConnSource {}

impl ConnSource {
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
                            "Find it in RDS, Aurora or Redshift with your AWS profile.",
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnSourceEvent::Aws))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnSource, ConnSourceEvent};
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext as _, TestAppContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[gpui_kit::test]
    fn each_card_says_which_way_was_chosen(cx: &mut TestAppContext) {
        let (handle, source) = cx.update(|cx| {
            gpui_kit::init(cx);
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| ConnSource))
                .expect("open test window")
        });
        let chosen = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let chosen = chosen.clone();
            cx.subscribe(&source, move |_, event, _| {
                chosen.borrow_mut().push(match event {
                    ConnSourceEvent::Manual => "manual",
                    ConnSourceEvent::Aws => "aws",
                })
            })
            .detach();
        });
        for id in ["source-manual", "source-aws"] {
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                window.click(id, cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        assert_eq!(*chosen.borrow(), ["manual", "aws"]);
    }
}
