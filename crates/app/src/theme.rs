//! The TupleBase theme. The palette is the brand's neutrals; the only color in the app
//! comes from the database logos.

use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::*;

/// How much smaller code is than the rest of the text, in pixels.
const CODE_FONT_GAP: f32 = 2.;

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

/// Sets the size of the app's text in pixels. Everything sized in rems follows it.
pub fn set_font_size(size: u8, cx: &mut App) {
    let size = f32::from(size);
    let code_size = size - CODE_FONT_GAP;
    Theme::update(cx, |theme| {
        // The light and dark themes carry their own sizes, which would come back when
        // the system switches between them.
        for config in [&mut theme.light_theme, &mut theme.dark_theme] {
            let mut sized = (**config).clone();
            sized.font_size = Some(size);
            sized.mono_font_size = Some(code_size);
            *config = Rc::new(sized);
        }
        theme.font_size = px(size);
        theme.mono_font_size = px(code_size);
    });
}
