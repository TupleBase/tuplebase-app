// No console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod conn_form;
mod results;
mod workspace;

use gpui_kit::component::Theme;
use gpui_kit::*;
use workspace::Workspace;

actions!(tuplebase, [Quit]);

#[cfg(target_os = "macos")]
const QUIT_KEY: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const QUIT_KEY: &str = "ctrl-q";

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            // Must run before any gpui-kit component is used.
            gpui_kit::init(cx);

            workspace::bind_keys(cx);
            cx.bind_keys([KeyBinding::new(QUIT_KEY, Quit, None)]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.set_menus([Menu::new(tuplebase_core::PRODUCT_NAME)
                .items([MenuItem::action("Quit TupleBase", Quit)])]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(tuplebase_core::PRODUCT_NAME.into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(780.)),
                    cx,
                ))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
                cx.new(|cx| Workspace::new(window, cx))
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
