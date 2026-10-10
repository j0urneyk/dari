use dari_proto::InputDesktop;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Observation {
    Named(String),
    /// The desktop couldn't be opened or named, which happens for a moment during a switch.
    Unreadable,
}

pub(crate) trait DesktopSource {
    fn poll(&mut self) -> Observation;
}

#[derive(Debug, Default)]
pub(crate) struct DesktopTracker {
    current: Option<InputDesktop>,
}

impl DesktopTracker {
    /// Returns the desktop to report: the first readable one, then each change. An unreadable
    /// observation keeps the current desktop.
    pub(crate) fn observe(&mut self, seen: Observation) -> Option<InputDesktop> {
        let Observation::Named(name) = seen else {
            return None;
        };
        let desktop = InputDesktop::from_name(&name);
        if self.current.as_ref() == Some(&desktop) {
            return None;
        }
        self.current = Some(desktop.clone());
        Some(desktop)
    }

    pub(crate) fn poll(&mut self, source: &mut impl DesktopSource) -> Option<InputDesktop> {
        self.observe(source.poll())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use dari_proto::DesktopName;

    use super::*;

    struct Script(VecDeque<Observation>);

    impl DesktopSource for Script {
        fn poll(&mut self) -> Observation {
            self.0.pop_front().unwrap_or(Observation::Unreadable)
        }
    }

    fn named(name: &str) -> Observation {
        Observation::Named(name.into())
    }

    fn reports(observations: Vec<Observation>) -> Vec<Option<InputDesktop>> {
        let count = observations.len();
        let mut source = Script(observations.into());
        let mut tracker = DesktopTracker::default();
        (0..count).map(|_| tracker.poll(&mut source)).collect()
    }

    #[test]
    fn the_starting_desktop_is_reported_once() {
        assert_eq!(
            reports(vec![named("Default"), named("Default"), named("default")]),
            [Some(InputDesktop::Default), None, None]
        );
    }

    #[test]
    fn a_lock_and_unlock_report_both_switches() {
        assert_eq!(
            reports(vec![
                named("Default"),
                named("Winlogon"),
                named("Winlogon"),
                named("Default"),
            ]),
            [
                Some(InputDesktop::Default),
                Some(InputDesktop::Winlogon),
                None,
                Some(InputDesktop::Default),
            ]
        );
    }

    #[test]
    fn an_unreadable_desktop_keeps_the_current_one() {
        assert_eq!(
            reports(vec![
                Observation::Unreadable,
                named("Default"),
                Observation::Unreadable,
                named("Default"),
                Observation::Unreadable,
                named("Winlogon"),
            ]),
            [
                None,
                Some(InputDesktop::Default),
                None,
                None,
                None,
                Some(InputDesktop::Winlogon),
            ]
        );
    }

    #[test]
    fn other_desktops_are_reported_by_name() {
        assert_eq!(
            reports(vec![
                named("Winlogon"),
                named("Screen-saver"),
                named("Screen-saver")
            ]),
            [
                Some(InputDesktop::Winlogon),
                Some(InputDesktop::Other(
                    DesktopName::parse("Screen-saver").unwrap()
                )),
                None,
            ]
        );
    }
}
