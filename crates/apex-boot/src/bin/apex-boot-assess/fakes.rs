use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use apex_boot::{Digest, HealthFailure, Verdict};

use crate::ports::{
    BootedImage, Clock, FailureStore, HostStatus, Rebooter, SessionInfo, Sessions, Updater,
};

pub(crate) const UNIX_START: u64 = 1_790_000_000;

pub(crate) fn session(id: &str, class: &str, state: &str, seat: &str, age: u64) -> SessionInfo {
    SessionInfo {
        id: id.to_owned(),
        class: class.to_owned(),
        state: state.to_owned(),
        seat: seat.to_owned(),
        age: Some(Duration::from_secs(age)),
    }
}

/// Moves only when something sleeps on it or advances it.
#[derive(Debug)]
pub(crate) struct FakeClock {
    start: Instant,
    elapsed: Mutex<Duration>,
}

impl FakeClock {
    pub(crate) fn new() -> Self {
        Self {
            start: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        *self.elapsed.lock().unwrap_or_else(PoisonError::into_inner)
    }

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

pub(crate) struct FakeSessions {
    clock: Arc<FakeClock>,
    script: Mutex<Script>,
    calls: AtomicUsize,
}

impl FakeSessions {
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

    pub(crate) fn set(
        &self,
        script: impl Fn(Duration) -> Result<Vec<SessionInfo>, String> + Send + Sync + 'static,
    ) {
        *self.script.lock().unwrap_or_else(PoisonError::into_inner) = Box::new(script);
    }

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

#[derive(Default)]
pub(crate) struct FakeUpdater {
    /// `u32::MAX` means all.
    ping_failures_left: Mutex<u32>,
    failing_ping_delay: Duration,
    ping_answer_after: Duration,
    clock: Option<Arc<FakeClock>>,
    report_error: Option<String>,
    /// The daemon's work, before its reply.
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
    /// Knows no booted digest.
    pub(crate) fn healthy() -> Self {
        Self::default()
    }

    pub(crate) fn unreachable() -> Self {
        Self {
            ping_failures_left: Mutex::new(u32::MAX),
            report_error: Some("cannot connect to the system bus: connection refused".to_owned()),
            booted: Some(Err(
                "cannot connect to the system bus: connection refused".to_owned()
            )),
            ..Self::default()
        }
    }

    pub(crate) fn ping_failing_times(
        mut self,
        times: u32,
        delay: Duration,
        clock: &Arc<FakeClock>,
    ) -> Self {
        self.ping_failures_left = Mutex::new(times);
        self.failing_ping_delay = delay;
        self.clock = Some(Arc::clone(clock));
        self
    }

    /// Times out instead when the caller's timeout is shorter.
    pub(crate) fn answering_ping_after(mut self, delay: Duration, clock: &Arc<FakeClock>) -> Self {
        self.ping_answer_after = delay;
        self.clock = Some(Arc::clone(clock));
        self
    }

    pub(crate) fn report_failing(mut self, error: &str) -> Self {
        self.report_error = Some(error.to_owned());
        self
    }

    pub(crate) fn on_report(mut self, effect: impl Fn(Verdict) + Send + Sync + 'static) -> Self {
        self.on_report = Some(Box::new(effect));
        self
    }

    pub(crate) fn last_ping_timeout(&self) -> Option<Duration> {
        *self
            .last_ping_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn with_booted(mut self, booted: Result<Option<String>, String>) -> Self {
        self.booted = Some(booted);
        self
    }

    pub(crate) fn pings(&self) -> usize {
        self.pings.load(Ordering::SeqCst)
    }

    /// Failed ones included.
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
            .ping_failures_left
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
            clock.advance(self.failing_ping_delay);
        }
        Err(if self.failing_ping_delay.is_zero() {
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

#[derive(Debug)]
pub(crate) struct FakeHost {
    answer: Mutex<Result<BootedImage, String>>,
    calls: AtomicUsize,
}

impl FakeHost {
    pub(crate) fn new(answer: Result<BootedImage, String>) -> Self {
        Self {
            answer: Mutex::new(answer),
            calls: AtomicUsize::new(0),
        }
    }

    pub(crate) fn booted(digest: &Digest) -> Self {
        Self::new(Ok(booted_image(digest, None)))
    }

    /// A rollback to `first` is queued when it is not the booted one.
    pub(crate) fn booted_with_first(digest: &Digest, first: &Digest) -> Self {
        Self::new(Ok(booted_image(digest, Some(first))))
    }

    pub(crate) fn set(&self, answer: Result<BootedImage, String>) {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner) = answer;
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

pub(crate) fn booted_image(booted: &Digest, first: Option<&Digest>) -> BootedImage {
    let queued = first.filter(|f| *f != booted);
    BootedImage {
        digest: Some(booted.as_str().to_owned()),
        image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
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

#[derive(Debug, Default)]
pub(crate) struct FakeStore {
    error: Option<String>,
    records: Mutex<Vec<HealthFailure>>,
}

impl FakeStore {
    pub(crate) fn failing(error: &str) -> Self {
        Self {
            error: Some(error.to_owned()),
            ..Self::default()
        }
    }

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

#[derive(Debug, Default)]
pub(crate) struct FakeRebooter {
    error: Option<String>,
    reboots: AtomicUsize,
}

impl FakeRebooter {
    pub(crate) fn failing(error: &str) -> Self {
        Self {
            error: Some(error.to_owned()),
            ..Self::default()
        }
    }

    /// Failed ones included.
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
