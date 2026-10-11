// No console: a console build started by the service as SYSTEM opens a console window on the
// user's desktop.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

#[cfg_attr(not(windows), allow(dead_code))]
mod channel;
#[cfg_attr(not(windows), allow(dead_code))]
mod command;
#[cfg_attr(not(windows), allow(dead_code))]
mod dxgi_result;
#[cfg_attr(not(windows), allow(dead_code))]
mod injector;
#[cfg_attr(not(windows), allow(dead_code))]
mod limiter;
#[cfg_attr(not(windows), allow(dead_code))]
mod pointer;
#[cfg_attr(not(windows), allow(dead_code))]
mod screen;
#[cfg_attr(not(windows), allow(dead_code))]
mod slot;
#[cfg_attr(not(windows), allow(dead_code))]
mod tracker;

#[cfg(windows)]
mod frames;
#[cfg(windows)]
mod helper;
#[cfg(windows)]
mod scm;
#[cfg(windows)]
mod service;
#[cfg(windows)]
#[allow(unsafe_code)]
mod win32;

#[cfg(windows)]
fn main() -> ExitCode {
    use std::ffi::OsString;

    use command::Command;

    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(command) = Command::parse(&arguments) else {
        return fail("usage: dari-service <install|uninstall|service>");
    };
    let result = match command {
        Command::Service => scm::run_service(),
        Command::Install => scm::install(),
        Command::Uninstall => scm::uninstall(),
        Command::Helper(args) => return helper::run(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => fail(&failure.to_string()),
    }
}

#[cfg(windows)]
fn fail(message: &str) -> ExitCode {
    use std::io::Write;

    let _ = writeln!(std::io::stderr(), "dari-service: {message}");
    if let Some(log) = win32::EventLog::open() {
        log.error(&format!("dari-service: {message}"));
    }
    ExitCode::FAILURE
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    use std::io::Write;

    let _ = writeln!(std::io::stderr(), "dari-service runs only on Windows");
    ExitCode::FAILURE
}

#[cfg(all(test, windows))]
mod test_support {
    use std::sync::atomic::{AtomicU32, Ordering};

    use dari_proto::PIPE_CLIENT_RIGHTS;

    use crate::win32::Pipe;

    pub(crate) fn test_pipe_path() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            r"\\.\pipe\dari-winsvc-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A pipe with a name no other test uses, which any local user may open as a client.
    #[expect(clippy::unwrap_used, reason = "a test helper")]
    pub(crate) fn test_pipe() -> (Pipe, String) {
        let path = test_pipe_path();
        let sddl = format!("D:P(A;;{PIPE_CLIENT_RIGHTS:#x};;;WD)");
        let pipe = Pipe::create_instances(&path, &sddl, 1).unwrap().remove(0);
        (pipe, path)
    }
}
