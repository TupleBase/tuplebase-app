//! Database icons and the app mark.

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

fn glyph(adapter: Adapter) -> &'static [u8] {
    match adapter {
        Adapter::Postgres => include_bytes!("../../../assets/logos/postgresql.svg"),
        Adapter::Mariadb => include_bytes!("../../../assets/logos/mariadb.svg"),
        Adapter::Mysql => include_bytes!("../../../assets/logos/mysql.svg"),
        Adapter::Sqlite => include_bytes!("../../../assets/logos/sqlite.svg"),
    }
}

/// Each database's brand color, deep enough to carry a white mark on either theme.
fn database_color(adapter: Adapter) -> u32 {
    match adapter {
        Adapter::Postgres => 0x336791,
        Adapter::Mariadb => 0xA0522D,
        Adapter::Mysql => 0x00758F,
        Adapter::Sqlite => 0x0F80CC,
    }
}

fn parse_color(color: &str) -> Option<u32> {
    u32::from_str_radix(color.strip_prefix('#')?, 16).ok()
}

/// The color a connection shows: its own choice, or else its database's color.
pub fn color_of(adapter: Adapter, color: &str) -> Hsla {
    rgb(parse_color(color).unwrap_or_else(|| database_color(adapter))).into()
}

/// A database's icon: its mark on a tile of its brand color. A connection's own color
/// never changes it, so the icon always says which database this is.
pub fn icon(adapter: Adapter, size: f32) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(size * 0.22))
        .bg(rgb(database_color(adapter)))
        .child(
            Icon::default()
                .data(glyph(adapter))
                .with_size(px((size * 0.78).round()))
                .text_color(white()),
        )
}

pub fn brand(size: f32) -> Img {
    img(BRAND.clone()).size(px(size))
}

#[cfg(test)]
mod tests {
    use super::color_of;
    use gpui_kit::{Hsla, rgb};
    use tuplebase_core::config::Adapter;

    #[test]
    fn a_connection_color_replaces_the_database_color() {
        let chosen: Hsla = rgb(0x3E8BD6).into();
        let database: Hsla = rgb(0x0F80CC).into();
        assert_eq!(color_of(Adapter::Sqlite, "#3E8BD6"), chosen);
        assert_eq!(color_of(Adapter::Sqlite, ""), database);
    }
}
