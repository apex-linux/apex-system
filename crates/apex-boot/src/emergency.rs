//! The early-boot watchdog (ADR 0009, Consequences; M1 contract §2): a boot of an update on
//! trial that stops in emergency or rescue mode still moves GRUB's counter.
//!
//! # The gap it closes
//!
//! `apex-boot-assess.service` judges a boot after `graphical.target`. A boot that stops
//! earlier, in `emergency.target` or `rescue.target` (for example after a failed
//! `local-fs.target` dependency), waits at `sulogin`; with the root account locked it only
//! offers "Press Enter to continue". Nothing reboots, so GRUB's counter would move only when
//! somebody forces a power-off, and the counted rollback would take three manual power
//! cycles. The kernel arguments `panic=10 rd.shell=0 rd.emergency=reboot` cover the kernel
//! and the initramfs, not the real root.
//!
//! # The watchdog
//!
//! Two identical drop-ins, [`EMERGENCY_DROP_IN`] and [`RESCUE_DROP_IN`], add
//! `ExecStartPre=-/usr/libexec/apex/apex-boot-assess --emergency-watchdog` to
//! `emergency.service` and `rescue.service`. The binary calls [`run`] with
//! [`Host::system`], which:
//!
//! 1. does nothing in an initramfs (`/etc/initrd-release` exists; dracut does not copy the
//!    drop-ins there, so this is only a second guard);
//! 2. reads grubenv in a child process ([`Host::read_vars`]);
//! 3. decides with [`action_for`]: reboot only while an armed update is being counted
//!    (`apex_target` set, `apex_ok` not 1, `apex_counter` 2, 1 or 0);
//! 4. schedules the reboot with `systemd-run` ([`schedule_args`]) and says so on the
//!    console, with the command that cancels it ([`CANCEL_COMMAND`]).
//!
//! In every other case it prints nothing, so an ordinary emergency or rescue shell stays
//! exactly as it was, except for one line when grubenv cannot be read.
//!
//! # Why the reboot cannot loop
//!
//! The watchdog reboots only on a boot GRUB counted, and each counted boot lowers the
//! counter: 2, 1, 0, then GRUB falls back to the previous deployment and stores -1, after
//! which the watchdog stays quiet ([`Stay::FellBack`]). So one arming leads to at most
//! [`BOOT_ATTEMPTS`](crate::BOOT_ATTEMPTS) reboots, the same bound as the assessment's own
//! (`tests/boot_simulation.rs` counts them together with panics and the assessment's
//! reboots). A counter still at 3 means GRUB did not count this boot (an `Apply` during this
//! boot, or a snippet or `save_env` that does not work); a reboot might then repeat forever,
//! so the watchdog does nothing.
//!
//! Unlike the assessment's `RedArmedNoSession` row, the watchdog does not need to know that
//! the booted deployment is the target: it cannot run `bootc status` within its time budget,
//! and it only does what the next power cycle would do anyway (GRUB decides what boots next,
//! from the same counter). So when the previous deployment stops in emergency mode while
//! GRUB counts (a manual menu pick, or the hedge after a lost fallback reply), the watchdog
//! reboots it too. Such boots are among those that can use up the hedge, the residual case
//! described in [`mod@crate::decide`]. The remaining assumption is the assessment's too: a
//! counter that GRUB decremented once keeps being saved. If `save_env` stopped working
//! after that, both would reboot on every boot until somebody picks a menu entry.
//!
//! # The reboot
//!
//! `systemd-run` creates `apex-emergency-reboot.timer` and `.service` as transient units:
//!
//! - after [`REBOOT_DELAY`] the service runs `systemctl reboot`. In emergency mode logind and
//!   the system bus are not running, so `systemctl` falls back to starting `reboot.target`
//!   directly, as it does when typed in the shell;
//! - `DefaultDependencies=no` on both units: the defaults would add
//!   `Requires=sysinit.target`, and starting the timer would then start `sysinit.target`
//!   again, which is typically what failed;
//! - `ExecCondition=` runs the reboot only if `emergency.service` or `rescue.service` is
//!   still active when the timer fires. Leaving the shell continues the boot (systemd's
//!   sulogin shell starts `default.target`, which also stops the timer when it isolates),
//!   and a boot that then reaches the desktop is judged by the assessment instead: it never
//!   reboots a user's session;
//! - `AccuracySec=1s` (the default is a minute), `--collect` and `RemainAfterElapse=no`
//!   (added by `systemd-run`), so the units disappear once done and a later entry into
//!   emergency mode in the same boot can schedule again. While the timer is still waiting,
//!   `systemd-run` refuses the name; [`Host::schedule_reboot`] then reports
//!   [`Schedule::AlreadyPending`].
//!
//! As root, `systemd-run` and `systemctl` talk to systemd over its private socket
//! (`/run/systemd/private`), which works without the system bus.
//!
//! # Never blocking the shell
//!
//! The drop-in's `-` prefix ignores the exit status, but a command that does not finish
//! would still hold the shell until the unit's start timeout and then fail it. Every step is
//! therefore bounded ([`WATCHDOG_TIME_BUDGET`], well inside [`SHELL_START_TIMEOUT`]): the
//! helpers are killed after their timeout and abandoned if they do not die. grubenv is read
//! by a child process (`head`) rather than in this process on purpose: a read from a stalled
//! `/boot` can sleep uninterruptibly, and a process with such a thread cannot exit, while an
//! abandoned child can be left behind.
//!
//! A shell entered on purpose (`systemd.unit=rescue.target`, `systemctl rescue`) during a
//! counted boot gets the same announcement and 60 s to run [`CANCEL_COMMAND`].

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

/// The drop-in for `emergency.service`, exactly as installed in
/// `/usr/lib/systemd/system/emergency.service.d/` (M1 contract §2).
pub const EMERGENCY_DROP_IN: &str =
    include_str!("../data/emergency.service.d/50-apex-bootcount.conf");

/// The drop-in for `rescue.service`, exactly as installed in
/// `/usr/lib/systemd/system/rescue.service.d/`. The same text as [`EMERGENCY_DROP_IN`].
pub const RESCUE_DROP_IN: &str = include_str!("../data/rescue.service.d/50-apex-bootcount.conf");

/// The file name of both drop-ins.
pub const DROP_IN_FILE_NAME: &str = "50-apex-bootcount.conf";

/// The units whose start runs the watchdog. The scheduled reboot happens only while one of
/// them is still active.
pub const SHELL_UNITS: [&str; 2] = ["emergency.service", "rescue.service"];

/// The `apex-boot-assess` flag the drop-ins pass.
pub const WATCHDOG_FLAG: &str = "--emergency-watchdog";

/// How long the console shows the announcement before the reboot.
pub const REBOOT_DELAY: Duration = Duration::from_secs(60);

/// The name of the transient units (`.timer` and `.service`) that carry the reboot.
pub const REBOOT_UNIT: &str = "apex-emergency-reboot";

/// The transient timer; stopping it cancels the reboot.
pub const REBOOT_TIMER: &str = "apex-emergency-reboot.timer";

/// What the console tells the user to run to stay in the shell.
pub const CANCEL_COMMAND: &str = "systemctl stop apex-emergency-reboot.timer";

/// `systemd-run(1)` (package `systemd`).
pub const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

/// `head(1)` (package `coreutils`), which reads grubenv for the watchdog.
pub const HEAD: &str = "/usr/bin/head";

/// Present only in an initramfs (systemd's own `in_initrd()` test).
pub const INITRD_RELEASE_PATH: &str = "/etc/initrd-release";

/// The largest grubenv the watchdog reads. `grub2-editenv` creates 1024-byte blocks.
pub const GRUBENV_READ_LIMIT: usize = 64 * 1024;

/// How long reading grubenv may take.
pub const GRUBENV_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `systemd-run` may take to create the units.
pub const SCHEDULE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long asking whether the timer already exists may take (after `systemd-run` failed).
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The start timeout `emergency.service` and `rescue.service` have: they set none, so
/// systemd's `DefaultTimeoutStartSec=` (90 s) applies. The drop-in's command runs inside it.
pub const SHELL_START_TIMEOUT: Duration = Duration::from_secs(90);

/// The longest a watchdog run takes with [`Host::system`]: the grubenv read, `systemd-run`
/// and, after a failure, the timer query, each followed by at most
/// [`KILL_REAP_GRACE`](crate::KILL_REAP_GRACE) for a killed helper: 5 + 5 + 10 + 5 + 5 + 5 =
/// 35 s. It must stay well inside [`SHELL_START_TIMEOUT`], or a stalled helper could fail
/// the shell; a test holds it to half.
pub const WATCHDOG_TIME_BUDGET: Duration = Duration::from_secs(
    GRUBENV_READ_TIMEOUT.as_secs()
        + SCHEDULE_TIMEOUT.as_secs()
        + QUERY_TIMEOUT.as_secs()
        + 3 * crate::KILL_REAP_GRACE.as_secs(),
);

/// What the watchdog does about the current boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// GRUB counted this boot of an armed update: reboot after [`REBOOT_DELAY`], so the
    /// counter moves on. `remaining` more unconfirmed boots of the update follow before
    /// GRUB falls back to the previous deployment (0: the next boot is the previous one).
    Reboot {
        /// 2, 1 or 0: the counter as GRUB left it.
        remaining: u8,
    },
    /// Leave the shell alone.
    Stay(Stay),
}

/// Why the watchdog leaves the shell alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stay {
    /// No update is on trial (`apex_target` is not set).
    NotArmed,
    /// `apex_ok` is 1, so GRUB did not count this boot.
    Confirmed,
    /// The counter is unset or still at 3: GRUB did not count this boot (armed during this
    /// boot, or the snippet or `save_env` does not work). A reboot might never move it.
    NotCounted,
    /// The counter is exhausted: GRUB already fell back to the previous deployment, and a
    /// reboot would only start it again.
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

/// The watchdog's decision, a pure function of the Apex variables as GRUB left them:
/// reboot exactly when `apex_target` is set, `apex_ok` is not 1 and `apex_counter` is 2, 1
/// or 0, i.e. when GRUB counted this boot of an armed update. This is the same test as
/// [`CounterState::reboot_makes_progress`] with the arming and confirmation checked first.
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

/// The arguments of the `systemd-run` call that schedules the reboot (after the program
/// name). Only compile-time constants: nothing read from the system reaches this command
/// line.
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

/// Whether the reboot was scheduled by this run or an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// `systemd-run` created the timer now.
    Created,
    /// The timer was already waiting: an earlier entry into emergency or rescue mode during
    /// this boot scheduled it.
    AlreadyPending,
}

/// The system the watchdog runs on: the files it reads and the programs it runs. Tests
/// point them at fixtures and fakes; [`Host::system`] is production.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The GRUB environment block.
    pub grubenv: PathBuf,
    /// The file whose existence means "this is an initramfs".
    pub initrd_release: PathBuf,
    /// `head`, to read grubenv.
    pub head: PathBuf,
    /// `systemd-run`, to schedule the reboot.
    pub systemd_run: PathBuf,
    /// `systemctl`, to ask whether the timer is already waiting.
    pub systemctl: PathBuf,
    /// Bound on reading grubenv.
    pub read_timeout: Duration,
    /// Bound on `systemd-run`.
    pub schedule_timeout: Duration,
    /// Bound on the timer query.
    pub query_timeout: Duration,
}

impl Host {
    /// The running system: `/boot/grub2/grubenv`, `/etc/initrd-release`, the programs in
    /// `/usr/bin`, and the timeouts [`WATCHDOG_TIME_BUDGET`] is made of.
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

    /// Whether this runs in an initramfs.
    pub fn in_initrd(&self) -> bool {
        self.initrd_release.exists()
    }

    /// Reads the Apex variables from grubenv with `head -c`, bounded by
    /// [`Host::read_timeout`], and interprets them as the assessment does. Any failure is
    /// returned as one line for the console.
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

    /// Runs `systemd-run` with [`schedule_args`], bounded by [`Host::schedule_timeout`].
    /// When it fails because the timer is already waiting, that is
    /// [`Schedule::AlreadyPending`]; any other failure is returned as one line.
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

    /// Whether [`REBOOT_TIMER`] is active (waiting, or its reboot running).
    fn timer_is_active(&self) -> bool {
        let mut command = Command::new(&self.systemctl);
        command.args(["--quiet", "is-active", "--", REBOOT_TIMER]);
        clean_environment(&mut command);
        process::run(&mut command, self.query_timeout).is_ok_and(|done| done.status.success())
    }
}

/// Helpers run with a known environment only.
fn clean_environment(command: &mut Command) {
    command
        .env_clear()
        .env("PATH", "/usr/bin:/usr/sbin")
        .env("LANG", "C.UTF-8");
}

/// `program failed (status): first line of stderr`.
fn failure(program: &str, status: ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    match stderr.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => format!("{program} failed ({status}): {line}"),
        None => format!("{program} failed ({status})"),
    }
}

/// What a watchdog run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// In an initramfs: nothing was read or done.
    Initrd,
    /// grubenv could not be read or interpreted: no reboot.
    Unreadable(String),
    /// Not a counted boot of an armed update: the shell stays.
    Stay(Stay),
    /// Dry run: the reboot would have been scheduled.
    WouldReboot {
        /// See [`Action::Reboot`].
        remaining: u8,
    },
    /// The reboot is scheduled.
    RebootScheduled {
        /// See [`Action::Reboot`].
        remaining: u8,
        /// By this run or an earlier one.
        schedule: Schedule,
    },
    /// The reboot should happen, but it could not be scheduled.
    ScheduleFailed {
        /// See [`Action::Reboot`].
        remaining: u8,
        /// Why, in one line.
        error: String,
    },
}

impl Outcome {
    /// Whether the watchdog did what the boot called for (the binary's exit status).
    pub fn succeeded(&self) -> bool {
        !matches!(
            self,
            Outcome::Unreadable(_) | Outcome::ScheduleFailed { .. }
        )
    }
}

/// Runs the watchdog on `host` and writes what it does to `console` (the drop-in's standard
/// output is the console). With `dry_run` it decides and prints, including the command it
/// would run, but never runs `systemd-run`.
///
/// Console output is best effort: a write error is ignored, because nothing here may keep
/// the shell from starting.
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

/// The first line of every announcement.
const TRIAL_LINE: &str =
    "Apex: this boot is trying out a newly installed update and did not start fully.";

/// What the console says when the reboot is scheduled.
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

/// What the console says when the reboot could not be scheduled.
pub fn schedule_failed_message(error: &str) -> Vec<String> {
    vec![
        TRIAL_LINE.to_owned(),
        format!("Apex: cannot schedule the automatic reboot: {error}"),
        "Apex: restart the machine so that the boot counter moves on.".to_owned(),
    ]
}

/// What the console says when grubenv cannot be read.
pub fn unreadable_message(error: &str) -> Vec<String> {
    vec![format!(
        "Apex: cannot read the boot counter, so no automatic reboot: {error}"
    )]
}

fn dry(text: &str) -> String {
    format!("Apex (dry run): {text}")
}

/// `program args…` for display, with single quotes around arguments that need them.
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

    /// Every combination: reboot exactly when armed, not confirmed and counted (2, 1, 0).
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
                        // The assessment's own test for a reboot that moves the counter.
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
        // Armed during this boot, after GRUB ran: GRUB has not counted it.
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

    /// With the GRUB snippet's model: an update whose every boot stops in emergency mode
    /// gets exactly `BOOT_ATTEMPTS` watchdog reboots, then GRUB boots the previous deployment
    /// and the watchdog stays quiet on every later boot.
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

    /// A counter that GRUB does not move (the snippet missing or `save_env` failing after
    /// Apply) never leads to a reboot, so it cannot loop.
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
        // The console is at least 80 columns wide.
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

    /// The drop-in runs this module through the installed binary.
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
        // The delay must be long enough to read the console and act on it.
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
