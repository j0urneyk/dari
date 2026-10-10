//! `dari-service`: the `DariService` Windows service, and the commands that install and remove it.

use std::process::ExitCode;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() -> ExitCode {
    windows::main()
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    use std::io::Write;

    // The exit code reports the failure even if stderr is gone.
    let _ = writeln!(std::io::stderr(), "dari-service runs only on Windows");
    ExitCode::FAILURE
}
