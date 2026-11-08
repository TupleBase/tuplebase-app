//! Official database logos and the app mark.

use std::sync::{Arc, LazyLock};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::*;
use tuplebase_core::config::Adapter;

fn svg_image(bytes: &[u8]) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec()))
}

static POSTGRES: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/postgresql.svg")));
static MARIADB: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/mariadb.svg")));
static MARIADB_DARK: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/mariadb-dark.svg")));
static MYSQL: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/mysql.svg")));
static MYSQL_DARK: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/mysql-dark.svg")));
static BRAND: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../../../assets/icon/icon-256.png").to_vec(),
    ))
});

fn logo(adapter: Adapter, dark: bool) -> Arc<Image> {
    match (adapter, dark) {
        (Adapter::Postgres, _) => POSTGRES.clone(),
        (Adapter::Mariadb, false) => MARIADB.clone(),
        (Adapter::Mariadb, true) => MARIADB_DARK.clone(),
        (Adapter::Mysql, false) => MYSQL.clone(),
        (Adapter::Mysql, true) => MYSQL_DARK.clone(),
    }
}

/// A database's logo, shown small and inline like a file icon in an editor.
pub fn icon(adapter: Adapter, size: f32, cx: &App) -> Img {
    img(logo(adapter, cx.theme().mode.is_dark()))
        .size(px(size))
        .flex_none()
}

pub fn brand(size: f32) -> Img {
    img(BRAND.clone()).size(px(size))
}
