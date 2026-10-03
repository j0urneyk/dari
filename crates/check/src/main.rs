//! `dari-check`: one side of a cross-device session check.
//!
//! Run `dari-check host` on one machine and `dari-check view` on another. Neither side needs a
//! channel besides the session itself: the viewer follows a fixed scenario (every display, a few
//! pointer targets, then clicking, scrolling, typing, and copying in an input window the host
//! opens, and a clipboard round trip), and each side checks what it can observe on its own
//! machine. Each prints `PASS`/`FAIL` lines and exits non-zero on any
//! failure. `scripts/crosscheck/crosscheck.sh` drives both sides from a Mac.

#![allow(
    clippy::print_stdout,
    reason = "the check's report is its output, read by people and by the driving script"
)]

mod host;
mod probe;
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
    let outcome = match Cli::parse().command {
        Command::Host(args) if args.wants_probe() => host_with_probe(args),
        Command::Host(args) => block_on(host::run(args, None)),
        Command::View(args) => block_on(viewer::run(args)),
    };
    match outcome {
        Ok(code) => code,
        Err(error) => {
            println!("FAIL {error:#}");
            println!("RESULT fail");
            ExitCode::FAILURE
        }
    }
}

fn block_on(check: impl Future<Output = anyhow::Result<ExitCode>>) -> anyhow::Result<ExitCode> {
    tokio::runtime::Runtime::new()?.block_on(check)
}

/// Hosts with the input window open. Windows must be made on the main thread (macOS insists), so
/// the window's event loop runs there and the host on another thread; the host ends the loop
/// when it is done.
fn host_with_probe(args: host::HostArgs) -> anyhow::Result<ExitCode> {
    let event_loop = winit::event_loop::EventLoop::<probe::Finished>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let log = probe::SharedProbe::default();
    let host_log = log.clone();
    let host = std::thread::spawn(move || {
        let outcome = block_on(host::run(args, Some(host_log)));
        let _closing = proxy.send_event(probe::Finished);
        outcome
    });
    event_loop.run_app(&mut probe::Probe::new(log))?;
    host.join()
        .map_err(|_panic| anyhow::anyhow!("the host thread panicked"))?
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
