use std::time::{Duration, Instant};

use apex_boot::{Digest, HealthFailure, Verdict};
use apex_bootc::ImageSignature;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) class: String,
    pub(crate) state: String,
    pub(crate) seat: String,
    pub(crate) age: Option<Duration>,
}

pub(crate) trait Sessions: Send + Sync {
    /// A session whose properties cannot be read is still listed, with empty `class` and `state`.
    fn list(&self) -> Result<Vec<SessionInfo>, String>;
}

pub(crate) trait Updater: Send + Sync {
    fn ping(&self, timeout: Duration) -> Result<(), String>;

    fn report_boot(&self, verdict: Verdict, digest: Option<&Digest>) -> Result<(), String>;

    fn booted_digest(&self) -> Result<Option<String>, String>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BootedImage {
    pub(crate) digest: Option<String>,
    pub(crate) image: Option<String>,
    pub(crate) transport: Option<String>,
    pub(crate) signature: Option<ImageSignature>,
    pub(crate) rollback_digest: Option<String>,
    pub(crate) rollback_queued: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusError {
    pub(crate) message: String,
    /// A timeout or a busy lock, which a later try can get past.
    pub(crate) transient: bool,
}

pub(crate) trait HostStatus: Send + Sync {
    /// `Ok` with empty fields on a host bootc does not manage.
    fn status(&self) -> Result<BootedImage, StatusError>;

    fn booted(&self) -> Result<BootedImage, String> {
        self.status().map_err(|err| err.message)
    }
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;

    fn sleep(&self, duration: Duration);

    fn unix_now(&self) -> u64;
}

pub(crate) trait FailureStore {
    fn record(&self, failure: &HealthFailure) -> Result<(), String>;
}

pub(crate) trait Rebooter {
    /// Returns once the request has been queued.
    fn reboot(&self) -> Result<(), String>;
}
