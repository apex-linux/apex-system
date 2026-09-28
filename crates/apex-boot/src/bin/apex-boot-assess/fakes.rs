//! Test doubles for [`crate::ports`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use apex_boot::{Digest, HealthFailure, Verdict};

use crate::ports::{
    BootedImage, Clock, FailureStore, HostStatus, Rebooter, SessionInfo, Sessions, Updater,
};

/// `unix_now` of a [`FakeClock`] that has not advanced.
pub(crate) const UNIX_START: u64 = 1_790_000_000;

/// A session with an age in seconds.
pub(crate) fn session(id: &str, class: &str, state: &str, seat: &str, age: u64) -> SessionInfo {
    SessionInfo {
        id: id.to_owned(),
        class: class.to_owned(),
        state: state.to_owned(),
        seat: seat.to_owned(),
        age: Some(Duration::from_secs(age)),
    }
}

/// A clock that only moves when something sleeps on it.
#[derive(Debug)]
pub(crate) struct FakeClock {
    start: Instant,
    elapsed: Mutex<Duration>,
}

impl FakeClock {
    /// A clock at its start.
    pub(crate) fn new() -> Self {
        Self {
            start: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    /// Time slept so far.
    pub(crate) fn elapsed(&self) -> Duration {
        *self.elapsed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves the clock forward.
    pub(crate) fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap_or_else(PoisonError::into_inner) += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.start + self.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        self.advance(duration);
    }

    fn unix_now(&self) -> u64 {
        UNIX_START + self.elapsed().as_secs()
    }
}

/// What logind answers at a given time since the clock's start.
pub(crate) type Script = Box<dyn Fn(Duration) -> Result<Vec<SessionInfo>, String> + Send + Sync>;

/// logind answering from a script over the fake clock's time.
pub(crate) struct FakeSessions {
    clock: Arc<FakeClock>,
    script: Mutex<Script>,
    calls: AtomicUsize,
}

impl FakeSessions {
    /// logind following `script`.
    pub(crate) fn new(
        clock: Arc<FakeClock>,
        script: impl Fn(Duration) -> Result<Vec<SessionInfo>, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            clock,
            script: Mutex::new(Box::new(script)),
            calls: AtomicUsize::new(0),
        }
    }

    /// Replaces the script (for example, a user logs in before the reboot).
    pub(crate) fn set(
        &self,
        script: impl Fn(Duration) -> Result<Vec<SessionInfo>, String> + Send + Sync + 'static,
    ) {
        *self.script.lock().unwrap_or_else(PoisonError::into_inner) = Box::new(script);
    }

    /// Number of `list` calls.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Sessions for FakeSessions {
    fn list(&self) -> Result<Vec<SessionInfo>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let script = self.script.lock().unwrap_or_else(PoisonError::into_inner);
        script(self.clock.elapsed())
    }
}

/// `apex-updated`, healthy or not.
#[derive(Default)]
pub(crate) struct FakeUpdater {
    /// Pings that still fail (`u32::MAX` means all).
    ping_failures: Mutex<u32>,
    /// How long a failing ping takes, on `clock`.
    ping_delay: Duration,
    /// How long a successful ping takes, on `clock` (a daemon holding `Ping`).
    ping_answer_after: Duration,
    clock: Option<Arc<FakeClock>>,
    report_error: Option<String>,
    /// Runs when `ReportBoot` is called, before its reply (the daemon's work).
    on_report: Option<Box<dyn Fn(Verdict) + Send + Sync>>,
    booted: Option<Result<Option<String>, String>>,
    pings: AtomicUsize,
    last_ping_timeout: Mutex<Option<Duration>>,
    reports: Mutex<Vec<(Verdict, Option<Digest>)>>,
}

impl std::fmt::Debug for FakeUpdater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeUpdater").finish_non_exhaustive()
    }
}

impl FakeUpdater {
    /// Answers everything; knows no booted digest.
    pub(crate) fn healthy() -> Self {
        Self::default()
    }

    /// Not on the bus: every call fails at once.
    pub(crate) fn unreachable() -> Self {
        Self {
            ping_failures: Mutex::new(u32::MAX),
            report_error: Some("cannot connect to the system bus: connection refused".to_owned()),
            booted: Some(Err(
                "cannot connect to the system bus: connection refused".to_owned()
            )),
            ..Self::default()
        }
    }

    /// The first `times` pings fail with "Ping timed out" after `delay` on `clock`.
    pub(crate) fn ping_failing_times(
        mut self,
        times: u32,
        delay: Duration,
        clock: &Arc<FakeClock>,
    ) -> Self {
        self.ping_failures = Mutex::new(times);
        self.ping_delay = delay;
        self.clock = Some(Arc::clone(clock));
        self
    }

    /// Every `Ping` succeeds after `delay` on `clock`, or times out after the caller's
    /// timeout when that is shorter.
    pub(crate) fn answering_ping_after(mut self, delay: Duration, clock: &Arc<FakeClock>) -> Self {
        self.ping_answer_after = delay;
        self.clock = Some(Arc::clone(clock));
        self
    }

    /// `ReportBoot` fails with `error`.
    pub(crate) fn report_failing(mut self, error: &str) -> Self {
        self.report_error = Some(error.to_owned());
        self
    }

    /// `effect` runs on every `ReportBoot`, before the reply (or the failure).
    pub(crate) fn on_report(mut self, effect: impl Fn(Verdict) + Send + Sync + 'static) -> Self {
        self.on_report = Some(Box::new(effect));
        self
    }

    /// The timeout of the last `Ping`.
    pub(crate) fn last_ping_timeout(&self) -> Option<Duration> {
        *self
            .last_ping_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The daemon's answer for the booted digest.
    pub(crate) fn with_booted(mut self, booted: Result<Option<String>, String>) -> Self {
        self.booted = Some(booted);
        self
    }

    /// Number of pings.
    pub(crate) fn pings(&self) -> usize {
        self.pings.load(Ordering::SeqCst)
    }

    /// Every `ReportBoot` call, failed ones included.
    pub(crate) fn reports(&self) -> Vec<(Verdict, Option<Digest>)> {
        self.reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Updater for FakeUpdater {
    fn ping(&self, timeout: Duration) -> Result<(), String> {
        self.pings.fetch_add(1, Ordering::SeqCst);
        *self
            .last_ping_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(timeout);
        let mut failures = self
            .ping_failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *failures == 0 {
            drop(failures);
            let Some(clock) = &self.clock else {
                return Ok(());
            };
            if self.ping_answer_after > timeout {
                clock.advance(timeout);
                return Err("Ping timed out".to_owned());
            }
            clock.advance(self.ping_answer_after);
            return Ok(());
        }
        if *failures != u32::MAX {
            *failures -= 1;
        }
        drop(failures);
        if let Some(clock) = &self.clock {
            clock.advance(self.ping_delay);
        }
        Err(if self.ping_delay.is_zero() {
            "cannot connect to the system bus: connection refused".to_owned()
        } else {
            "Ping timed out".to_owned()
        })
    }

    fn report_boot(&self, verdict: Verdict, digest: Option<&Digest>) -> Result<(), String> {
        self.reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((verdict, digest.cloned()));
        if let Some(effect) = &self.on_report {
            effect(verdict);
        }
        self.report_error.clone().map_or(Ok(()), Err)
    }

    fn booted_digest(&self) -> Result<Option<String>, String> {
        self.booted.clone().unwrap_or(Ok(None))
    }
}

/// `bootc status`: a fixed answer that a test can replace (for example after the daemon's
/// `bootc rollback`).
#[derive(Debug)]
pub(crate) struct FakeHost {
    answer: Mutex<Result<BootedImage, String>>,
    calls: AtomicUsize,
}

impl FakeHost {
    /// bootc answers `answer`.
    pub(crate) fn new(answer: Result<BootedImage, String>) -> Self {
        Self {
            answer: Mutex::new(answer),
            calls: AtomicUsize::new(0),
        }
    }

    /// bootc reports `digest` as booted and first in the boot order, from the stable
    /// image, verified.
    pub(crate) fn booted(digest: &Digest) -> Self {
        Self::new(Ok(booted_image(digest, None)))
    }

    /// `digest` is booted, and `first` comes first in the boot order: the rollback
    /// deployment is queued when `first` is not the booted one.
    pub(crate) fn booted_with_first(digest: &Digest, first: &Digest) -> Self {
        Self::new(Ok(booted_image(digest, Some(first))))
    }

    /// Replaces the answer.
    pub(crate) fn set(&self, answer: Result<BootedImage, String>) {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner) = answer;
    }

    /// Number of `bootc status` calls.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// `bootc status` for `booted`, with `first` as entry 0 when given and not the booted one.
pub(crate) fn booted_image(booted: &Digest, first: Option<&Digest>) -> BootedImage {
    let queued = first.filter(|f| *f != booted);
    BootedImage {
        digest: Some(booted.as_str().to_owned()),
        image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
        transport: Some("registry".to_owned()),
        signature: Some(apex_bootc::ImageSignature::ContainerPolicy),
        rollback_digest: queued.map(|d| d.as_str().to_owned()),
        rollback_queued: queued.is_some(),
    }
}

impl HostStatus for FakeHost {
    fn booted(&self) -> Result<BootedImage, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.answer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// `health-failure.json` in memory.
#[derive(Debug, Default)]
pub(crate) struct FakeStore {
    error: Option<String>,
    records: Mutex<Vec<HealthFailure>>,
}

impl FakeStore {
    /// A store whose writes fail with `error`.
    pub(crate) fn failing(error: &str) -> Self {
        Self {
            error: Some(error.to_owned()),
            ..Self::default()
        }
    }

    /// Every record written successfully.
    pub(crate) fn records(&self) -> Vec<HealthFailure> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl FailureStore for FakeStore {
    fn record(&self, failure: &HealthFailure) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(failure.clone());
        Ok(())
    }
}

/// `systemctl reboot` that only counts.
#[derive(Debug, Default)]
pub(crate) struct FakeRebooter {
    error: Option<String>,
    reboots: AtomicUsize,
}

impl FakeRebooter {
    /// A rebooter that fails with `error`.
    pub(crate) fn failing(error: &str) -> Self {
        Self {
            error: Some(error.to_owned()),
            ..Self::default()
        }
    }

    /// Number of reboot requests, failed ones included.
    pub(crate) fn reboots(&self) -> usize {
        self.reboots.load(Ordering::SeqCst)
    }
}

impl Rebooter for FakeRebooter {
    fn reboot(&self) -> Result<(), String> {
        self.reboots.fetch_add(1, Ordering::SeqCst);
        self.error.clone().map_or(Ok(()), Err)
    }
}
