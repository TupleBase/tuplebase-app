//! Keyboard navigation shared by the app's trees.
//!
//! A tree gets it by giving its element `key_context(CONTEXT)`, tracking its own focus and
//! handling the actions below. The keys go to whichever tree has focus.

use gpui_kit::{KeyBinding, actions};

actions!(
    tuplebase,
    [TreeUp, TreeDown, TreeCollapse, TreeExpand, TreeOpen]
);

pub const CONTEXT: &str = "Tree";

pub fn key_bindings() -> [KeyBinding; 5] {
    [
        KeyBinding::new("up", TreeUp, Some(CONTEXT)),
        KeyBinding::new("down", TreeDown, Some(CONTEXT)),
        KeyBinding::new("left", TreeCollapse, Some(CONTEXT)),
        KeyBinding::new("right", TreeExpand, Some(CONTEXT)),
        KeyBinding::new("enter", TreeOpen, Some(CONTEXT)),
    ]
}

/// Where the cursor lands after moving `delta` rows through `len` of them, stopping at
/// both ends. With no cursor yet, moving down starts at the top and moving up at the
/// bottom. `None` when there are no rows.
pub fn step(len: usize, current: Option<usize>, delta: isize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match current {
        Some(index) => index.saturating_add_signed(delta).min(last),
        None if delta > 0 => 0,
        None => last,
    })
}

#[cfg(test)]
mod tests {
    use super::step;

    #[test]
    fn the_cursor_stops_at_both_ends() {
        assert_eq!(step(3, Some(1), 1), Some(2));
        assert_eq!(step(3, Some(2), 1), Some(2));
        assert_eq!(step(3, Some(0), -1), Some(0));
    }

    #[test]
    fn the_first_key_picks_an_end() {
        assert_eq!(step(3, None, 1), Some(0));
        assert_eq!(step(3, None, -1), Some(2));
        assert_eq!(step(0, None, 1), None);
    }
}
