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
    ArmingRefused, BOOT_ATTEMPTS, BootVar, BootVars, BootVarsError, Booted, CounterState, EnvOp,
    ValueError, arm_ops, check_arming, disarm_ops,
};
pub use writer::{Editenv, GrubEnvWriter, RecordingWriter, WriteError, apply_ops};

pub const ASSESS_BINARY_PATH: &str = "/usr/libexec/apex/apex-boot-assess";

pub const SERVICE_UNIT: &str = include_str!("../data/apex-boot-assess.service");

pub const SESSION_MIN_ACTIVE: Duration = Duration::from_secs(20);

/// After this the checks run anyway, and the session check fails.
pub const SESSION_WAIT: Duration = Duration::from_secs(120);

/// Includes the D-Bus activation of `apex-updated`.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(30);

pub const LOGIND_TIMEOUT: Duration = Duration::from_secs(5);

pub const DAEMON_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

pub const REBOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// A helper in uninterruptible sleep (a stalled `/boot`) survives `SIGKILL`: abandon it after this.
pub const KILL_REAP_GRACE: Duration = Duration::from_secs(5);

pub const RECORD_TIMEOUT: Duration = Duration::from_secs(10);

/// apex-bootc's 30 s status timeout plus 5 s kill grace. apex-bootc may then still wait for the
/// reap, so the binary enforces this bound, and its tests keep it in step with apex-bootc.
pub const BOOTC_STATUS_BOUND: Duration = Duration::from_secs(35);

const fn max_secs(a: Duration, b: Duration) -> u64 {
    if a.as_secs() > b.as_secs() {
        a.as_secs()
    } else {
        b.as_secs()
    }
}

const EDITENV_BOUND_SECS: u64 = writer::EDITENV_TIMEOUT.as_secs() + KILL_REAP_GRACE.as_secs();

const SLOWEST_DECISION_STEPS_SECS: u64 = {
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

/// Counts every bounded wait of the binary. The unit's `TimeoutStartSec=` must cover it, or
/// systemd could kill the assessment between its grubenv write and its report.
pub const ASSESS_TIME_BUDGET: Duration = Duration::from_secs(
    SESSION_WAIT.as_secs()
        + LOGIND_TIMEOUT.as_secs()
        + max_secs(CHECK_TIMEOUT, BOOTC_STATUS_BOUND)
        + DAEMON_QUERY_TIMEOUT.as_secs()
        + LOGIND_TIMEOUT.as_secs()
        + SLOWEST_DECISION_STEPS_SECS,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_is_305_seconds() {
        assert_eq!(EDITENV_BOUND_SECS, 35);
        assert_eq!(SLOWEST_DECISION_STEPS_SECS, 135);
        assert_eq!(ASSESS_TIME_BUDGET, Duration::from_secs(305));
        assert!(BOOTC_STATUS_BOUND > CHECK_TIMEOUT);
    }
}
