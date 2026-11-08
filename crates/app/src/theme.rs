//! The TupleBase theme. The palette is the brand's neutrals; the only color in the app
//! comes from the database logos.

use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::*;

/// Loads the bundled themes. Call after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    ThemeRegistry::global_mut(cx)
        .load_themes_from_str(include_str!("theme.json"))
        .expect("bundled theme is valid");
    let themes = ThemeRegistry::global(cx).themes();
    let light = themes["TupleBase Light"].clone();
    let dark = themes["TupleBase Dark"].clone();
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
}
