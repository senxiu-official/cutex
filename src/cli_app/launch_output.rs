use std::fmt;

/// Controls ownership of stdout while Cutex is wrapping another CLI.
///
/// Human launches retain the historical presentation on stdout. A child that
/// explicitly declares an `exec --json` protocol owns stdout byte-for-byte;
/// Cutex diagnostics are routed to stderr and are never parsed or filtered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaunchOutput {
    Human,
    MachineReadableStdout,
}

impl LaunchOutput {
    pub(crate) fn for_child_args(args: &[String]) -> Self {
        if child_declares_json_exec(args) {
            Self::MachineReadableStdout
        } else {
            Self::Human
        }
    }

    pub(crate) fn including_child_args(self, args: &[String]) -> Self {
        if self == Self::MachineReadableStdout || child_declares_json_exec(args) {
            Self::MachineReadableStdout
        } else {
            Self::Human
        }
    }

    pub(crate) fn line(self, args: fmt::Arguments<'_>) {
        match self {
            Self::Human => println!("{args}"),
            Self::MachineReadableStdout => eprintln!("{args}"),
        }
    }

    pub(crate) fn is_machine_readable(self) -> bool {
        self == Self::MachineReadableStdout
    }
}

fn child_declares_json_exec(args: &[String]) -> bool {
    args.iter()
        .position(|arg| arg == "exec")
        .is_some_and(|exec| args[exec + 1..].iter().any(|arg| arg == "--json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn only_explicit_json_exec_reserves_child_stdout() {
        assert_eq!(
            LaunchOutput::for_child_args(&args(&["exec", "--json", "Hi."])),
            LaunchOutput::MachineReadableStdout
        );
        assert_eq!(
            LaunchOutput::for_child_args(&args(&["--profile", "review", "exec", "Hi.", "--json"])),
            LaunchOutput::MachineReadableStdout
        );
        for values in [
            &[][..],
            &["exec"][..],
            &["--json"][..],
            &["--json", "exec"][..],
            &["resume", "--json"][..],
        ] {
            assert_eq!(
                LaunchOutput::for_child_args(&args(values)),
                LaunchOutput::Human
            );
        }
    }

    #[test]
    fn effective_default_args_can_upgrade_but_never_downgrade_machine_output() {
        assert_eq!(
            LaunchOutput::Human.including_child_args(&args(&["exec", "--json"])),
            LaunchOutput::MachineReadableStdout
        );
        assert_eq!(
            LaunchOutput::MachineReadableStdout.including_child_args(&[]),
            LaunchOutput::MachineReadableStdout
        );
    }
}
