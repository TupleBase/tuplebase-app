//! Database icons and the app mark.

use std::sync::{Arc, LazyLock};

use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _};
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

/// Each database's own color, in a shade that reads on the current theme.
fn default_color(adapter: Adapter, dark: bool) -> u32 {
    match (adapter, dark) {
        (Adapter::Postgres, false) => 0x336791,
        (Adapter::Postgres, true) => 0x7AA7E0,
        (Adapter::Mariadb, false) => 0x003545,
        (Adapter::Mariadb, true) => 0xC0765A,
        (Adapter::Mysql, false) => 0x00758F,
        (Adapter::Mysql, true) => 0x4FB3CE,
        (Adapter::Sqlite, false) => 0x0F80CC,
        (Adapter::Sqlite, true) => 0x5AB3E8,
    }
}

/// The color a connection shows: its own choice, or else its database's color.
pub fn color_of(adapter: Adapter, color: &str, cx: &App) -> Hsla {
    let chosen = color
        .strip_prefix('#')
        .and_then(|hex| u32::from_str_radix(hex, 16).ok());
    rgb(chosen.unwrap_or_else(|| default_color(adapter, cx.theme().mode.is_dark()))).into()
}

/// A database's icon, shown small and inline like a file icon in an editor.
pub fn icon(adapter: Adapter, color: &str, size: f32, cx: &App) -> Icon {
    Icon::default()
        .data(glyph(adapter))
        .with_size(px(size))
        .text_color(color_of(adapter, color, cx))
}

pub fn brand(size: f32) -> Img {
    img(BRAND.clone()).size(px(size))
}
