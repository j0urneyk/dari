//! The Dari executable.

// Release builds are GUI apps on Windows; without this a console window opens alongside.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    dari::run()
}
