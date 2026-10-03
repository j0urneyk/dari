//! Weak-links Core Audio on macOS.
//!
//! cpal's system audio capture calls Core Audio's process tap functions, which only exist from
//! macOS 14.2. Linked normally, the app would fail to launch on the older macOS versions it
//! supports (12 and later). Weakly linked, those symbols are simply absent there, and
//! `dari-media` never calls them below macOS 14.6.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
    }
}
