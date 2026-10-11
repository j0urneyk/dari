use std::ffi::OsString;

use dari_proto::{HelperPipeName, SecureDesktopControl};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Service,
    Install,
    Uninstall,
    /// `policy on|off`. Needs an administrator.
    Policy(SecureDesktopControl),
    /// Started only by the service: `helper <pipe> <input|no-input> <app process handle>`.
    Helper(HelperArgs),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HelperArgs {
    pub(crate) pipe: HelperPipeName,
    pub(crate) input: bool,
    pub(crate) app: usize,
}

impl HelperArgs {
    pub(crate) fn to_arguments(&self) -> String {
        let input = if self.input { "input" } else { "no-input" };
        format!("helper {} {input} {}", self.pipe.as_str(), self.app)
    }
}

impl Command {
    pub(crate) fn parse(arguments: &[OsString]) -> Option<Self> {
        let arguments: Vec<&str> = arguments
            .iter()
            .map(|argument| argument.to_str())
            .collect::<Option<_>>()?;
        match arguments[..] {
            ["service"] => Some(Self::Service),
            ["install"] => Some(Self::Install),
            ["uninstall"] => Some(Self::Uninstall),
            ["policy", "on"] => Some(Self::Policy(SecureDesktopControl::On)),
            ["policy", "off"] => Some(Self::Policy(SecureDesktopControl::Off)),
            ["helper", pipe, input, app] => Some(Self::Helper(HelperArgs {
                pipe: HelperPipeName::parse(pipe).ok()?,
                input: match input {
                    "input" => true,
                    "no-input" => false,
                    _ => return None,
                },
                app: parse_handle(app)?,
            })),
            _ => None,
        }
    }
}

fn parse_handle(value: &str) -> Option<usize> {
    if value.starts_with('0') || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use dari_proto::PIPE_RANDOM_BYTES;

    use super::*;

    fn parse(arguments: &[&str]) -> Option<Command> {
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        Command::parse(&arguments)
    }

    fn helper(input: bool, app: usize) -> HelperArgs {
        HelperArgs {
            pipe: HelperPipeName::from_random([7; PIPE_RANDOM_BYTES]),
            input,
            app,
        }
    }

    #[test]
    fn parses_exactly_one_known_command() {
        assert_eq!(parse(&["service"]), Some(Command::Service));
        assert_eq!(parse(&["install"]), Some(Command::Install));
        assert_eq!(parse(&["uninstall"]), Some(Command::Uninstall));
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["Install"]), None);
        assert_eq!(parse(&["install", "service"]), None);
    }

    #[test]
    fn policy_takes_exactly_on_or_off() {
        assert_eq!(
            parse(&["policy", "on"]),
            Some(Command::Policy(SecureDesktopControl::On))
        );
        assert_eq!(
            parse(&["policy", "off"]),
            Some(Command::Policy(SecureDesktopControl::Off))
        );
        for bad in [
            &["policy"][..],
            &["policy", "On"],
            &["policy", "1"],
            &["policy", "0"],
            &["policy", "on", "off"],
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn helper_arguments_round_trip() {
        for args in [helper(true, 4), helper(false, 0x2a4)] {
            let line = args.to_arguments();
            let words: Vec<&str> = line.split(' ').collect();
            assert_eq!(parse(&words), Some(Command::Helper(args)), "{line}");
        }
    }

    #[test]
    fn helper_arguments_are_checked() {
        let pipe = HelperPipeName::from_random([7; PIPE_RANDOM_BYTES]);
        let pipe = pipe.as_str();
        assert!(parse(&["helper", pipe, "input", "676"]).is_some());
        for bad in [
            ["helper", "dari-service", "input", "676"],
            ["helper", r"\\.\pipe\dari-service", "input", "676"],
            ["helper", pipe, "yes", "676"],
            ["helper", pipe, "input", "0"],
            ["helper", pipe, "input", "0676"],
            ["helper", pipe, "input", "-4"],
            ["helper", pipe, "input", "+4"],
            ["helper", pipe, "input", "0x2a4"],
            ["helper", pipe, "input", ""],
            ["helper", pipe, "input", "99999999999999999999999"],
        ] {
            assert_eq!(parse(&bad), None, "{bad:?}");
        }
        assert_eq!(parse(&["helper", pipe, "input"]), None);
        assert_eq!(parse(&["helper", pipe, "input", "4", "extra"]), None);
    }
}
