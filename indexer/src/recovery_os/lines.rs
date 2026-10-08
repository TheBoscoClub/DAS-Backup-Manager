//! The machine lines `scripts/recovery-os-vm.sh` prints during a session
//! (`PROGRESS`, `OUTPUT`, `RESULT`, `DRIVE`), parsed for the helper's
//! progress signals.
//!
//! A line that does not fit its grammar exactly is [`SessionLine::Other`],
//! whole -- never partially parsed, never defaulted. A `RESULT` whose exit is
//! not a number is `Other`, not exit 0: a guess here would turn a broken line
//! into a success.

/// The session's steps in the order the script runs them.
pub const STEPS: [&str; 18] = [
    "preflight",
    "start",
    "wait",
    "boot",
    "egress",
    "snapshot",
    "guard-lift",
    "keyrings",
    "upgrade",
    "packages",
    "initramfs",
    "verify-btrbk",
    "verify-boot",
    "guard-engage",
    "reboot",
    "kernel",
    "poweroff",
    "giveback",
];

/// One line of the script's stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLine<'a> {
    /// `event` is `start`, `ok` or `fail`; `message` is empty when absent.
    Progress {
        label: &'a str,
        step: &'a str,
        event: &'a str,
        message: &'a str,
    },
    Output {
        label: &'a str,
        step: &'a str,
        text: &'a str,
    },
    Result {
        label: &'a str,
        exit: i32,
        outcome: &'a str,
    },
    /// `exit` is `None` for `skipped`.
    Drive {
        label: &'a str,
        exit: Option<i32>,
    },
    Other(&'a str),
}

/// A plain decimal exit status: optional `-`, digits, nothing else (not `+5`, not ` 5`).
fn exit_of(s: &str) -> Option<i32> {
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn token(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

fn parse_known(line: &str) -> Option<SessionLine<'_>> {
    if let Some(rest) = line.strip_prefix("PROGRESS ") {
        let mut it = rest.splitn(4, ' ');
        let (label, step, event) = (token(it.next()?)?, token(it.next()?)?, it.next()?);
        if !matches!(event, "start" | "ok" | "fail") {
            return None;
        }
        return Some(SessionLine::Progress {
            label,
            step,
            event,
            message: it.next().unwrap_or(""),
        });
    }
    if let Some(rest) = line.strip_prefix("OUTPUT ") {
        let mut it = rest.splitn(3, ' ');
        let (label, step) = (token(it.next()?)?, token(it.next()?)?);
        return Some(SessionLine::Output {
            label,
            step,
            text: it.next()?,
        });
    }
    if let Some(rest) = line.strip_prefix("RESULT ") {
        let mut it = rest.splitn(3, ' ');
        let label = token(it.next()?)?;
        let exit = exit_of(it.next()?)?;
        return Some(SessionLine::Result {
            label,
            exit,
            outcome: token(it.next()?)?,
        });
    }
    if let Some(rest) = line.strip_prefix("DRIVE ") {
        let (label, end) = rest.split_once(' ')?;
        let exit = if end == "skipped" {
            None
        } else {
            Some(exit_of(end)?)
        };
        return Some(SessionLine::Drive {
            label: token(label)?,
            exit,
        });
    }
    None
}

/// Parse one stdout line; anything that does not fit exactly is `Other(line)`.
pub fn parse_session_line(line: &str) -> SessionLine<'_> {
    parse_known(line).unwrap_or(SessionLine::Other(line))
}

/// The step's position for a percent (0..=100): preflight 0 ... giveback 100;
/// an unknown step is `None`.
pub fn step_percent(step: &str) -> Option<i32> {
    STEPS
        .iter()
        .position(|s| *s == step)
        .map(|i| (i as i32) * 100 / (STEPS.len() as i32 - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_machine_lines_parse_and_everything_else_is_other() {
        assert_eq!(
            parse_session_line("PROGRESS system-recovery-A-2tb upgrade start"),
            SessionLine::Progress {
                label: "system-recovery-A-2tb",
                step: "upgrade",
                event: "start",
                message: ""
            }
        );
        assert_eq!(
            parse_session_line(
                "PROGRESS system-recovery-A-2tb egress fail the public address belongs to AS207990 HostRoyale"
            ),
            SessionLine::Progress {
                label: "system-recovery-A-2tb",
                step: "egress",
                event: "fail",
                message: "the public address belongs to AS207990 HostRoyale"
            }
        );
        assert_eq!(
            parse_session_line(
                "OUTPUT system-recovery-A-2tb upgrade :: Proceed with installation? [Y/n]"
            ),
            SessionLine::Output {
                label: "system-recovery-A-2tb",
                step: "upgrade",
                text: ":: Proceed with installation? [Y/n]"
            }
        );
        assert_eq!(
            parse_session_line("RESULT system-recovery-B-2tb 5 warnings"),
            SessionLine::Result {
                label: "system-recovery-B-2tb",
                exit: 5,
                outcome: "warnings"
            }
        );
        assert_eq!(
            parse_session_line("DRIVE system-recovery-B-2tb skipped"),
            SessionLine::Drive {
                label: "system-recovery-B-2tb",
                exit: None
            }
        );
        assert_eq!(
            parse_session_line("DRIVE system-recovery-A-2tb 0"),
            SessionLine::Drive {
                label: "system-recovery-A-2tb",
                exit: Some(0)
            }
        );
        for other in [
            "",
            "took the DAS maintenance lock",
            "PROGRESS",
            "PROGRESS x",
            "RESULT x notanumber clean",
            "progress a b c",
            "DRIVE x -1x",
        ] {
            assert_eq!(
                parse_session_line(other),
                SessionLine::Other(other),
                "{other:?}"
            );
        }
    }

    #[test]
    fn unknown_lines_are_logged_not_lost() {
        // Review focus 4: a PROGRESS line with a step not in STEPS still parses (the GUI logs it);
        // only step_percent is None.
        assert!(matches!(
            parse_session_line("PROGRESS system-recovery-A-2tb newstep ok"),
            SessionLine::Progress {
                step: "newstep",
                ..
            }
        ));
        assert_eq!(step_percent("newstep"), None);
        assert_eq!(step_percent("preflight"), Some(0));
        assert_eq!(step_percent("giveback"), Some(100));
        assert_eq!(step_percent("upgrade"), Some(8 * 100 / 17));
    }
}
