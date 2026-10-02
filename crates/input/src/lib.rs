//! Applies remote keyboard and pointer input to the local machine.
//!
//! [`InputSession`] validates and tracks input from one remote session: it maps normalized
//! pointer positions onto the captured display and remembers which keys and buttons are held,
//! so ending the session never leaves a key stuck down. The OS work happens in an
//! [`InputBackend`]; [`EnigoBackend`] is the real one.

mod backend;
mod keymap;
mod session;

pub use backend::{EnigoBackend, InputBackend, RecordedAction, RecordingBackend};
pub use keymap::ModifierMapping;
pub use session::{DisplayGeometry, InputSession, MAX_HELD_INPUTS};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum InjectError {
    /// macOS Accessibility permission is missing.
    #[error("permission to control this computer has not been granted")]
    PermissionDenied,
    #[error("{0} cannot be pressed on this computer")]
    Unsupported(String),
    #[error("input injection failed: {0}")]
    Backend(String),
}

/// Prepares process-wide input state. Call once at startup, before creating windows.
///
/// On Windows this makes the process per-monitor DPI aware, so pointer coordinates are physical
/// pixels on every monitor, matching the captured frames. Harmless if already DPI aware.
pub fn prepare_process() {
    #[cfg(windows)]
    backend::win32::enable_per_monitor_dpi_awareness();
}
