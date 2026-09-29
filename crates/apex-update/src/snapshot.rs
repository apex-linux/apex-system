use crate::records::{Denylist, Intent};
use crate::version::Version;

pub type Bytes = u64;
pub type UnixSeconds = u64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Power {
    pub on_ac: bool,
    pub battery_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeploymentView {
    pub digest: String,
    pub version: Option<Version>,
    /// Staged but locked: it will not boot. Meaningful only for the staged deployment.
    pub download_only: bool,
    pub pinned: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent flags reported by bootc and ostree, any combination can occur"
)]
pub struct HostView {
    pub booted: Option<DeploymentView>,
    pub staged: Option<DeploymentView>,
    pub rollback: Option<DeploymentView>,
    pub rollback_queued: bool,
    pub incompatible: bool,
    pub usr_overlay: bool,
    pub finalize_failed: bool,
}

impl HostView {
    pub fn booted_digest(&self) -> Option<&str> {
        self.booted.as_ref().map(|d| d.digest.as_str())
    }

    pub fn staged_digest(&self) -> Option<&str> {
        self.staged.as_ref().map(|d| d.digest.as_str())
    }

    pub fn booted_version(&self) -> Option<&Version> {
        self.booted.as_ref().and_then(|d| d.version.as_ref())
    }

    pub fn has_unlocked_stage(&self) -> bool {
        self.staged.as_ref().is_some_and(|d| !d.download_only)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteView {
    pub digest: String,
    pub version: Option<Version>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemSnapshot {
    pub power: Power,
    /// `NetworkManager` `YES`/`GUESS_YES` is metered, `NO`/`GUESS_NO` not. Unknown is not, as in
    /// `GLib`'s `GNetworkMonitor`: otherwise updates would stop on every unclassified network.
    pub metered: Option<bool>,
    pub sysroot_free: Bytes,
    pub boot_free: Bytes,
    /// Uncompressed.
    pub image_size: Option<Bytes>,
    pub boot_payload: Option<Bytes>,
    pub host: HostView,
    pub remote: Option<RemoteView>,
    pub intent: Option<Intent>,
    pub denylist: Denylist,
    /// Failed its health checks during a user session and not yet acknowledged.
    pub health_failure: Option<String>,
    pub now: UnixSeconds,
}
