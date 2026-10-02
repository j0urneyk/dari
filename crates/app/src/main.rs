//! open-desk desktop application: shares this device's screen and controls remote devices.

// Release builds are GUI apps on Windows; without this a console window opens alongside.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod home;

use gpui_kit::*;
use tracing_subscriber::EnvFilter;

const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(960.),
    height: px(640.),
};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx| {
            gpui_kit::init(cx);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(WINDOW_SIZE, cx)),
                titlebar: Some(TitlebarOptions {
                    title: Some("open-desk".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| home::Home::new(window, cx))
            }) {
                tracing::error!(%error, "failed to open the home window");
                cx.quit();
            }
        });
}
