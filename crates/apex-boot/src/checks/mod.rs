pub mod force_red;
pub mod grub_cfg;
pub mod units;

use std::any::Any;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared by the binary, the reports and `health-failure.json`.
pub mod names {
    pub const SESSION: &str = "session";
    pub const CORE_UNITS_ACTIVE: &str = "core-units-active";
    pub const UPDATER_PING: &str = "apex-updated-ping";
    pub const FAILED_UNITS: &str = "failed-units";
    pub const GRUB_CFG_MARKER: &str = "grub-cfg-marker";
    /// Added by [`crate::decide::decide`] when armed but the booted digest is unknown.
    pub const BOOTED_DIGEST: &str = "booted-digest";
    /// Added by the qualification hook [`super::force_red`], only on the boots it targets.
    pub const FORCED_RED: &str = "forced-red";

    pub const AUDIO_INIT_VERBS: &str = "audio-init-verbs";
    pub const CHARGE_THRESHOLD: &str = "charge-threshold";
    pub const DGPU_RUNTIME_PM: &str = "dgpu-runtime-pm";
    pub const NVIDIA_DRIVER: &str = "nvidia-driver";
    pub const FINGERPRINT_DEVICE: &str = "fingerprint-device";
    pub const GRUB_CFG_CURRENT: &str = "grub-cfg-current";
    pub const IMAGE_SOURCE: &str = "image-source";

    pub const REQUIRED: [&str; 5] = [
        SESSION,
        CORE_UNITS_ACTIVE,
        UPDATER_PING,
        FAILED_UNITS,
        GRUB_CFG_MARKER,
    ];

    /// Never influence the decision.
    pub const REPORTED: [&str; 7] = [
        AUDIO_INIT_VERBS,
        CHARGE_THRESHOLD,
        DGPU_RUNTIME_PM,
        NVIDIA_DRIVER,
        FINGERPRINT_DEVICE,
        GRUB_CFG_CURRENT,
        IMAGE_SOURCE,
    ];
}

/// Checks must not share locks with each other or with the caller: a check that times out
/// keeps running in the background.
pub trait Check: Send {
    fn name(&self) -> &str;

    fn run(&self) -> Result<(), String>;

    fn timeout(&self) -> Duration {
        CHECK_TIMEOUT
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    Pass,
    Fail(String),
    Timeout,
}

impl CheckOutcome {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub name: String,
    pub outcome: CheckOutcome,
}

impl CheckResult {
    pub fn pass(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: CheckOutcome::Pass,
        }
    }

    pub fn fail(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: CheckOutcome::Fail(reason.into()),
        }
    }

    pub fn timeout(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            outcome: CheckOutcome::Timeout,
        }
    }
}

pub struct FnCheck<F> {
    name: String,
    timeout: Duration,
    run: F,
}

impl<F> FnCheck<F>
where
    F: Fn() -> Result<(), String> + Send,
{
    pub fn new(name: impl Into<String>, run: F) -> Self {
        Self {
            name: name.into(),
            timeout: CHECK_TIMEOUT,
            run,
        }
    }

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

/// std threads cannot be cancelled: a check that times out is abandoned and its late result
/// discarded, so checks that start subprocesses must bound them themselves.
pub fn run_checks(checks: Vec<Box<dyn Check>>) -> Vec<CheckResult> {
    let start = Instant::now();
    let (sender, receiver) = mpsc::channel::<(usize, Result<(), String>)>();
    let mut slots: Vec<Slot> = Vec::with_capacity(checks.len());
    for (index, check) in checks.into_iter().enumerate() {
        let name = check.name().to_owned();
        let deadline = start.checked_add(check.timeout());
        let sender = sender.clone();
        let spawned = thread::Builder::new()
            .name(format!("check {}", name.replace('\0', "")))
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| check.run()))
                    .unwrap_or_else(|payload| Err(panic_message(payload.as_ref())));
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
