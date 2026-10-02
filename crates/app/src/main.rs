//! open-desk desktop application: shares this device's screen and controls remote devices.

// Release builds are GUI apps on Windows; without this a console window opens alongside.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cli;
mod config;
mod home;

use clap::{Parser, Subcommand};
use gpui_kit::*;
use tracing_subscriber::EnvFilter;

const WINDOW_SIZE: Size<Pixels> = Size {
    width: px(960.),
    height: px(640.),
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
    },
    /// Connect to a host without the GUI and report stream statistics.
    Connect {
        /// The host's address: IP, IP:port, or hostname.
        address: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    open_desk_input::prepare_process();

    let cli = Cli::parse();
    match cli.command {
        None => {
            run_gui();
            Ok(())
        }
        Some(command) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                match command {
                    Command::Host { port } => cli::host(port).await,
                    Command::Connect { address } => cli::connect(&address).await,
                }
            })
        }
    }
}

fn run_gui() {
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
