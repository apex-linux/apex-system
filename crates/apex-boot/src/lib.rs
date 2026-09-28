//! Apex boot assessment (ADR 0009): decides after every boot whether the running
//! deployment is healthy, and drives the GRUB boot counter that rolls a broken update back.
//!
//! The pieces, from the bottom up:
//!
//! - [`grubenv`]: the GRUB environment block, parsed and edited byte for byte as GRUB does.
//! - [`vars`]: the three variables Apex owns (`apex_target`, `apex_counter`, `apex_ok`),
//!   what GRUB did with the counter ([`CounterState`]), and the [`arm_ops`] /
//!   [`disarm_ops`] that `apex-updated` uses on `Apply` and `DiscardStaged`. The names are
//!   Apex's own because Fedora's `grub2-set-bootflag` sets `boot_success=1` after a login,
//!   which would mark a failed armed boot as good.
//! - [`snippet`]: the GRUB snippet `08_apex-bootcount.cfg` and a model of it.
//! - [`writer`]: the [`GrubEnvWriter`] boundary. Production writes run `grub2-editenv` in a
//!   private mount namespace in which the read-only `/boot` of a bootc system is remounted
//!   read-write ([`Editenv::system`]).
//! - [`checks`]: the [`Check`] trait, the concurrent runner with per-check timeouts, the
//!   local checks (grub.cfg marker, failed units, active units) and the qualification hook
//!   [`checks::force_red`].
//! - [`mod@decide`]: the pure decision table and the ordered [`Step`]s that carry it out.
//! - [`health`]: the `health-failure.json` record.
//! - [`emergency`]: the early-boot watchdog that the `emergency.service` and
//!   `rescue.service` drop-ins run (`apex-boot-assess --emergency-watchdog`): the pure
//!   decision, and its two helper calls (`head` for grubenv, `systemd-run` for the reboot)
//!   with every program and path explicit, so tests drive them with fakes.
//!
//! How `apex-boot-assess` uses it (the binary owns all I/O and D-Bus):
//!
//! 1. Wait, bounded by [`SESSION_WAIT`], for a greeter or user session.
//! 2. [`run_checks`] on the required checks (its own session and `Ping` checks plus
//!    [`checks::units::ActiveUnitsCheck`], [`checks::units::FailedUnitsCheck`],
//!    [`checks::grub_cfg::GrubCfgMarkerCheck`]) and the reported ones, in one concurrent
//!    batch, with `bootc status` alongside for the booted digest.
//! 3. Add the failed check `forced-red` when the qualification hook names the booted
//!    digest (see below).
//! 4. Read grubenv ([`grubenv::read_file`], [`BootVars::from_env`]); if the variables
//!    cannot be interpreted, change nothing and exit with failure.
//! 5. [`decide()`] (it also needs the boot order from `bootc status`,
//!    [`DefaultDeployment`]), then perform [`Decision::steps`] in order: [`apply_ops`] with
//!    [`Editenv::system`], write [`HealthFailure`], call `ReportBoot`, after a failed
//!    fallback report reconcile grubenv with the boot order ([`reconcile_fallback`]), and
//!    reboot last.
//!
//! # Arming precondition
//!
//! `Apply` arms only when the booted deployment comes from an image, a different image is
//! staged, and the booted deployment is known good ([`check_arming`]: grubenv is not
//! armed for it and it has no recorded health failure). When the staged deployment is
//! finalized, ostree makes it entry 0 and keeps the booted deployment as entry 1, so GRUB's
//! fallback target (entry 1) is always a real deployment; any older deployment that is not
//! pinned is pruned, which is why the booted one must be good. See [`arm_ops`].
//!
//! # Qualification hook: forced red
//!
//! The KVM qualification (ADR 0009, "an injected red check leads to a counted rollback")
//! needs a boot that fails on purpose. If `/var/lib/apex/force-red` exists and its trimmed
//! content equals the booted image digest, the assessment adds a failed required check
//! named `forced-red` ([`checks::names::FORCED_RED`]), and the decision table treats that
//! boot like any other red boot. It changes nothing when the file is absent, names another
//! digest, or the booted digest is unknown. `/var/lib/apex` is `0700 root`, so only root
//! can create the file, and it is ignored if it or its directory is writable by group or
//! others, or if it is not a regular file. Because it names one digest, it cannot outlive
//! the deployment it targets: after the counted rollback the previous deployment boots
//! with another digest and is judged normally. Details in [`checks::force_red`].
//!
//! # Early-boot watchdog: a boot that stops in emergency or rescue mode
//!
//! The assessment is the only userspace component that judges a boot, and it runs after
//! `graphical.target`. A boot that stops before that in `emergency.target` or
//! `rescue.target` (the shell after a failed `local-fs.target` dependency, which waits for
//! Enter because the root account is locked) is never judged. Drop-ins for
//! `emergency.service` and `rescue.service` ([`emergency::EMERGENCY_DROP_IN`],
//! [`emergency::RESCUE_DROP_IN`]) therefore run `apex-boot-assess --emergency-watchdog`
//! before the shell starts. When GRUB counted this boot of an armed update (`apex_target`
//! set, `apex_ok` not 1, `apex_counter` 2, 1 or 0; [`emergency::action_for`]) it schedules
//! `systemctl reboot` in 60 s through `systemd-run`, announced on the console together
//! with the command that cancels it; otherwise it does nothing. The counter bounds these
//! reboots like the assessment's own, so GRUB falls back after three attempts. Details and
//! the reasoning in [`emergency`].
//!
//! Still not covered: a boot that hangs in a start job without a timeout never reaches
//! either the assessment or an emergency shell; the counter then moves only when the user
//! forces a power-off. `panic=10` covers kernel panics; an initramfs failure waits at the
//! dracut rescue shell, and a forced power-off moves the counter.
//!
//! # Time budget
//!
//! [`ASSESS_TIME_BUDGET`] is the only statement of how long an assessment can take. It is
//! built from the timeouts below, which the binary uses for its bounded waits, and the
//! tests hold the unit's `TimeoutStartSec=` and its comment to it. The watchdog has its own,
//! much shorter bound, [`emergency::WATCHDOG_TIME_BUDGET`], which must fit inside the start
//! timeout of the shell units.

pub mod checks;
pub mod decide;
pub mod digest;
pub mod emergency;
pub mod grubenv;
pub mod health;
mod process;
pub mod snippet;
pub mod vars;
pub mod writer;

use std::time::Duration;

pub use checks::{CHECK_TIMEOUT, Check, CheckOutcome, CheckResult, FnCheck, run_checks};
pub use decide::{
    AssessInput, Decision, DefaultDeployment, HEDGE_COUNTER, Step, Verdict, decide,
    reconcile_fallback,
};
pub use digest::{Digest, DigestError};
pub use grubenv::GrubEnv;
pub use health::HealthFailure;
pub use vars::{
    ArmingRefused, BOOT_ATTEMPTS, BootVar, BootVars, BootVarsError, CounterState, EnvOp,
    ValueError, arm_ops, check_arming, disarm_ops,
};
pub use writer::{Editenv, GrubEnvWriter, RecordingWriter, WriteError, apply_ops};

/// Where the binary is installed (M1 contract §2).
pub const ASSESS_BINARY_PATH: &str = "/usr/libexec/apex/apex-boot-assess";

/// The systemd unit that runs the assessment once per boot, exactly as installed in
/// `/usr/lib/systemd/system/`.
pub const SERVICE_UNIT: &str = include_str!("../data/apex-boot-assess.service");

/// A greeter or user session must have been active this long (ADR 0009).
pub const SESSION_MIN_ACTIVE: Duration = Duration::from_secs(20);

/// How long the binary waits for a greeter or user session to appear before it runs the
/// checks anyway (the session check then fails).
pub const SESSION_WAIT: Duration = Duration::from_secs(120);

/// How long the `ReportBoot` D-Bus call may take, `apex-updated` activation included. A
/// fallback report that times out is not trusted either way (see [`mod@decide`]).
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one logind query (`ListSessions` plus every session's properties) may take.
pub const LOGIND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the fallback query of the daemon's `Booted` property may take.
pub const DAEMON_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `systemctl reboot` may take to queue the reboot.
pub const REBOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// After a helper (`grub2-editenv`, `systemctl`) was killed for running too long, how long
/// its exit is waited for before it is abandoned. A process in uninterruptible sleep (a
/// stalled `/boot`) does not die on `SIGKILL`; waiting for it without a bound would stall
/// the assessment until systemd kills the unit.
pub const KILL_REAP_GRACE: Duration = Duration::from_secs(5);

/// How long writing `health-failure.json` (temporary file, fsync, rename, directory fsync)
/// may take before the step is reported as failed and its thread abandoned.
pub const RECORD_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest one `bootc status` may take. apex-bootc's own status timeout (30 s) and
/// kill grace (5 s) come first (M1 contract §6); apex-bootc may then wait for the killed
/// process to be reaped, so the binary enforces this bound itself and abandons the call.
/// The library may not depend on apex-bootc (M1 contract §9), so the binary's tests keep
/// this at least apex-bootc's `STATUS_TIMEOUT + KILL_GRACE`.
pub const BOOTC_STATUS_BOUND: Duration = Duration::from_secs(35);

const fn max_secs(a: Duration, b: Duration) -> u64 {
    if a.as_secs() > b.as_secs() {
        a.as_secs()
    } else {
        b.as_secs()
    }
}

/// One `grub2-editenv` call, the wait for a killed helper included.
const EDITENV_BOUND_SECS: u64 = writer::EDITENV_TIMEOUT.as_secs() + KILL_REAP_GRACE.as_secs();

/// The steps of the slowest decision:
///
/// - green, inconsistent, target skipped: two `grub2-editenv` calls and the report
///   (100 s);
/// - red without a session: one call, the record, the report, the logind re-check and
///   `systemctl reboot` (115 s);
/// - fallback: the hedge, the report, then either the disarm or, after a failed report,
///   `bootc status` and one more call (135 s).
const STEPS_BOUND_SECS: u64 = {
    let confirm = 2 * EDITENV_BOUND_SECS + REPORT_TIMEOUT.as_secs();
    let reboot = EDITENV_BOUND_SECS
        + RECORD_TIMEOUT.as_secs()
        + REPORT_TIMEOUT.as_secs()
        + LOGIND_TIMEOUT.as_secs()
        + REBOOT_TIMEOUT.as_secs()
        + KILL_REAP_GRACE.as_secs();
    let reconcile = BOOTC_STATUS_BOUND.as_secs() + EDITENV_BOUND_SECS;
    let fallback = EDITENV_BOUND_SECS
        + REPORT_TIMEOUT.as_secs()
        + if reconcile > EDITENV_BOUND_SECS {
            reconcile
        } else {
            EDITENV_BOUND_SECS
        };
    let mut max = confirm;
    if reboot > max {
        max = reboot;
    }
    if fallback > max {
        max = fallback;
    }
    max
};

/// The longest a complete run of `apex-boot-assess` can take, in the order the binary
/// works:
///
/// 1. the session wait ([`SESSION_WAIT`]), plus one logind query that starts just before it
///    ends ([`LOGIND_TIMEOUT`]);
/// 2. one concurrent batch of every check (each bounded by [`CHECK_TIMEOUT`]) with
///    `bootc status` alongside ([`BOOTC_STATUS_BOUND`]): the longer of the two;
/// 3. the daemon fallback for the booted digest ([`DAEMON_QUERY_TIMEOUT`]);
/// 4. the logind query for `user_session_active` ([`LOGIND_TIMEOUT`]);
/// 5. the steps of the slowest decision (the fallback: the hedge, the report, `bootc
///    status` and one more `grub2-editenv` call; each call 30 s plus [`KILL_REAP_GRACE`]).
///
/// 120 + 5 + 35 + 5 + 5 + 135 = 305 s. The unit's `TimeoutStartSec=` must be at least this,
/// or systemd could kill the assessment between its grubenv write and its report; the
/// tests enforce that, and that the unit's comment states this figure. Every bounded wait
/// added to the binary must be added here.
pub const ASSESS_TIME_BUDGET: Duration = Duration::from_secs(
    SESSION_WAIT.as_secs()
        + LOGIND_TIMEOUT.as_secs()
        + max_secs(CHECK_TIMEOUT, BOOTC_STATUS_BOUND)
        + DAEMON_QUERY_TIMEOUT.as_secs()
        + LOGIND_TIMEOUT.as_secs()
        + STEPS_BOUND_SECS,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_the_documented_sum() {
        assert_eq!(EDITENV_BOUND_SECS, 35);
        assert_eq!(STEPS_BOUND_SECS, 135);
        assert_eq!(ASSESS_TIME_BUDGET, Duration::from_secs(305));
        // The bootc bound is the longer of the two in the concurrent batch.
        assert!(BOOTC_STATUS_BOUND > CHECK_TIMEOUT);
    }
}
