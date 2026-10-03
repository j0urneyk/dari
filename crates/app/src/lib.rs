//! Dari desktop application: shares this device's screen and controls remote devices.
//!
//! The binary calls [`run`]. The library form exists so the GUI can be tested headlessly
//! (`tests/gui.rs`) through [`test_support`].

mod backdrop;
mod cli;
mod config;
mod home;
mod keymap;
mod permissions;
mod runtime;
mod settings;
mod state;
mod style;
mod text;
mod video_layout;
mod viewer;

use clap::{Parser, Subcommand};
use gpui_kit::*;
use tracing_subscriber::EnvFilter;

const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(960.),
    height: px(660.),
};
const MIN_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(720.),
    height: px(480.),
};

#[derive(Debug, Parser)]
#[command(version, about = "Remote desktop for macOS and Windows")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Share this computer's screen without the GUI.
    Host {
        /// UDP port to listen on.
        #[arg(long, default_value_t = config::DEFAULT_PORT)]
        port: u16,
        /// Relay server to register with, so viewers elsewhere can connect by ID.
        #[arg(long)]
        relay: Option<String>,
    },
    /// Connect to a host without the GUI and report stream statistics.
    Connect {
        /// The host's address (IP, IP:port, hostname) or, with --relay, its nine-digit ID.
        address: String,
        /// Relay server for connecting by ID.
        #[arg(long, default_value = "")]
        relay: String,
    },
}

/// Runs the app: the GUI, or a headless subcommand when one is given.
pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    dari_input::prepare_process();

    let cli = Cli::parse();
    match cli.command {
        None => run_gui(),
        Some(command) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                match command {
                    Command::Host { port, relay } => cli::host(port, relay).await,
                    Command::Connect { address, relay } => cli::connect(&address, &relay).await,
                }
            })
        }
    }
}

fn run_gui() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let data_directory = config::data_directory()?;
    gpui_kit::application()
        .with_assets(style::AppAssets)
        .run(move |cx| {
            init_ui(cx);
            runtime::TokioRuntime::install(runtime, cx);
            state::AppState::install(data_directory, cx);
            style::sync_theme(cx);

            let options = style::window_options("Dari", WINDOW_SIZE, Some(MIN_WINDOW_SIZE), cx);
            if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| home::Home::new(window, cx))
            }) {
                tracing::error!(%error, "failed to open the home window");
                cx.quit();
            }
            // Bring the window to the front, even when launched from a terminal.
            cx.activate(true);
        });
    Ok(())
}

/// Initializes gpui-kit, Dari's theme, and the viewer's key bindings.
fn init_ui(cx: &mut App) {
    gpui_kit::init(cx);
    style::init(cx);
    viewer::init(cx);
}

/// Entry points for the headless GUI tests. Not a stable API.
#[doc(hidden)]
pub mod test_support {
    pub use crate::home::Home;
    pub use crate::runtime::TokioRuntime;
    pub use crate::settings::Settings;
    pub use crate::state::AppState;
    pub use crate::style::{AppAssets, apply as apply_theme};
    pub use crate::viewer::{ViewerView, open_viewer_window};

    /// Everything the GUI needs before opening a window, as the app does at startup.
    pub fn init(cx: &mut gpui_kit::App) {
        crate::init_ui(cx);
    }
}
