use open_desk_proto::{InputEvent, KeyCode, NamedKey, Os};

/// Translates shortcut modifiers between operating systems on the viewer side.
///
/// A Mac user controlling Windows expects ⌘C to copy, which Windows spells Ctrl+C; a Windows
/// user controlling a Mac expects Ctrl+C to do the same, which macOS spells ⌘C. When enabled,
/// the viewer's shortcut modifier is sent as the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifierMapping {
    from: NamedKey,
    to: NamedKey,
}

impl ModifierMapping {
    /// The mapping for a viewer on `viewer` controlling `host`, or `None` if both use the same
    /// shortcut modifier.
    pub fn between(viewer: Os, host: Os) -> Option<Self> {
        match (shortcut_modifier(viewer), shortcut_modifier(host)) {
            (from, to) if from == to => None,
            (from, to) => Some(Self { from, to }),
        }
    }

    pub fn apply(&self, event: InputEvent) -> InputEvent {
        match event {
            InputEvent::Key {
                key: KeyCode::Named(named),
                pressed,
            } => {
                let swapped = if named == self.from {
                    self.to
                } else if named == self.to {
                    self.from
                } else {
                    named
                };
                InputEvent::Key {
                    key: KeyCode::Named(swapped),
                    pressed,
                }
            }
            other => other,
        }
    }
}

fn shortcut_modifier(os: Os) -> NamedKey {
    match os {
        Os::MacOs => NamedKey::Meta,
        Os::Windows | Os::Linux | Os::Other => NamedKey::Control,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(key: NamedKey) -> InputEvent {
        InputEvent::Key {
            key: KeyCode::Named(key),
            pressed: true,
        }
    }

    #[test]
    fn mac_command_becomes_windows_control_and_back() {
        let mapping = ModifierMapping::between(Os::MacOs, Os::Windows).unwrap();
        assert_eq!(
            mapping.apply(named(NamedKey::Meta)),
            named(NamedKey::Control)
        );
        assert_eq!(
            mapping.apply(named(NamedKey::Control)),
            named(NamedKey::Meta)
        );
        assert_eq!(
            mapping.apply(named(NamedKey::Shift)),
            named(NamedKey::Shift)
        );

        let reverse = ModifierMapping::between(Os::Windows, Os::MacOs).unwrap();
        assert_eq!(
            reverse.apply(named(NamedKey::Control)),
            named(NamedKey::Meta)
        );
    }

    #[test]
    fn same_platform_needs_no_mapping() {
        assert!(ModifierMapping::between(Os::MacOs, Os::MacOs).is_none());
        assert!(ModifierMapping::between(Os::Windows, Os::Windows).is_none());
    }
}
