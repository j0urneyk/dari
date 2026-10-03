//! `dari-check`: one side of a cross-device session check.
//!
//! Run `dari-check host` on one machine and `dari-check view` on another. Neither side needs a
//! channel besides the session itself: the viewer follows a fixed scenario (every display, a few
//! pointer targets, the shortcut modifier, a clipboard round trip), and each side checks what it
//! can observe on its own machine. Each prints `PASS`/`FAIL` lines and exits non-zero on any
//! failure. `scripts/crosscheck/crosscheck.sh` drives both sides from a Mac.

#![allow(
    clippy::print_stdout,
    reason = "the check's report is its output, read by people and by the driving script"
)]

mod host;
mod scenario;
mod viewer;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(version, about = "One side of a cross-device dari session check")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Share this machine and check what the viewer's session did to it.
    Host(host::HostArgs),
    /// Connect to a `dari-check host` and run the scenario.
    View(viewer::ViewArgs),
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();
    dari_input::prepare_process();
    let cli = Cli::parse();
    let outcome = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| {
            runtime.block_on(async {
                match cli.command {
                    Command::Host(args) => host::run(args).await,
                    Command::View(args) => viewer::run(args).await,
                }
            })
        });
    match outcome {
        Ok(code) => code,
        Err(error) => {
            println!("FAIL {error:#}");
            println!("RESULT fail");
            ExitCode::FAILURE
        }
    }
}

/// This machine's name as the other side sees it.
fn device_name() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let name = name.strip_suffix(".local").unwrap_or(&name);
    let name: String = name.chars().filter(|c| !c.is_control()).take(32).collect();
    if name.trim().is_empty() {
        "dari-check".into()
    } else {
        name
    }
}
