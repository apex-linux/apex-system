//! Health checks: the [`Check`] trait, a runner with per-check timeouts, and the checks
//! that need nothing but local files and `systemctl`.
//!
//! The binary adds the checks that need D-Bus or logind (the session check and the
//! `apex-updated` ping) by implementing [`Check`], for example with [`FnCheck`].

pub mod force_red;
pub mod grub_cfg;
pub mod units;

use std::any::Any;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// Time each check gets (ADR 0009).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Check names, shared by the binary, the reports and `health-failure.json`.
pub mod names {
    /// Required: a greeter or user session has been active for at least 20 s.
    pub const SESSION: &str = "session";
    /// Required: `NetworkManager` and `systemd-logind` are active.
    pub const CORE_UNITS_ACTIVE: &str = "core-units-active";
    /// Required: `apex-updated` answers `Ping`.
    pub const UPDATER_PING: &str = "apex-updated-ping";
    /// Required: none of [`super::units::REQUIRED_UNITS`] has failed.
    pub const FAILED_UNITS: &str = "failed-units";
    /// Required: `/boot/grub2/grub.cfg` contains the snippet marker, in a version this
    /// image can drive.
    pub const GRUB_CFG_MARKER: &str = "grub-cfg-marker";
    /// Added by [`crate::decide::decide`] when the counter is armed but the booted digest
    /// is unknown.
    pub const BOOTED_DIGEST: &str = "booted-digest";
    /// Added as a failed required check by the qualification hook
    /// ([`super::force_red`]) when `/var/lib/apex/force-red` names the booted digest. Never
    /// part of [`REQUIRED`]: it exists only on the boots the hook targets.
    pub const FORCED_RED: &str = "forced-red";

    /// Reported only: the HDA `init_verbs` contain the generated verbs.
    pub const AUDIO_INIT_VERBS: &str = "audio-init-verbs";
    /// Reported only: the battery charge threshold is applied.
    pub const CHARGE_THRESHOLD: &str = "charge-threshold";
    /// Reported only: the dGPU runtime PM is `auto`.
    pub const DGPU_RUNTIME_PM: &str = "dgpu-runtime-pm";
    /// Reported only: the fingerprint reader is present.
    pub const FINGERPRINT_DEVICE: &str = "fingerprint-device";
    /// Reported only: the snippet in `grub.cfg` is the version this image ships.
    pub const GRUB_CFG_CURRENT: &str = "grub-cfg-current";
    /// Reported only: the booted image is a registry image from the expected repository,
    /// verified by policy.
    pub const IMAGE_SOURCE: &str = "image-source";

    /// The required checks, whose failure makes a boot red.
    pub const REQUIRED: [&str; 5] = [
        SESSION,
        CORE_UNITS_ACTIVE,
        UPDATER_PING,
        FAILED_UNITS,
        GRUB_CFG_MARKER,
    ];

    /// The checks that are only reported and never cause a rollback.
    pub const REPORTED: [&str; 6] = [
        AUDIO_INIT_VERBS,
        CHARGE_THRESHOLD,
        DGPU_RUNTIME_PM,
        FINGERPRINT_DEVICE,
        GRUB_CFG_CURRENT,
        IMAGE_SOURCE,
    ];
}

/// A health check.
///
/// `run` is the I/O boundary: it may block, and it runs on its own thread (hence `Send`).
/// Checks must not share locks with each other or with the caller, because a check that
/// times out keeps running in the background (see [`run_checks`]).
pub trait Check: Send {
    /// A stable identifier (see [`names`]).
    fn name(&self) -> &str;

    /// Performs the check. `Err` carries a human-readable reason.
    fn run(&self) -> Result<(), String>;

    /// How long the runner waits for this check.
    fn timeout(&self) -> Duration {
        CHECK_TIMEOUT
    }
}

/// How a check ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The check passed.
    Pass,
    /// The check failed, with its reason.
    Fail(String),
    /// The check did not finish within its timeout.
    Timeout,
}

impl CheckOutcome {
    /// Whether the check passed.
    pub fn is_pass(&self) -> bool {
        *self == Self::Pass
    }
}

impl fmt::Display for CheckOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckOutcome::Pass => f.write_str("pass"),
            CheckOutcome::Fail(reason) => write!(f, "fail: {reason}"),
            CheckOutcome::Timeout => f.write_str("timeout"),
        }
    }
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// The check's name.
    pub name: String,
    /// How it ended.
    pub outcome: CheckOutcome,
}

impl CheckResult {
    /// A passed check.
    pub fn pass(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: CheckOutcome::Pass,
        }
    }

    /// A failed check.
    pub fn fail(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: CheckOutcome::Fail(reason.into()),
        }
    }

    /// A check that timed out.
    pub fn timeout(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: CheckOutcome::Timeout,
        }
    }
}

/// A check made from a closure, for checks the binary implements itself.
pub struct FnCheck<F> {
    name: String,
    timeout: Duration,
    run: F,
}

impl<F> FnCheck<F>
where
    F: Fn() -> Result<(), String> + Send,
{
    /// A check named `name` that runs `run`, with the default timeout.
    pub fn new(name: impl Into<String>, run: F) -> Self {
        Self {
            name: name.into(),
            timeout: CHECK_TIMEOUT,
            run,
        }
    }

    /// Replaces the timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl<F> fmt::Debug for FnCheck<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnCheck")
            .field("name", &self.name)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl<F> Check for FnCheck<F>
where
    F: Fn() -> Result<(), String> + Send,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn run(&self) -> Result<(), String> {
        (self.run)()
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Runs every check concurrently, each on its own thread, and returns one result per
/// check in the order given.
///
/// Each check's deadline is its [`Check::timeout`] counted from the start of the run; the
/// runner waits on a channel with `recv_timeout` until every check has reported or passed
/// its deadline, so the whole run takes at most the longest timeout. A check that panics is
/// a failure with the panic message; a check whose thread cannot be started is a failure.
///
/// A check that times out is abandoned, not stopped: std threads cannot be cancelled, so
/// its thread is detached and keeps running until it returns or the process exits, and its
/// late result is discarded. Checks that start subprocesses should bound them themselves
/// (the ones in this module kill their `systemctl` at the deadline). `apex-boot-assess`
/// exits right after its decision, which ends any abandoned thread.
pub fn run_checks(checks: Vec<Box<dyn Check>>) -> Vec<CheckResult> {
    let start = Instant::now();
    let (sender, receiver) = mpsc::channel::<(usize, Result<(), String>)>();
    let mut slots: Vec<Slot> = Vec::with_capacity(checks.len());
    for (index, check) in checks.into_iter().enumerate() {
        let name = check.name().to_owned();
        // A timeout too large to add means "wait as long as it takes".
        let deadline = start.checked_add(check.timeout());
        let sender = sender.clone();
        let spawned = thread::Builder::new()
            // Thread names cannot contain NUL; the name is only a debugging aid.
            .name(format!("check {}", name.replace('\0', "")))
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| check.run()))
                    .unwrap_or_else(|payload| Err(panic_message(payload.as_ref())));
                // The runner has stopped listening once this check timed out.
                let _ = sender.send((index, result));
            });
        let outcome = spawned
            .err()
            .map(|err| CheckOutcome::Fail(format!("cannot start the check thread: {err}")));
        slots.push(Slot {
            name,
            deadline,
            outcome,
        });
    }
    drop(sender);

    loop {
        let now = Instant::now();
        for slot in &mut slots {
            if slot.outcome.is_none() && slot.deadline.is_some_and(|d| now >= d) {
                slot.outcome = Some(CheckOutcome::Timeout);
            }
        }
        let pending = slots.iter().filter(|s| s.outcome.is_none());
        // `None` (no deadline) sorts after every real deadline.
        let Some(next_deadline) = pending
            .map(|s| s.deadline)
            .min_by_key(|d| (d.is_none(), *d))
        else {
            break;
        };
        let message = match next_deadline {
            Some(deadline) => receiver.recv_timeout(deadline.saturating_duration_since(now)),
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match message {
            Ok((index, result)) => {
                if let Some(slot) = slots.get_mut(index).filter(|s| s.outcome.is_none()) {
                    slot.outcome = Some(match result {
                        Ok(()) => CheckOutcome::Pass,
                        Err(reason) => CheckOutcome::Fail(reason),
                    });
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Every thread has ended; a check still pending never reported.
                for slot in slots.iter_mut().filter(|s| s.outcome.is_none()) {
                    slot.outcome = Some(CheckOutcome::Fail(
                        "the check ended without a result".to_owned(),
                    ));
                }
                break;
            }
        }
    }

    slots
        .into_iter()
        .map(|slot| CheckResult {
            name: slot.name,
            outcome: slot.outcome.unwrap_or(CheckOutcome::Timeout),
        })
        .collect()
}

/// One check's bookkeeping in [`run_checks`].
struct Slot {
    name: String,
    /// `None` when the timeout does not fit in an `Instant`.
    deadline: Option<Instant>,
    outcome: Option<CheckOutcome>,
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_owned());
    format!("the check panicked: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn check(
        name: &str,
        timeout: Duration,
        run: impl Fn() -> Result<(), String> + Send + 'static,
    ) -> Box<dyn Check> {
        Box::new(FnCheck::new(name, run).with_timeout(timeout))
    }

    #[test]
    fn results_keep_input_order() {
        let results = run_checks(vec![
            check("slow-pass", Duration::from_secs(10), || {
                thread::sleep(Duration::from_millis(100));
                Ok(())
            }),
            check("fail", Duration::from_secs(10), || Err("broken".to_owned())),
            check("pass", Duration::from_secs(10), || Ok(())),
        ]);
        assert_eq!(
            results,
            vec![
                CheckResult::pass("slow-pass"),
                CheckResult::fail("fail", "broken"),
                CheckResult::pass("pass"),
            ]
        );
    }

    #[test]
    fn checks_run_concurrently() {
        // Each check waits for all three: this only finishes if they run at the same time.
        let barrier = Arc::new(Barrier::new(3));
        let checks = (0..3)
            .map(|i| {
                let barrier = Arc::clone(&barrier);
                check(&format!("c{i}"), Duration::from_secs(20), move || {
                    barrier.wait();
                    Ok(())
                })
            })
            .collect();
        let results = run_checks(checks);
        assert!(results.iter().all(|r| r.outcome.is_pass()), "{results:?}");
    }

    #[test]
    fn a_hung_check_times_out_without_delaying_the_others() {
        let start = Instant::now();
        let results = run_checks(vec![
            check("hangs", Duration::from_millis(200), || {
                // Detached after the timeout; parks until the test process exits.
                loop {
                    thread::park();
                }
            }),
            check("quick", Duration::from_secs(10), || Ok(())),
        ]);
        assert_eq!(
            results,
            vec![CheckResult::timeout("hangs"), CheckResult::pass("quick")]
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn each_check_has_its_own_deadline() {
        let results = run_checks(vec![
            check("short", Duration::from_millis(50), || {
                thread::sleep(Duration::from_millis(400));
                Ok(())
            }),
            check("long", Duration::from_secs(10), || {
                thread::sleep(Duration::from_millis(400));
                Ok(())
            }),
        ]);
        assert_eq!(
            results,
            vec![CheckResult::timeout("short"), CheckResult::pass("long")]
        );
    }

    #[test]
    fn a_panic_is_a_failure() {
        let results = run_checks(vec![check("panics", Duration::from_secs(10), || {
            panic!("boom")
        })]);
        assert_eq!(
            results,
            vec![CheckResult::fail("panics", "the check panicked: boom")]
        );
    }

    #[test]
    fn huge_timeouts_do_not_overflow() {
        let results = run_checks(vec![check("forever", Duration::MAX, || Ok(()))]);
        assert_eq!(results, vec![CheckResult::pass("forever")]);
    }

    #[test]
    fn no_checks_no_results() {
        assert!(run_checks(Vec::new()).is_empty());
    }

    #[test]
    fn names_are_unique_and_split() {
        let mut all: Vec<&str> = names::REQUIRED
            .iter()
            .chain(names::REPORTED.iter())
            .copied()
            .collect();
        all.push(names::BOOTED_DIGEST);
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count);
    }
}
