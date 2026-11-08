//! Look and feel: the TupleBase theme and the colors that mark environments.
//!
//! The palette is the brand's neutrals. Color is reserved for database logos and for
//! environment folders, so seeing "Production" in red always means something.

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

const RED: u32 = 0xE5484D;
const AMBER: u32 = 0xE2A336;
const GREEN: u32 = 0x46A758;
const OTHERS: [u32; 4] = [0x3E8BD6, 0x8E6FD8, 0x2EA69A, 0xD66BA0];

fn folder_hex(folder: &str) -> u32 {
    let name = folder.to_lowercase();
    let has = |words: &[&str]| {
        name.split(|c: char| !c.is_alphanumeric())
            .any(|token| words.contains(&token))
    };
    if has(&["prod", "production", "live"]) {
        RED
    } else if has(&["dev", "development", "local", "sandbox"]) {
        GREEN
    } else if has(&["staging", "stage", "uat", "qa", "test", "testing"]) {
        AMBER
    } else {
        // Nested folders share their top-level folder's color.
        let root = name.split('/').next().unwrap_or(&name);
        let sum: usize = root.bytes().map(usize::from).sum();
        OTHERS[sum % OTHERS.len()]
    }
}

/// The color of an environment folder, from its path.
pub fn folder_color(folder: &str) -> Hsla {
    rgb(folder_hex(folder)).into()
}

#[cfg(test)]
mod tests {
    // No glob: gpui_kit's `test` macro would shadow the built-in `#[test]`.
    use super::{AMBER, GREEN, OTHERS, RED, folder_hex};

    #[test]
    fn environment_names_get_their_colors() {
        assert_eq!(folder_hex("Production"), RED);
        assert_eq!(folder_hex("eu-prod"), RED);
        assert_eq!(folder_hex("Staging"), AMBER);
        assert_eq!(folder_hex("local dev"), GREEN);
    }

    #[test]
    fn nested_folders_inherit_their_environment_and_color() {
        assert_eq!(folder_hex("Production/EU"), RED);
        assert_eq!(folder_hex("Analytics/one"), folder_hex("Analytics/two"));
    }

    #[test]
    fn local_wins_over_test() {
        assert_eq!(folder_hex("Local test databases"), GREEN);
    }

    #[test]
    fn words_only_match_whole() {
        assert_ne!(folder_hex("Products"), RED);
    }

    #[test]
    fn other_names_keep_a_stable_color() {
        assert_eq!(folder_hex("Analytics"), folder_hex("analytics"));
        assert!(OTHERS.contains(&folder_hex("Analytics")));
    }
}
