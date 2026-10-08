//! Mirrors the Mac's keyboard input source switches to the host as Hangul/English toggles.
//!
//! macOS consumes whatever switches its input source (Caps Lock, Ctrl+Space, the Globe key),
//! so the viewer never sees a key it could forward. It watches the selected input source
//! instead: every switch the user makes while a controlling viewer window is active becomes
//! one tap of the host's Hangul/English key.

use dari_proto::{InputEvent, KeyCode, NamedKey};

/// The id of the keyboard input source selected on this Mac (`com.apple.keylayout.ABC`,
/// `com.apple.inputmethod.Korean.2SetKorean`, …), or `None` when no window of this app is
/// taking text input.
pub(crate) fn selected_input_source() -> Option<String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSTextInputContext;

    let main_thread = MainThreadMarker::new()?;
    let context = NSTextInputContext::currentInputContext(main_thread)?;
    Some(context.selectedKeyboardInputSource()?.to_string())
}

/// The input source the user had while the viewer window has been active and controlling the
/// host. Switches are measured against it, so the source macOS restores on its own when the
/// window becomes active again never counts as one.
#[derive(Debug, Default)]
pub(crate) struct InputSourceTracker {
    recorded: Option<String>,
}

impl InputSourceTracker {
    /// `source` is selected now, with the viewer window `active` and `control` of the host
    /// allowed. Returns the Hangul/English tap to send when this is a switch the user made
    /// since the window became active.
    pub(crate) fn observe(
        &mut self,
        source: Option<&str>,
        active: bool,
        control: bool,
    ) -> Option<[InputEvent; 2]> {
        if !(active && control) {
            self.recorded = None;
            return None;
        }
        let source = source?;
        let switched = self
            .recorded
            .as_deref()
            .is_some_and(|recorded| recorded != source);
        self.recorded = Some(source.to_owned());
        switched.then_some(HANGUL_TAP)
    }
}

const HANGUL_TAP: [InputEvent; 2] = [
    InputEvent::Key {
        key: KeyCode::Named(NamedKey::HangulMode),
        pressed: true,
    },
    InputEvent::Key {
        key: KeyCode::Named(NamedKey::HangulMode),
        pressed: false,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    const ABC: &str = "com.apple.keylayout.ABC";
    const KOREAN: &str = "com.apple.inputmethod.Korean.2SetKorean";

    #[test]
    fn a_switch_while_active_taps_hangul_once() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.observe(Some(ABC), true, true), None);
        assert_eq!(tracker.observe(Some(KOREAN), true, true), Some(HANGUL_TAP));
        assert_eq!(tracker.observe(Some(ABC), true, true), Some(HANGUL_TAP));
    }

    #[test]
    fn the_same_source_reported_again_is_not_a_switch() {
        // macOS posts the selection notification twice for one Ctrl+Space.
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.observe(Some(ABC), true, true), None);
        assert_eq!(tracker.observe(Some(KOREAN), true, true), Some(HANGUL_TAP));
        assert_eq!(tracker.observe(Some(KOREAN), true, true), None);
    }

    #[test]
    fn the_source_found_on_activation_is_adopted_silently() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.observe(Some(ABC), true, true), None);
        // Switched while another app was active, then came back: macOS reports the new
        // source around the activation, before and after it.
        assert_eq!(tracker.observe(None, false, true), None);
        assert_eq!(tracker.observe(Some(KOREAN), false, true), None);
        assert_eq!(tracker.observe(Some(KOREAN), true, true), None);
        assert_eq!(tracker.observe(Some(KOREAN), true, true), None);
        assert_eq!(tracker.observe(Some(ABC), true, true), Some(HANGUL_TAP));
    }

    #[test]
    fn an_unknown_source_changes_nothing() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.observe(Some(ABC), true, true), None);
        assert_eq!(tracker.observe(None, true, true), None);
        assert_eq!(tracker.observe(Some(KOREAN), true, true), Some(HANGUL_TAP));
    }

    #[test]
    fn view_only_sessions_send_nothing() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.observe(Some(ABC), true, false), None);
        assert_eq!(tracker.observe(Some(KOREAN), true, false), None);
        // Control granted later: the first source seen is the baseline, not a switch.
        assert_eq!(tracker.observe(Some(ABC), true, true), None);
        assert_eq!(tracker.observe(Some(KOREAN), true, true), Some(HANGUL_TAP));
    }
}
