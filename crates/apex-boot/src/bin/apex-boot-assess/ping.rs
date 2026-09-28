//! The required check "`apex-updated` answers `Ping`" ([`names::UPDATER_PING`]).
//!
//! `apex-updated` holds a `Ping` for up to 20 s (`PING_WAIT` in `apex-updated`'s engine)
//! while its first `bootc status` refresh runs, and then answers. Each call may therefore
//! take [`PING_TIMEOUT`], which is longer than that hold; a shorter per-call timeout would
//! abandon every call before the daemon's answer and turn a healthy but slow first start
//! into a red boot. Calls that fail early (activation refused, the daemon not on the bus
//! yet) are repeated after [`PING_RETRY_PAUSE`] while [`PING_BUDGET`] lasts; each retry gets
//! only what is left of the budget.

use std::sync::Arc;
use std::time::Duration;

use apex_boot::checks::names;
use apex_boot::{Check, FnCheck};

use crate::ports::{Clock, Updater};

/// The daemon's longest deliberate `Ping` hold (`apex_updated::engine::PING_WAIT`). The
/// library may not depend on `apex-updated`; a test reads the constant from its source.
pub(crate) const DAEMON_PING_WAIT: Duration = Duration::from_secs(20);

/// Time one `Ping` may take: the daemon's hold plus 5 s for activation and the reply.
pub(crate) const PING_TIMEOUT: Duration = DAEMON_PING_WAIT.saturating_add(Duration::from_secs(5));

/// Time all attempts together may take. Shorter than [`apex_boot::CHECK_TIMEOUT`], so the
/// check ends with the last error instead of a bare timeout.
pub(crate) const PING_BUDGET: Duration = Duration::from_secs(25);

/// Pause after a failed attempt (activation failures come back at once).
pub(crate) const PING_RETRY_PAUSE: Duration = Duration::from_secs(1);

/// Pings until one attempt succeeds or the budget is spent. Every attempt ends by the
/// budget's deadline.
pub(crate) fn ping_until_answered(updater: &dyn Updater, clock: &dyn Clock) -> Result<(), String> {
    let deadline = clock.now() + PING_BUDGET;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let left = deadline.saturating_duration_since(clock.now());
        let Err(err) = updater.ping(left.min(PING_TIMEOUT)) else {
            if attempts > 1 {
                tracing::info!(attempts, "apex-updated answered Ping");
            }
            return Ok(());
        };
        // Another attempt only if some of the budget is left after the pause.
        if clock.now() + PING_RETRY_PAUSE >= deadline {
            return Err(format!(
                "apex-updated did not answer Ping ({attempts} attempts): {err}"
            ));
        }
        tracing::debug!(attempts, %err, "Ping failed; retrying");
        clock.sleep(PING_RETRY_PAUSE);
    }
}

/// The required check.
pub(crate) fn check(updater: Arc<dyn Updater>, clock: Arc<dyn Clock>) -> Box<dyn Check> {
    Box::new(FnCheck::new(names::UPDATER_PING, move || {
        ping_until_answered(&*updater, &*clock)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{FakeClock, FakeUpdater};

    #[test]
    fn a_healthy_daemon_is_pinged_once() {
        let clock = Arc::new(FakeClock::new());
        let updater = Arc::new(FakeUpdater::healthy());
        assert_eq!(check(updater.clone(), clock.clone()).run(), Ok(()));
        assert_eq!(updater.pings(), 1);
        assert_eq!(clock.elapsed(), Duration::ZERO);
    }

    /// Finding: the daemon holds the first `Ping` for up to 20 s while it refreshes; a
    /// 5 s per-call timeout abandoned every call before that answer.
    #[test]
    fn a_daemon_that_answers_after_its_first_refresh_passes() {
        for answer_after in [Duration::from_secs(20), Duration::from_secs(24)] {
            let clock = Arc::new(FakeClock::new());
            let updater = FakeUpdater::healthy().answering_ping_after(answer_after, &clock);
            assert_eq!(ping_until_answered(&updater, &*clock), Ok(()));
            assert_eq!(updater.pings(), 1);
            assert_eq!(clock.elapsed(), answer_after);
        }
        assert!(PING_TIMEOUT > DAEMON_PING_WAIT);
    }

    #[test]
    fn a_failed_activation_is_retried() {
        let clock = Arc::new(FakeClock::new());
        let updater = FakeUpdater::healthy().ping_failing_times(2, Duration::ZERO, &clock);
        assert_eq!(ping_until_answered(&updater, &*clock), Ok(()));
        assert_eq!(updater.pings(), 3);
        assert_eq!(clock.elapsed(), 2 * PING_RETRY_PAUSE);
    }

    #[test]
    fn a_dead_daemon_fails_within_the_budget() {
        // A daemon that never answers: one call uses the whole budget.
        let clock = Arc::new(FakeClock::new());
        let updater = FakeUpdater::healthy().answering_ping_after(Duration::MAX, &clock);
        let err = ping_until_answered(&updater, &*clock).unwrap_err();
        assert_eq!(
            err,
            "apex-updated did not answer Ping (1 attempts): Ping timed out"
        );
        assert_eq!(clock.elapsed(), PING_BUDGET);

        // Immediate failures (activation refused) are retried every second.
        let clock = Arc::new(FakeClock::new());
        let updater = FakeUpdater::healthy().ping_failing_times(u32::MAX, Duration::ZERO, &clock);
        assert!(ping_until_answered(&updater, &*clock).is_err());
        assert_eq!(updater.pings(), 25);
        assert!(clock.elapsed() <= PING_BUDGET, "{:?}", clock.elapsed());

        // A failure late in the budget leaves only what is left for the next attempt.
        let clock = Arc::new(FakeClock::new());
        let updater = FakeUpdater::healthy().ping_failing_times(1, Duration::from_secs(20), &clock);
        assert_eq!(ping_until_answered(&updater, &*clock), Ok(()));
        assert_eq!(updater.last_ping_timeout(), Some(Duration::from_secs(4)));
    }

    /// The daemon's hold, read from `apex-updated`'s source: [`DAEMON_PING_WAIT`] must
    /// stay equal to it, and [`PING_TIMEOUT`] above it.
    #[test]
    fn the_timeout_covers_the_daemons_ping_hold() {
        let engine = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../apex-updated/src/engine.rs"
        ))
        .unwrap();
        let line = engine
            .lines()
            .find(|l| l.contains("pub const PING_WAIT: Duration"))
            .expect("apex-updated defines PING_WAIT");
        assert!(
            line.contains(&format!(
                "Duration::from_secs({})",
                DAEMON_PING_WAIT.as_secs()
            )),
            "{line}"
        );
        assert!(PING_TIMEOUT >= DAEMON_PING_WAIT + Duration::from_secs(5));
        assert!(PING_BUDGET >= PING_TIMEOUT);
    }
}
