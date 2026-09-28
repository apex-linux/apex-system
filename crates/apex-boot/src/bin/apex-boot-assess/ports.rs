//! The I/O boundaries of the assessment, as traits. Production implementations live in
//! [`crate::system`]; the tests use fakes.
//!
//! Every method is synchronous. The glue and the checks run on plain threads (the
//! library's check runner uses one thread per check); the production implementations
//! bridge to the tokio runtime themselves.
//!
//! Errors are strings on purpose: each one ends up as a check's failure reason or a
//! journal line, and nothing branches on its kind.

use std::time::{Duration, Instant};

use apex_boot::{Digest, HealthFailure, Verdict};
use apex_bootc::ImageSignature;

/// A logind session, reduced to what the assessment looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInfo {
    /// logind's session id, e.g. `c1` or `2`.
    pub(crate) id: String,
    /// `Class`: `greeter`, `user`, `user-early`, `lock-screen`, `background`, …
    pub(crate) class: String,
    /// `State`: `online`, `active` or `closing`.
    pub(crate) state: String,
    /// The seat, e.g. `seat0`; empty for sessions without one (SSH, cron).
    pub(crate) seat: String,
    /// Time since logind created the session (`TimestampMonotonic`); `None` when unknown.
    pub(crate) age: Option<Duration>,
}

/// systemd-logind.
pub(crate) trait Sessions: Send + Sync {
    /// Every current session (`ListSessions` plus each session's properties). A session
    /// whose properties cannot be read is still listed, with an empty `class` and `state`
    /// (see [`crate::session::user_logged_in`]).
    fn list(&self) -> Result<Vec<SessionInfo>, String>;
}

/// `apex-updated` on the system bus.
pub(crate) trait Updater: Send + Sync {
    /// One `Ping`, bounded by `timeout` (at most [`crate::ping::PING_TIMEOUT`]).
    fn ping(&self, timeout: Duration) -> Result<(), String>;

    /// `ReportBoot(verdict, digest)`, an unknown digest sent as `""`; bounded by
    /// [`apex_boot::REPORT_TIMEOUT`].
    fn report_boot(&self, verdict: Verdict, digest: Option<&Digest>) -> Result<(), String>;

    /// The `digest` key of the daemon's `Booted` property: the fallback when `bootc status`
    /// fails in this process.
    fn booted_digest(&self) -> Result<Option<String>, String>;
}

/// What `bootc status` says about the booted deployment and the boot order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BootedImage {
    /// Manifest digest, `sha256:…`.
    pub(crate) digest: Option<String>,
    /// Image reference, e.g. `ghcr.io/sekirokenjii/apex:45`.
    pub(crate) image: Option<String>,
    /// Transport, e.g. `registry`.
    pub(crate) transport: Option<String>,
    /// How the image was verified when it was pulled.
    pub(crate) signature: Option<ImageSignature>,
    /// Manifest digest of the rollback deployment, if there is one with an image.
    pub(crate) rollback_digest: Option<String>,
    /// `rollbackQueued`: the rollback deployment comes first in the boot order.
    pub(crate) rollback_queued: bool,
}

/// `bootc status` (read-only).
pub(crate) trait HostStatus: Send + Sync {
    /// The booted deployment and the boot order. `Ok` with empty fields on a host bootc
    /// does not manage. Bounded by [`apex_boot::BOOTC_STATUS_BOUND`].
    fn booted(&self) -> Result<BootedImage, String>;
}

/// Time, so that the bounded waits can be tested without waiting.
pub(crate) trait Clock: Send + Sync {
    /// A monotonic instant.
    fn now(&self) -> Instant;

    /// Blocks the calling thread.
    fn sleep(&self, duration: Duration);

    /// Seconds since the Unix epoch, for `health-failure.json`.
    fn unix_now(&self) -> u64;
}

/// Where a red armed boot is recorded (`/var/lib/apex/health-failure.json`).
pub(crate) trait FailureStore {
    /// Writes the record atomically, replacing an older one. Bounded by
    /// [`apex_boot::RECORD_TIMEOUT`].
    fn record(&self, failure: &HealthFailure) -> Result<(), String>;
}

/// `systemctl reboot`.
pub(crate) trait Rebooter {
    /// Asks systemd to reboot; returns once the request has been queued.
    fn reboot(&self) -> Result<(), String>;
}
