//! Database and agent icons and the app mark.

use std::sync::{Arc, LazyLock};

use gpui_kit::component::{Icon, Sizable as _};
use gpui_kit::*;
use tuplebase_core::config::Adapter;

static BRAND: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../../../assets/icon/icon-256.png").to_vec(),
    ))
});

/// How a database looks in the app: its mark, and its brand color, deep enough to
/// carry the white mark on either theme.
struct Logo {
    glyph: &'static [u8],
    color: u32,
    /// Two letters for the badge, where a list names the database in text.
    letters: &'static str,
}

/// Each engine's logo. A new engine adds its SVG to `assets/logos/` and an arm here.
fn logo(adapter: Adapter) -> Logo {
    match adapter {
        Adapter::Postgres => Logo {
            glyph: include_bytes!("../../../assets/logos/postgresql.svg"),
            color: 0x336791,
            letters: "PG",
        },
        Adapter::Mariadb => Logo {
            glyph: include_bytes!("../../../assets/logos/mariadb.svg"),
            color: 0xA0522D,
            letters: "MA",
        },
        Adapter::Mysql => Logo {
            glyph: include_bytes!("../../../assets/logos/mysql.svg"),
            color: 0x00758F,
            letters: "MY",
        },
        Adapter::Sqlite => Logo {
            glyph: include_bytes!("../../../assets/logos/sqlite.svg"),
            color: 0x0F80CC,
            letters: "SL",
        },
    }
}

fn parse_color(color: &str) -> Option<u32> {
    u32::from_str_radix(color.strip_prefix('#')?, 16).ok()
}

/// The color a connection shows: its own choice, or else its database's color.
pub fn color_of(adapter: Adapter, color: &str) -> Hsla {
    rgb(parse_color(color).unwrap_or_else(|| logo(adapter).color)).into()
}

/// A database's icon: its mark on a tile of its brand color. A connection's own color
/// never changes it, so the icon always says which database this is.
pub fn icon(adapter: Adapter, size: f32) -> Div {
    let logo = logo(adapter);
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(size * 0.22))
        .bg(rgb(logo.color))
        .child(
            Icon::default()
                .data(logo.glyph)
                .with_size(px((size * 0.78).round()))
                .text_color(white()),
        )
}

/// A database's badge: its two letters on a tile of its brand color, the same color as
/// its icon.
pub fn badge(adapter: Adapter, size: f32) -> Div {
    let logo = logo(adapter);
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(size * 0.24))
        .bg(rgb(logo.color))
        .text_color(white())
        .text_size(px((size * 0.4).round()))
        .font_weight(FontWeight::SEMIBOLD)
        .line_height(px(size))
        .child(logo.letters)
}

const CLAUDE: &[u8] = include_bytes!("../../../assets/logos/claude.svg");
const CLAUDE_COLOR: u32 = 0xD97757;

/// The mark of the agent the chat panel talks to, in its brand color.
pub fn agent() -> Icon {
    Icon::default().data(CLAUDE).text_color(rgb(CLAUDE_COLOR))
}

pub fn brand(size: f32) -> Img {
    img(BRAND.clone()).size(px(size))
}

#[cfg(test)]
mod tests {
    use super::{color_of, logo};
    use gpui_kit::{Hsla, rgb};
    use tuplebase_core::config::Adapter;

    #[test]
    fn every_engine_has_its_own_logo() {
        let logos = Adapter::ALL.map(logo);
        for (adapter, logo) in Adapter::ALL.iter().zip(&logos) {
            let is_svg = logo.glyph.windows(4).any(|w| w == b"<svg");
            assert!(is_svg, "{} has no SVG mark", adapter.label());
            assert_eq!(
                logos.iter().filter(|l| l.color == logo.color).count(),
                1,
                "{} shares its color",
                adapter.label()
            );
            assert_eq!(
                logo.letters.len(),
                2,
                "{} needs two letters",
                adapter.label()
            );
            assert_eq!(
                logos.iter().filter(|l| l.letters == logo.letters).count(),
                1,
                "{} shares its letters",
                adapter.label()
            );
        }
    }

    #[test]
    fn a_connection_color_replaces_the_database_color() {
        let chosen: Hsla = rgb(0x3E8BD6).into();
        let database: Hsla = rgb(0x0F80CC).into();
        assert_eq!(color_of(Adapter::Sqlite, "#3E8BD6"), chosen);
        assert_eq!(color_of(Adapter::Sqlite, ""), database);
    }
}
