//! The TupleBase theme. The palette is the brand's neutrals plus one accent, a teal of
//! the wordmark's hue, kept for what is chosen or about to run: the primary button, focus,
//! the selected row and connection, links. Other color comes from the database logos.
//! The fonts are bundled, so the app reads the same on every system.

use std::borrow::Cow;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, StyledExt as _, Theme, ThemeMode, ThemeRegistry, h_flex,
};
use gpui_kit::*;
use tuplebase_core::config::Appearance;

/// How much smaller code is than the rest of the text, in pixels.
const CODE_FONT_GAP: f32 = 2.;

/// Inter for the interface, JetBrains Mono for SQL and values. Both under the SIL Open
/// Font License; see `assets/fonts`.
const FONTS: [&[u8]; 7] = [
    include_bytes!("../../../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../../../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf"),
    include_bytes!("../../../assets/fonts/Inter-Bold.ttf"),
    include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../../../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../../../assets/fonts/JetBrainsMono-Italic.ttf"),
];

/// Loads the bundled fonts and themes. Call after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    cx.text_system()
        .add_fonts(FONTS.iter().map(|font| Cow::Borrowed(*font)).collect())
        .expect("bundled fonts are valid");
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

/// Uses the light or dark colors `appearance` asks for, or the system's.
pub fn set_appearance(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    match appearance {
        Appearance::System => Theme::sync_system_appearance(window, cx),
        Appearance::Light => Theme::change(ThemeMode::Light, window, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
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

/// The bar on top of a panel: one height, padding and title style for every panel.
/// Add the panel's buttons as children; they sit on the right.
pub fn panel_header(title: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .h(rems(2.5))
        .pl_3()
        .pr_1p5()
        .gap_0p5()
        .border_b_1()
        .border_color(theme.border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .font_semibold()
                .child(title.into()),
        )
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
    use gpui_kit::{Hsla, TestAppContext, rgb};

    /// The status colors come from the app's theme, not the library's defaults: a key
    /// spelled wrong would be ignored without a word.
    #[gpui_kit::test]
    fn the_theme_sets_the_status_colors(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::init(cx);
            let colors = |cx: &mut gpui_kit::App, mode| {
                Theme::change(mode, None, cx);
                (cx.theme().success, cx.theme().warning)
            };
            let hsla = |color: u32| -> Hsla { rgb(color).into() };
            assert_eq!(
                colors(cx, ThemeMode::Dark),
                (hsla(0x8FD19E), hsla(0xF0B072))
            );
            assert_eq!(
                colors(cx, ThemeMode::Light),
                (hsla(0x2A7A4B), hsla(0xA15C12))
            );
        });
    }
}
