use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::checks::{Check, names};
use crate::process;

pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// Shorter than [`CHECK_TIMEOUT`](crate::checks::CHECK_TIMEOUT), so the check reports why
/// before the runner gives up on it.
pub const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(25);

/// Only units whose failure makes the desktop, logging in, or updating and rolling back
/// unusable. Deliberately absent: `ostree-boot-complete` (fails for a failed stage, not for
/// this deployment), anything that needs the network to be reachable,
/// `systemd-tmpfiles-setup` (fails for any bad package line), the D-Bus-activated
/// `apex-updated` (the `Ping` check covers it) and hardware services (reported checks).
pub const REQUIRED_UNITS: &[&str] = &[
    "boot.mount",
    "ostree-remount.service",
    "systemd-journald.service",
    "systemd-udevd.service",
    "dbus-broker.service",
    "systemd-logind.service",
    "systemd-user-sessions.service",
    "accounts-daemon.service",
    "gdm.service",
    "polkit.service",
    "NetworkManager.service",
    "systemd-resolved.service",
];

pub const REQUIRED_ACTIVE_UNITS: &[&str] = &["NetworkManager.service", "systemd-logind.service"];

const RUNNING_STATES: &[&str] = &["active", "reloading", "refreshing"];

const UNIT_SUFFIXES: &[&str] = &[
    ".service",
    ".socket",
    ".target",
    ".mount",
    ".automount",
    ".swap",
    ".device",
    ".path",
    ".timer",
    ".slice",
    ".scope",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnitStateError {
    #[error("expected {expected} unit states from systemctl, got {found}")]
    CountMismatch { expected: usize, found: usize },
}

pub fn parse_failed_units(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut tokens = line
                .split_whitespace()
                .skip_while(|t| matches!(*t, "●" | "×" | "*"));
            tokens.next().filter(|t| is_unit_name(t)).map(str::to_owned)
        })
        .collect()
}

pub fn failed_required_units(failed: &BTreeSet<String>, required: &[&str]) -> Vec<String> {
    required
        .iter()
        .filter(|unit| failed.contains(**unit))
        .map(|unit| (*unit).to_owned())
        .collect()
}

/// `systemctl is-active` prints one state per line, in argument order.
pub fn parse_unit_states(
    units: &[&str],
    output: &str,
) -> Result<Vec<(String, String)>, UnitStateError> {
    let states: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if states.len() != units.len() {
        return Err(UnitStateError::CountMismatch {
            expected: units.len(),
            found: states.len(),
        });
    }
    Ok(units
        .iter()
        .zip(states)
        .map(|(unit, state)| ((*unit).to_owned(), state.to_owned()))
        .collect())
}

pub fn not_running(states: &[(String, String)]) -> Vec<String> {
    states
        .iter()
        .filter(|(_, state)| !RUNNING_STATES.contains(&state.as_str()))
        .map(|(unit, state)| format!("{unit} ({state})"))
        .collect()
}

fn is_unit_name(token: &str) -> bool {
    UNIT_SUFFIXES
        .iter()
        .any(|suffix| token.len() > suffix.len() && token.ends_with(suffix))
}

/// Returns stdout whatever the exit status: `is-active` exits non-zero for an inactive unit.
fn systemctl(program: &Path, args: &[&str]) -> Result<(bool, String), String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/usr/sbin")
        .env("LANG", "C.UTF-8")
        .env("SYSTEMD_COLORS", "0");
    let done = process::run(&mut command, SYSTEMCTL_TIMEOUT).map_err(|err| err.describe())?;
    let stdout = String::from_utf8_lossy(&done.stdout).into_owned();
    if !done.status.success() && stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&done.stderr);
        return Err(format!(
            "systemctl {} failed ({}): {}",
            args.first().copied().unwrap_or_default(),
            done.status,
            stderr.trim()
        ));
    }
    Ok((done.status.success(), stdout))
}

#[derive(Debug, Clone)]
pub struct FailedUnitsCheck {
    systemctl: PathBuf,
    units: Vec<String>,
}

impl FailedUnitsCheck {
    pub fn system() -> Self {
        Self::new(SYSTEMCTL, REQUIRED_UNITS)
    }

    pub fn new(systemctl: impl Into<PathBuf>, units: &[&str]) -> Self {
        Self {
            systemctl: systemctl.into(),
            units: units.iter().map(|u| (*u).to_owned()).collect(),
        }
    }
}

impl Check for FailedUnitsCheck {
    fn name(&self) -> &str {
        names::FAILED_UNITS
    }

    fn run(&self) -> Result<(), String> {
        let (success, stdout) = systemctl(
            &self.systemctl,
            &[
                "list-units",
                "--state=failed",
                "--plain",
                "--no-legend",
                "--no-pager",
                "--full",
            ],
        )?;
        if !success {
            return Err("systemctl list-units failed".to_owned());
        }
        let failed = parse_failed_units(&stdout);
        let units: Vec<&str> = self.units.iter().map(String::as_str).collect();
        let bad = failed_required_units(&failed, &units);
        if bad.is_empty() {
            Ok(())
        } else {
            Err(format!("failed units: {}", bad.join(", ")))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActiveUnitsCheck {
    systemctl: PathBuf,
    units: Vec<String>,
}

impl ActiveUnitsCheck {
    pub fn system() -> Self {
        Self::new(SYSTEMCTL, REQUIRED_ACTIVE_UNITS)
    }

    pub fn new(systemctl: impl Into<PathBuf>, units: &[&str]) -> Self {
        Self {
            systemctl: systemctl.into(),
            units: units.iter().map(|u| (*u).to_owned()).collect(),
        }
    }
}

impl Check for ActiveUnitsCheck {
    fn name(&self) -> &str {
        names::CORE_UNITS_ACTIVE
    }

    fn run(&self) -> Result<(), String> {
        let units: Vec<&str> = self.units.iter().map(String::as_str).collect();
        let mut args = vec!["is-active", "--"];
        args.extend(&units);
        let (_, stdout) = systemctl(&self.systemctl, &args)?;
        let states = parse_unit_states(&units, &stdout).map_err(|err| err.to_string())?;
        let bad = not_running(&states);
        if bad.is_empty() {
            Ok(())
        } else {
            Err(format!("not active: {}", bad.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CHECK_TIMEOUT;

    #[test]
    fn unit_lists_are_well_formed() {
        for list in [REQUIRED_UNITS, REQUIRED_ACTIVE_UNITS] {
            let unique: BTreeSet<_> = list.iter().collect();
            assert_eq!(unique.len(), list.len(), "duplicates in {list:?}");
            assert!(list.iter().all(|u| is_unit_name(u)), "{list:?}");
        }
        assert!(
            REQUIRED_ACTIVE_UNITS
                .iter()
                .all(|u| REQUIRED_UNITS.contains(u))
        );
        for excluded in [
            "ostree-boot-complete.service",
            "NetworkManager-wait-online.service",
            "network-online.target",
            "systemd-tmpfiles-setup.service",
            "apex-updated.service",
            "apex-boot-assess.service",
        ] {
            assert!(!REQUIRED_UNITS.contains(&excluded), "{excluded}");
        }
        assert!(SYSTEMCTL_TIMEOUT < CHECK_TIMEOUT);
    }

    #[test]
    fn parses_plain_list_units_output() {
        let output = "gdm.service loaded failed failed GNOME Display Manager\n\
                      swap.img.swap loaded failed failed /swap.img\n\n";
        let failed = parse_failed_units(output);
        assert_eq!(
            failed.iter().map(String::as_str).collect::<Vec<_>>(),
            ["gdm.service", "swap.img.swap"]
        );
        assert_eq!(
            failed_required_units(&failed, REQUIRED_UNITS),
            vec!["gdm.service".to_owned()]
        );
    }

    #[test]
    fn parses_decorated_output_and_ignores_legend() {
        let output = "  UNIT LOAD ACTIVE SUB DESCRIPTION\n\
                      ● polkit.service loaded failed failed Authorization Manager\n\
                      × boot.mount loaded failed failed /boot\n\
                      * dbus-broker.service loaded failed failed D-Bus System Message Bus\n\
                      \n\
                      LOAD   = Reflects whether the unit definition was properly loaded.\n\
                      3 loaded units listed.\n";
        assert_eq!(
            failed_required_units(&parse_failed_units(output), REQUIRED_UNITS),
            vec![
                "boot.mount".to_owned(),
                "dbus-broker.service".to_owned(),
                "polkit.service".to_owned()
            ]
        );
        assert!(parse_failed_units("").is_empty());
        assert!(parse_failed_units("0 loaded units listed.\n").is_empty());
    }

    #[test]
    fn pairs_is_active_output_with_units() {
        let units = [
            "NetworkManager.service",
            "systemd-logind.service",
            "x.service",
        ];
        let states = parse_unit_states(&units, "active\nreloading\ninactive\n").unwrap();
        assert_eq!(
            not_running(&states),
            vec!["x.service (inactive)".to_owned()]
        );
        assert_eq!(
            parse_unit_states(&units, "active\n"),
            Err(UnitStateError::CountMismatch {
                expected: 3,
                found: 1
            })
        );
        let all_up = parse_unit_states(&units[..2], "active\nactive").unwrap();
        assert!(not_running(&all_up).is_empty());
        let failing = parse_unit_states(&units[..1], "activating\n").unwrap();
        assert_eq!(
            not_running(&failing),
            vec!["NetworkManager.service (activating)".to_owned()]
        );
    }

    #[test]
    fn check_names() {
        assert_eq!(FailedUnitsCheck::system().name(), names::FAILED_UNITS);
        assert_eq!(ActiveUnitsCheck::system().name(), names::CORE_UNITS_ACTIVE);
    }
}
