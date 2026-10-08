//! macOS consumes whatever switches its input source (Caps Lock, Ctrl+Space, the Globe key),
//! so the viewer never sees a key it could forward.

use dari_proto::{InputEvent, KeyCode, NamedKey};

pub(crate) fn selected_input_source() -> Option<String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSTextInputContext;

    let main_thread = MainThreadMarker::new()?;
    let context = NSTextInputContext::currentInputContext(main_thread)?;
    Some(context.selectedKeyboardInputSource()?.to_string())
}

#[derive(Debug, Default)]
pub(crate) struct InputSourceTracker {
    baseline_since_activation: Option<String>,
}

impl InputSourceTracker {
    pub(crate) fn deactivate(&mut self) {
        self.baseline_since_activation = None;
    }

    pub(crate) fn select(&mut self, source: &str, control: bool) -> Option<[InputEvent; 2]> {
        let switched = self
            .baseline_since_activation
            .replace(source.to_owned())
            .is_some_and(|baseline| baseline != source);
        (switched && control).then_some(HANGUL_TAP)
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
        assert_eq!(tracker.select(ABC, true), None);
        assert_eq!(tracker.select(KOREAN, true), Some(HANGUL_TAP));
        assert_eq!(tracker.select(ABC, true), Some(HANGUL_TAP));
    }

    #[test]
    fn the_same_source_reported_again_is_not_a_switch() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.select(ABC, true), None);
        assert_eq!(tracker.select(KOREAN, true), Some(HANGUL_TAP));
        assert_eq!(tracker.select(KOREAN, true), None);
    }

    #[test]
    fn the_source_found_on_activation_is_adopted_silently() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.select(ABC, true), None);
        tracker.deactivate();
        assert_eq!(tracker.select(KOREAN, true), None);
        assert_eq!(tracker.select(KOREAN, true), None);
        assert_eq!(tracker.select(ABC, true), Some(HANGUL_TAP));
    }

    #[test]
    fn view_only_sessions_send_nothing() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.select(ABC, false), None);
        assert_eq!(tracker.select(KOREAN, false), None);
    }

    #[test]
    fn the_first_switch_after_control_is_granted_taps_hangul() {
        let mut tracker = InputSourceTracker::default();
        assert_eq!(tracker.select(ABC, false), None);
        assert_eq!(tracker.select(KOREAN, true), Some(HANGUL_TAP));
    }
}
