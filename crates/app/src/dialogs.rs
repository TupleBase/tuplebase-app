//! The buttons of a dialog that the app draws itself. The ones `gpui-component` draws
//! for a dialog show the arrow and take no style from the app, so the app's dialogs
//! turn them off and use these, which show the hand like every other button.

use gpui_kit::component::button::*;
use gpui_kit::component::dialog::{Cancel, Confirm, DialogClose, DialogFooter};
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;

/// `content` with the close icon in its top right corner, for a dialog opened with
/// `close_button(false)` and `p_0()`, so that the icon sits where the library's would.
pub fn with_close_button(content: impl IntoElement) -> Div {
    div().relative().child(content).child(
        div()
            .absolute()
            .top_2()
            .right_2()
            .child(DialogClose::new().trigger(close_icon)),
    )
}

/// Cancel and a red `ok_text` button, for an alert dialog that confirms a deletion.
/// They act as Escape and Enter do, so the dialog's `on_ok` runs for OK. Cancel is a
/// plain button: `DialogClose` would stretch it across the footer.
pub fn confirm_footer(ok_text: &'static str) -> DialogFooter {
    DialogFooter::new().child(cancel()).child(ok(ok_text))
}

fn close_icon(button: Button) -> Button {
    button
        .small()
        .ghost()
        .icon(IconName::Close)
        .accessibility_label("Close")
        .cursor_pointer()
}

fn cancel() -> Button {
    Button::new("cancel")
        .label("Cancel")
        .cursor_pointer()
        .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx))
}

fn ok(label: &'static str) -> Button {
    Button::new("ok")
        .label(label)
        .danger()
        .cursor_pointer()
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(Confirm { secondary: false }), cx)
        })
}

#[cfg(test)]
mod tests {
    use super::{cancel, close_icon, ok};
    use gpui_kit::component::button::Button;
    use gpui_kit::{CursorStyle, Styled as _};

    /// The test window doesn't report the cursor it shows, so this checks the style each
    /// button asks for.
    #[test]
    fn the_dialog_buttons_show_the_hand() {
        let hand = Some(CursorStyle::PointingHand);
        assert_eq!(close_icon(Button::new("close")).style().mouse_cursor, hand);
        assert_eq!(cancel().style().mouse_cursor, hand);
        assert_eq!(ok("Delete").style().mouse_cursor, hand);
    }
}
