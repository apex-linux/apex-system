//! The early-boot watchdog. The assessment judges a boot only after `graphical.target`, so a
//! boot of an update on trial that stops in emergency or rescue mode would wait at `sulogin`
//! and GRUB's counter would move only on a forced power-off. The drop-ins run this before the
//! shell starts; it schedules a reboot when GRUB counted this boot.
//!
//! As root, `systemd-run` and `systemctl` reach systemd over its private socket, which works
//! without the system bus that emergency mode lacks.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::time::Duration;

use crate::checks::units::SYSTEMCTL;
use crate::grubenv::GrubEnv;
use crate::process;
use crate::vars::{BootVars, CounterState};
use crate::writer::GRUBENV_PATH;

pub const EMERGENCY_DROP_IN: &str =
    include_str!("../data/emergency.service.d/50-apex-bootcount.conf");

pub const RESCUE_DROP_IN: &str = include_str!("../data/rescue.service.d/50-apex-bootcount.conf");

pub const DROP_IN_FILE_NAME: &str = "50-apex-bootcount.conf";

pub const SHELL_UNITS: [&str; 2] = ["emergency.service", "rescue.service"];

pub const WATCHDOG_FLAG: &str = "--emergency-watchdog";

pub const REBOOT_DELAY: Duration = Duration::from_secs(60);

pub const REBOOT_UNIT: &str = "apex-emergency-reboot";

pub const REBOOT_TIMER: &str = "apex-emergency-reboot.timer";

pub const CANCEL_COMMAND: &str = "systemctl stop apex-emergency-reboot.timer";

pub const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

pub const HEAD: &str = "/usr/bin/head";

/// systemd's own `in_initrd()` test.
pub const INITRD_RELEASE_PATH: &str = "/etc/initrd-release";

pub const GRUBENV_READ_LIMIT: usize = 64 * 1024;

pub const GRUBENV_READ_TIMEOUT: Duration = Duration::from_secs(5);

pub const SCHEDULE_TIMEOUT: Duration = Duration::from_secs(10);

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The shell units set none, so systemd's `DefaultTimeoutStartSec=` applies.
pub const SHELL_START_TIMEOUT: Duration = Duration::from_secs(90);

/// Must stay well inside [`SHELL_START_TIMEOUT`], or a stalled helper could fail the shell.
pub const WATCHDOG_TIME_BUDGET: Duration = Duration::from_secs(
    GRUBENV_READ_TIMEOUT.as_secs()
        + SCHEDULE_TIMEOUT.as_secs()
        + QUERY_TIMEOUT.as_secs()
        + 3 * crate::KILL_REAP_GRACE.as_secs(),
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Reboot { remaining: u8 },
    Stay(Stay),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stay {
    NotArmed,
    Confirmed,
    /// A reboot might never move the counter, and could loop.
    NotCounted,
    /// A reboot would only start the previous deployment again.
    FellBack,
}

impl fmt::Display for Stay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stay::NotArmed => "no update is on trial (apex_target is not set)",
            Stay::Confirmed => "apex_ok is 1, so GRUB did not count this boot",
            Stay::NotCounted => "GRUB did not count this boot (apex_counter is not 2, 1 or 0)",
            Stay::FellBack => "GRUB already fell back to the previous version",
        })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Reboot { remaining } => write!(
                f,
                "reboot in {} s ({remaining} more attempts of the update)",
                REBOOT_DELAY.as_secs()
            ),
            Action::Stay(reason) => write!(f, "no reboot: {reason}"),
        }
    }
}

/// Reboots only a boot GRUB counted, so one arming leads to at most
/// [`BOOT_ATTEMPTS`](crate::BOOT_ATTEMPTS) reboots. It cannot run `bootc status` in its
/// budget, so unlike the assessment it does not check that the target is booted.
pub fn action_for(vars: &BootVars) -> Action {
    if !vars.is_armed() {
        return Action::Stay(Stay::NotArmed);
    }
    if vars.apex_ok == Some(true) {
        return Action::Stay(Stay::Confirmed);
    }
    match vars.counter_state() {
        CounterState::Counted { remaining } => Action::Reboot { remaining },
        CounterState::Unset | CounterState::NotYetCounted => Action::Stay(Stay::NotCounted),
        CounterState::Exhausted => Action::Stay(Stay::FellBack),
    }
}

/// Only compile-time constants: nothing read from the system reaches this command line.
///
/// `DefaultDependencies=no`: the defaults would start `sysinit.target` again, which is
/// typically what failed. `ExecCondition=` drops the reboot once the shell has been left and
/// the boot continues, so a user's session is never rebooted.
pub fn schedule_args() -> Vec<String> {
    let condition = format!(
        "--property=ExecCondition={SYSTEMCTL} --quiet is-active {}",
        SHELL_UNITS.join(" ")
    );
    vec![
        "--quiet".to_owned(),
        "--no-ask-password".to_owned(),
        "--collect".to_owned(),
        format!("--unit={REBOOT_UNIT}"),
        "--description=Apex: reboot so GRUB counts the next boot of the update on trial".to_owned(),
        format!("--on-active={}s", REBOOT_DELAY.as_secs()),
        "--timer-property=AccuracySec=1s".to_owned(),
        "--timer-property=DefaultDependencies=no".to_owned(),
        "--property=DefaultDependencies=no".to_owned(),
        condition,
        "--".to_owned(),
        SYSTEMCTL.to_owned(),
        "reboot".to_owned(),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    Created,
    /// An earlier entry into emergency or rescue mode during this boot scheduled it.
    AlreadyPending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub grubenv: PathBuf,
    pub initrd_release: PathBuf,
    pub head: PathBuf,
    pub systemd_run: PathBuf,
    pub systemctl: PathBuf,
    pub read_timeout: Duration,
    pub schedule_timeout: Duration,
    pub query_timeout: Duration,
}

impl Host {
    pub fn system() -> Self {
        Self {
            grubenv: GRUBENV_PATH.into(),
            initrd_release: INITRD_RELEASE_PATH.into(),
            head: HEAD.into(),
            systemd_run: SYSTEMD_RUN.into(),
            systemctl: SYSTEMCTL.into(),
            read_timeout: GRUBENV_READ_TIMEOUT,
            schedule_timeout: SCHEDULE_TIMEOUT,
            query_timeout: QUERY_TIMEOUT,
        }
    }

    pub fn in_initrd(&self) -> bool {
        self.initrd_release.exists()
    }

    /// Reads through a child process: a read from a stalled `/boot` can sleep
    /// uninterruptibly, and this process could then never exit, while a child can be
    /// abandoned.
    pub fn read_vars(&self) -> Result<BootVars, String> {
        let mut command = Command::new(&self.head);
        command
            .arg("-c")
            .arg((GRUBENV_READ_LIMIT + 1).to_string())
            .arg("--")
            .arg(&self.grubenv);
        clean_environment(&mut command);
        let done = process::run(&mut command, self.read_timeout).map_err(|err| err.describe())?;
        if !done.status.success() {
            return Err(failure("head", done.status, &done.stderr));
        }
        let path = self.grubenv.display();
        if done.stdout.len() > GRUBENV_READ_LIMIT {
            return Err(format!("{path} is larger than {GRUBENV_READ_LIMIT} bytes"));
        }
        let env = GrubEnv::parse(&done.stdout).map_err(|err| format!("{path}: {err}"))?;
        BootVars::from_env(&env).map_err(|err| format!("{path}: {err}"))
    }

    /// `systemd-run` refuses the unit name while the timer is still waiting.
    pub fn schedule_reboot(&self) -> Result<Schedule, String> {
        let mut command = Command::new(&self.systemd_run);
        command.args(schedule_args());
        clean_environment(&mut command);
        let error = match process::run(&mut command, self.schedule_timeout) {
            Ok(done) if done.status.success() => return Ok(Schedule::Created),
            Ok(done) => failure("systemd-run", done.status, &done.stderr),
            Err(err) => err.describe(),
        };
        if self.timer_is_active() {
            Ok(Schedule::AlreadyPending)
        } else {
            Err(error)
        }
    }

    fn timer_is_active(&self) -> bool {
        let mut command = Command::new(&self.systemctl);
        command.args(["--quiet", "is-active", "--", REBOOT_TIMER]);
        clean_environment(&mut command);
        process::run(&mut command, self.query_timeout).is_ok_and(|done| done.status.success())
    }
}

fn clean_environment(command: &mut Command) {
    command
        .env_clear()
        .env("PATH", "/usr/bin:/usr/sbin")
        .env("LANG", "C.UTF-8");
}

fn failure(program: &str, status: ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    match stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => format!("{program} failed ({status}): {line}"),
        None => format!("{program} failed ({status})"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Initrd,
    Unreadable(String),
    Stay(Stay),
    WouldReboot { remaining: u8 },
    RebootScheduled { remaining: u8, schedule: Schedule },
    ScheduleFailed { remaining: u8, error: String },
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        !matches!(
            self,
            Outcome::Unreadable(_) | Outcome::ScheduleFailed { .. }
        )
    }
}

/// A console write error is ignored: nothing here may keep the shell from starting.
pub fn run(host: &Host, dry_run: bool, console: &mut dyn Write) -> Outcome {
    let mut say = |lines: &[String]| {
        for line in lines {
            let _ = writeln!(console, "{line}");
        }
        let _ = console.flush();
    };
    if host.in_initrd() {
        if dry_run {
            say(&[dry("no reboot: this is an initramfs")]);
        }
        return Outcome::Initrd;
    }
    let vars = match host.read_vars() {
        Ok(vars) => vars,
        Err(error) => {
            say(&unreadable_message(&error));
            return Outcome::Unreadable(error);
        }
    };
    let remaining = match action_for(&vars) {
        Action::Reboot { remaining } => remaining,
        Action::Stay(reason) => {
            if dry_run {
                say(&[dry(&format!("no reboot: {reason}"))]);
            }
            return Outcome::Stay(reason);
        }
    };
    if dry_run {
        let mut lines = reboot_message(remaining, Schedule::Created);
        lines.push(dry(&format!(
            "would run: {}",
            command_line(&host.systemd_run.display().to_string(), &schedule_args())
        )));
        say(&lines);
        return Outcome::WouldReboot { remaining };
    }
    match host.schedule_reboot() {
        Ok(schedule) => {
            say(&reboot_message(remaining, schedule));
            Outcome::RebootScheduled {
                remaining,
                schedule,
            }
        }
        Err(error) => {
            say(&schedule_failed_message(&error));
            Outcome::ScheduleFailed { remaining, error }
        }
    }
}

const TRIAL_LINE: &str =
    "Apex: this boot is trying out a newly installed update and did not start fully.";

pub fn reboot_message(remaining: u8, schedule: Schedule) -> Vec<String> {
    let when = match schedule {
        Schedule::Created => format!(
            "Apex: rebooting in {} s so that the boot counter moves on.",
            REBOOT_DELAY.as_secs()
        ),
        Schedule::AlreadyPending => {
            "Apex: a reboot is already scheduled so that the boot counter moves on.".to_owned()
        }
    };
    let next = match remaining {
        0 => "Apex: the previous version starts next.".to_owned(),
        1 => "Apex: the previous version starts after 1 more failed attempt.".to_owned(),
        n => format!("Apex: the previous version starts after {n} more failed attempts."),
    };
    vec![
        TRIAL_LINE.to_owned(),
        when,
        next,
        format!("Apex: to stay in this shell, run: {CANCEL_COMMAND}"),
    ]
}

pub fn schedule_failed_message(error: &str) -> Vec<String> {
    vec![
        TRIAL_LINE.to_owned(),
        format!("Apex: cannot schedule the automatic reboot: {error}"),
        "Apex: restart the machine so that the boot counter moves on.".to_owned(),
    ]
}

pub fn unreadable_message(error: &str) -> Vec<String> {
    vec![format!(
        "Apex: cannot read the boot counter, so no automatic reboot: {error}"
    )]
}

fn dry(text: &str) -> String {
    format!("Apex (dry run): {text}")
}

fn command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_=./:@,".contains(&b))
            {
                arg.to_owned()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::Digest;
    use crate::snippet::{BootEntry, grub_boot};
    use crate::vars::{BOOT_ATTEMPTS, arm_ops};
    use crate::writer::{RecordingWriter, apply_ops};
    use crate::{ASSESS_BINARY_PATH, KILL_REAP_GRACE};

    const TARGET: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    fn vars(target: bool, counter: Option<i32>, ok: Option<bool>) -> BootVars {
        BootVars {
            apex_target: target.then(|| Digest::parse(TARGET).unwrap()),
            apex_counter: counter,
            apex_ok: ok,
        }
    }

    #[test]
    fn reboots_only_on_a_counted_boot_of_an_armed_update() {
        let counters = [
            None,
            Some(i32::MIN),
            Some(-2),
            Some(-1),
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(i32::MAX),
        ];
        for target in [false, true] {
            for ok in [None, Some(false), Some(true)] {
                for counter in counters {
                    let vars = vars(target, counter, ok);
                    let action = action_for(&vars);
                    let counted = counter.is_some_and(|c| (0..=2).contains(&c));
                    if target && ok != Some(true) && counted {
                        assert_eq!(
                            action,
                            Action::Reboot {
                                remaining: counter.unwrap().unsigned_abs() as u8
                            },
                            "{vars:?}"
                        );
                        assert!(vars.counter_state().reboot_makes_progress());
                    } else {
                        assert!(matches!(action, Action::Stay(_)), "{vars:?}: {action}");
                    }
                }
            }
        }
    }

    #[test]
    fn says_why_it_stays() {
        assert_eq!(
            action_for(&vars(false, Some(2), Some(false))),
            Action::Stay(Stay::NotArmed)
        );
        assert_eq!(
            action_for(&vars(false, None, None)),
            Action::Stay(Stay::NotArmed)
        );
        assert_eq!(
            action_for(&vars(true, Some(1), Some(true))),
            Action::Stay(Stay::Confirmed)
        );
        assert_eq!(
            action_for(&vars(true, Some(BOOT_ATTEMPTS), Some(false))),
            Action::Stay(Stay::NotCounted)
        );
        assert_eq!(
            action_for(&vars(true, None, Some(false))),
            Action::Stay(Stay::NotCounted)
        );
        assert_eq!(
            action_for(&vars(true, Some(-1), Some(false))),
            Action::Stay(Stay::FellBack)
        );
        assert_eq!(
            action_for(&vars(true, Some(7), None)),
            Action::Stay(Stay::FellBack)
        );
        assert_eq!(
            action_for(&vars(true, Some(0), None)).to_string(),
            "reboot in 60 s (0 more attempts of the update)"
        );
        assert_eq!(
            action_for(&vars(false, None, None)).to_string(),
            "no reboot: no update is on trial (apex_target is not set)"
        );
    }

    #[test]
    fn an_update_that_always_stops_early_gets_three_reboots_then_the_previous_one() {
        let mut writer = RecordingWriter::new(GrubEnv::new());
        apply_ops(&mut writer, &arm_ops(&Digest::parse(TARGET).unwrap())).unwrap();
        let mut env = writer.env().clone();
        let mut reboots = Vec::new();
        for _ in 0..10 {
            let choice = grub_boot(&mut env).unwrap();
            match action_for(&BootVars::from_env(&env).unwrap()) {
                Action::Reboot { remaining } => {
                    assert_eq!(choice.entry, BootEntry::Default);
                    reboots.push(remaining);
                }
                Action::Stay(reason) => {
                    assert_eq!(reason, Stay::FellBack);
                    assert_eq!(choice.entry, BootEntry::Previous);
                }
            }
        }
        assert_eq!(reboots, [2, 1, 0]);
        assert_eq!(reboots.len(), BOOT_ATTEMPTS as usize);
    }

    #[test]
    fn a_counter_grub_does_not_move_never_reboots() {
        let mut writer = RecordingWriter::new(GrubEnv::new());
        apply_ops(&mut writer, &arm_ops(&Digest::parse(TARGET).unwrap())).unwrap();
        let env = writer.env().clone();
        assert_eq!(
            action_for(&BootVars::from_env(&env).unwrap()),
            Action::Stay(Stay::NotCounted)
        );
    }

    #[test]
    fn the_schedule_command_is_fixed() {
        assert_eq!(
            schedule_args(),
            [
                "--quiet",
                "--no-ask-password",
                "--collect",
                "--unit=apex-emergency-reboot",
                "--description=Apex: reboot so GRUB counts the next boot of the update on trial",
                "--on-active=60s",
                "--timer-property=AccuracySec=1s",
                "--timer-property=DefaultDependencies=no",
                "--property=DefaultDependencies=no",
                "--property=ExecCondition=/usr/bin/systemctl --quiet is-active \
                 emergency.service rescue.service",
                "--",
                "/usr/bin/systemctl",
                "reboot",
            ]
        );
        assert_eq!(REBOOT_TIMER, format!("{REBOOT_UNIT}.timer"));
        assert!(CANCEL_COMMAND.ends_with(REBOOT_TIMER));
        assert_eq!(
            command_line("/usr/bin/systemd-run", &schedule_args()[..5]),
            "/usr/bin/systemd-run --quiet --no-ask-password --collect \
             --unit=apex-emergency-reboot \
             '--description=Apex: reboot so GRUB counts the next boot of the update on trial'"
        );
        assert_eq!(command_line("a", &["it's".to_owned()]), r"a 'it'\''s'");
    }

    #[test]
    fn the_announcement_says_when_and_how_to_stay() {
        let lines = reboot_message(2, Schedule::Created);
        assert_eq!(
            lines,
            [
                "Apex: this boot is trying out a newly installed update and did not start fully.",
                "Apex: rebooting in 60 s so that the boot counter moves on.",
                "Apex: the previous version starts after 2 more failed attempts.",
                "Apex: to stay in this shell, run: systemctl stop apex-emergency-reboot.timer",
            ]
        );
        assert_eq!(
            reboot_message(1, Schedule::Created)[2],
            "Apex: the previous version starts after 1 more failed attempt."
        );
        assert_eq!(
            reboot_message(0, Schedule::AlreadyPending)[1..3],
            [
                "Apex: a reboot is already scheduled so that the boot counter moves on.",
                "Apex: the previous version starts next.",
            ]
        );
        for remaining in 0..=2 {
            for schedule in [Schedule::Created, Schedule::AlreadyPending] {
                for line in reboot_message(remaining, schedule) {
                    assert!(line.len() <= 80, "{line}");
                }
            }
        }
        assert!(schedule_failed_message("x")[1].ends_with(": x"));
        assert!(unreadable_message("gone")[0].contains("no automatic reboot: gone"));
    }

    #[test]
    fn the_drop_ins_run_the_watchdog() {
        assert_eq!(EMERGENCY_DROP_IN, RESCUE_DROP_IN);
        let line = format!("ExecStartPre=-{ASSESS_BINARY_PATH} {WATCHDOG_FLAG}");
        assert_eq!(
            EMERGENCY_DROP_IN
                .lines()
                .filter(|l| l.starts_with("ExecStartPre="))
                .collect::<Vec<_>>(),
            [line.as_str()]
        );
    }

    #[test]
    fn the_budget_fits_the_shell_start_timeout() {
        assert_eq!(KILL_REAP_GRACE, Duration::from_secs(5));
        assert_eq!(WATCHDOG_TIME_BUDGET, Duration::from_secs(35));
        assert!(WATCHDOG_TIME_BUDGET * 2 <= SHELL_START_TIMEOUT);
        let host = Host::system();
        assert_eq!(
            host.read_timeout + host.schedule_timeout + host.query_timeout + 3 * KILL_REAP_GRACE,
            WATCHDOG_TIME_BUDGET
        );
        assert!(REBOOT_DELAY >= Duration::from_secs(30));
    }

    #[test]
    fn outcomes_that_need_attention_fail_the_run() {
        assert!(Outcome::Initrd.succeeded());
        assert!(Outcome::Stay(Stay::NotArmed).succeeded());
        assert!(Outcome::WouldReboot { remaining: 1 }.succeeded());
        assert!(
            Outcome::RebootScheduled {
                remaining: 1,
                schedule: Schedule::AlreadyPending
            }
            .succeeded()
        );
        assert!(!Outcome::Unreadable("x".into()).succeeded());
        assert!(
            !Outcome::ScheduleFailed {
                remaining: 0,
                error: "x".into()
            }
            .succeeded()
        );
    }
}
