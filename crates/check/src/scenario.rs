//! What host and viewer agree on without a side channel: where the pointer goes, which
//! clipboard texts travel, and how each side reports its verdict.

use std::fmt::Display;
use std::process::ExitCode;

use dari_media::DisplayInfo;
use dari_proto::{NamedKey, Os, PointerPosition};

/// Normalized pointer targets the viewer sends on every display, in order.
pub(crate) const POINTER_TARGETS: [(f64, f64); 3] = [(0.25, 0.25), (0.75, 0.6), (0.5, 0.5)];

/// How far, in OS pointer units, a landed pointer may be from its target.
pub(crate) const POINTER_TOLERANCE: i32 = 2;

/// What the host user answers when the viewer asks to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Approval {
    /// Allow control: input and clipboard reach the host, and the host ends the session after
    /// the clipboard round trip.
    Allow,
    /// View only: no input or clipboard may reach the host, and the viewer disconnects.
    ViewOnly,
}

/// Clipboard text the viewer copies; non-ASCII so text encoding is exercised too.
pub(crate) fn viewer_token(nonce: &str) -> String {
    format!("dari-check 다리 viewer→host {nonce}")
}

/// Clipboard text the host copies once it has received the viewer's.
pub(crate) fn host_token(nonce: &str) -> String {
    format!("dari-check 다리 host→viewer {nonce}")
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "fractions are within 0..=1"
)]
pub(crate) fn pointer_position((fx, fy): (f64, f64)) -> PointerPosition {
    PointerPosition {
        x: (f64::from(u16::MAX) * fx).round() as u16,
        y: (f64::from(u16::MAX) * fy).round() as u16,
    }
}

/// Where a pointer target should land on `display`, in OS pointer coordinates. Derived from
/// the display's own bounds, so a mismatch between capture and input coordinate spaces (for
/// example on a scaled display) shows up as a miss.
#[expect(
    clippy::cast_possible_truncation,
    reason = "display sizes are far below i32::MAX"
)]
pub(crate) fn expected_landing(display: &DisplayInfo, (fx, fy): (f64, f64)) -> (i32, i32) {
    (
        display.x + (f64::from(display.width.saturating_sub(1)) * fx).round() as i32,
        display.y + (f64::from(display.height.saturating_sub(1)) * fy).round() as i32,
    )
}

pub(crate) fn lands_near(landed: (i32, i32), expected: (i32, i32)) -> bool {
    (landed.0 - expected.0).abs() <= POINTER_TOLERANCE
        && (landed.1 - expected.1).abs() <= POINTER_TOLERANCE
}

/// The modifier `os` uses for shortcuts such as copy: ⌘ on macOS, Ctrl elsewhere.
pub(crate) fn shortcut_modifier(os: Os) -> NamedKey {
    match os {
        Os::MacOs => NamedKey::Meta,
        Os::Windows | Os::Linux | Os::Other => NamedKey::Control,
    }
}

/// Collects check results and prints each as it is decided.
#[derive(Debug, Default)]
pub(crate) struct Verdict {
    failures: usize,
}

impl Verdict {
    pub(crate) fn check(&mut self, passed: bool, what: impl Display) {
        if passed {
            println!("PASS {what}");
        } else {
            println!("FAIL {what}");
            self.failures += 1;
        }
    }

    pub(crate) fn fail(&mut self, what: impl Display) {
        self.check(false, what);
    }

    pub(crate) fn finish(self) -> ExitCode {
        if self.failures == 0 {
            println!("RESULT pass");
            ExitCode::SUCCESS
        } else {
            println!("RESULT fail ({} failed)", self.failures);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use dari_input::DisplayGeometry;

    use super::*;

    #[test]
    fn expected_landings_match_how_the_host_maps_positions() {
        let display = DisplayInfo {
            id: 2,
            name: "secondary".into(),
            x: -1920,
            y: 120,
            width: 1920,
            height: 1080,
            scale_factor: 1.5,
            is_primary: false,
        };
        let geometry = DisplayGeometry {
            x: display.x,
            y: display.y,
            width: display.width,
            height: display.height,
        };
        for target in POINTER_TARGETS {
            let mapped = geometry.to_os(pointer_position(target));
            assert!(
                lands_near(mapped, expected_landing(&display, target)),
                "{target:?}: host maps to {mapped:?}, check expects {:?}",
                expected_landing(&display, target)
            );
        }
    }
}
