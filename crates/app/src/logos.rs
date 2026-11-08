//! Official database logos and the app mark.

use std::sync::{Arc, LazyLock};

use gpui_kit::*;
use tuplebase_core::config::Adapter;

fn svg_image(bytes: &[u8]) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec()))
}

static POSTGRES: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/postgresql.svg")));
static MARIADB: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/mariadb.svg")));
static MYSQL: LazyLock<Arc<Image>> =
    LazyLock::new(|| svg_image(include_bytes!("../../../assets/logos/mysql.svg")));
static BRAND: LazyLock<Arc<Image>> = LazyLock::new(|| {
    Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../../../assets/icon/icon-256.png").to_vec(),
    ))
});

fn logo(adapter: Adapter) -> Arc<Image> {
    match adapter {
        Adapter::Postgres => POSTGRES.clone(),
        Adapter::Mariadb => MARIADB.clone(),
        Adapter::Mysql => MYSQL.clone(),
    }
}

/// A logo on a white tile, so its official colors read on light and dark themes alike.
pub fn tile(adapter: Adapter, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(size * 0.24))
        .bg(rgb(0xFFFFFF))
        .child(img(logo(adapter)).size(px(size * 0.82)))
}

pub fn brand(size: f32) -> Img {
    img(BRAND.clone()).size(px(size))
}
