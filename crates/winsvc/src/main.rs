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

    let _ = writeln!(std::io::stderr(), "dari-service runs only on Windows");
    ExitCode::FAILURE
}
