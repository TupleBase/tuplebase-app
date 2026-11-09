// No console window for release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod completions;
mod conn_form;
mod logos;
mod results;
mod runtime;
mod schema;
mod theme;
mod workspace;

use gpui_kit::component::{GlobalState, Theme, TitleBar};
use gpui_kit::*;
use workspace::{About, AddMcpToAgents, CheckForUpdates, OpenSettings, Workspace};

actions!(tuplebase, [Quit]);

#[cfg(target_os = "macos")]
const QUIT_KEY: &str = "cmd-q";
#[cfg(not(target_os = "macos"))]
const QUIT_KEY: &str = "ctrl-q";

fn app_menus() -> Vec<Menu> {
    vec![Menu::new(tuplebase_core::PRODUCT_NAME).items([
        MenuItem::action("About TupleBase", About),
        MenuItem::action("Check for Updates…", CheckForUpdates),
        MenuItem::separator(),
        MenuItem::action("Settings…", OpenSettings),
        MenuItem::action("Add MCP Server to Agents", AddMcpToAgents),
        MenuItem::separator(),
        MenuItem::action("Quit TupleBase", Quit),
    ])]
}

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            // Must run before any gpui-kit component is used.
            gpui_kit::init(cx);

            theme::init(cx);
            workspace::bind_keys(cx);
            cx.bind_keys([KeyBinding::new(QUIT_KEY, Quit, None)]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.set_menus(app_menus());
            // Windows and Linux have no native menu bar; the title bar draws these.
            GlobalState::global_mut(cx)
                .set_app_menus(app_menus().into_iter().map(Menu::owned).collect());
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(tuplebase_core::PRODUCT_NAME.into()),
                    traffic_light_position: Some(point(px(13.), px(13.))),
                    ..TitleBar::title_bar_options()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1200.), px(780.)),
                    cx,
                ))),
                ..TitleBar::window_options()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                Theme::sync_system_appearance(Some(window), cx);
                let workspace = cx.new(|cx| Workspace::new(window, cx));
                workspace::register_menu_actions(&workspace, window.window_handle(), cx);
                workspace
            })
            .expect("failed to open window");
            cx.activate(true);
        });
}
