//! The engine's input: everything a decision needs, gathered by the daemon.
//!
//! These types are the boundary of the pure engine. `apex-updated` fills them from
//! `bootc status --format=json --format-version=1` (via `apex-bootc`), `UPower`,
//! `NetworkManager`, `statvfs` and the files in `/var/lib/apex`. This crate never depends
//! on `apex-bootc`, so the bootc model can change without touching the decisions.

use crate::records::{Denylist, Intent};
use crate::version::Version;

/// Power supply state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Power {
    /// The charger is connected (`UPower` `OnBattery` is false). A machine without a
    /// battery is on AC.
    pub on_ac: bool,
    /// Battery charge in percent, if a battery reports one.
    pub battery_percent: Option<u8>,
}

/// One deployment as the engine sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeploymentView {
    /// Manifest digest of the image, `sha256:…`.
    pub digest: String,
    /// Image version, if the image carries a valid one.
    pub version: Option<Version>,
    /// The deployment is staged but locked: it will not be booted (`downloadOnly`).
    /// Only meaningful for the staged deployment.
    pub download_only: bool,
    /// The deployment is pinned and will not be garbage-collected.
    pub pinned: bool,
}

/// The host's deployments and bootc flags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent flags reported by bootc and ostree, any combination can occur"
)]
pub struct HostView {
    /// The running deployment.
    pub booted: Option<DeploymentView>,
    /// The deployment staged for the next boot.
    pub staged: Option<DeploymentView>,
    /// The previous deployment, GRUB entry 1.
    pub rollback: Option<DeploymentView>,
    /// A rollback is queued for the next boot (`rollbackQueued`).
    pub rollback_queued: bool,
    /// The host has local rpm-ostree changes bootc cannot upgrade (`incompatible`).
    pub incompatible: bool,
    /// A transient writable overlay is mounted on `/usr` (`usrOverlay`).
    pub usr_overlay: bool,
    /// The last staged deployment could not be finalized at shutdown
    /// (`/boot/ostree/finalize-failure.stamp` exists or `ostree-boot-complete` failed).
    pub finalize_failed: bool,
}

impl HostView {
    /// Digest of the booted deployment.
    pub fn booted_digest(&self) -> Option<&str> {
        self.booted.as_ref().map(|d| d.digest.as_str())
    }

    /// Digest of the staged deployment.
    pub fn staged_digest(&self) -> Option<&str> {
        self.staged.as_ref().map(|d| d.digest.as_str())
    }

    /// Version of the booted deployment.
    pub fn booted_version(&self) -> Option<&Version> {
        self.booted.as_ref().and_then(|d| d.version.as_ref())
    }

    /// Whether a staged deployment exists and is unlocked, that is set to boot next.
    pub fn has_unlocked_stage(&self) -> bool {
        self.staged.as_ref().is_some_and(|d| !d.download_only)
    }
}

/// The newest image on the followed channel, from the last check.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteView {
    /// Manifest digest the channel tag points at.
    pub digest: String,
    /// Its version label, if valid.
    pub version: Option<Version>,
}

/// Everything the engine decides on. All times are Unix seconds and all sizes bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemSnapshot {
    /// Power supply state.
    pub power: Power,
    /// Whether the primary connection is metered. The daemon maps `NetworkManager`'s
    /// `YES` and `GUESS_YES` to `Some(true)` and `NO` and `GUESS_NO` to `Some(false)`.
    /// `None` (unknown: `NetworkManager` is not running or reports `UNKNOWN`) is treated
    /// as not metered, which is also what `GLib`'s `GNetworkMonitor` reports; blocking on
    /// unknown would stop updates on every network `NetworkManager` cannot classify.
    pub metered: Option<bool>,
    /// Free bytes on `/sysroot` for an unprivileged writer (`f_bavail`).
    pub sysroot_free: u64,
    /// Free bytes on `/boot`.
    pub boot_free: u64,
    /// Uncompressed size of the image to download, if known.
    pub image_size: Option<u64>,
    /// Size of the staged kernel plus initramfs that finalization copies to `/boot`, if
    /// known.
    pub boot_payload: Option<u64>,
    /// Deployments and bootc flags.
    pub host: HostView,
    /// Newest image on the channel, if a check has found one.
    pub remote: Option<RemoteView>,
    /// The user's pending apply, from `intent.json`.
    pub intent: Option<Intent>,
    /// Digests that failed their health checks, from `denylist.json`.
    pub denylist: Denylist,
    /// Digest that failed its health checks during a user session and is not yet
    /// acknowledged, from `health-failure.json`.
    pub health_failure: Option<String>,
    /// Current time.
    pub now: u64,
}
