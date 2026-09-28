//! Snapshot builders shared by the unit tests.

use crate::gates::GIB;
use crate::records::{DenyEntry, Intent};
use crate::snapshot::{DeploymentView, HostView, Power, RemoteView, SystemSnapshot};
use crate::version::Version;

pub(crate) const BOOTED: &str = "sha256:booted";
pub(crate) const STAGED: &str = "sha256:staged";
pub(crate) const REMOTE: &str = "sha256:remote";

pub(crate) fn ver(s: &str) -> Version {
    Version::parse(s).expect("valid version")
}

pub(crate) fn deployment(digest: &str, version: &str, download_only: bool) -> DeploymentView {
    DeploymentView {
        digest: digest.into(),
        version: Version::parse(version),
        download_only,
        pinned: false,
    }
}

/// Booted 45.20260920.1, remote 45.20260924.1 found, on AC, unmetered, plenty of space,
/// nothing staged: every gate passes.
pub(crate) fn healthy() -> SystemSnapshot {
    SystemSnapshot {
        power: Power {
            on_ac: true,
            battery_percent: Some(80),
        },
        metered: Some(false),
        sysroot_free: 100 * GIB,
        boot_free: GIB,
        image_size: Some(4 * GIB),
        boot_payload: Some(120 * 1024 * 1024),
        host: HostView {
            booted: Some(deployment(BOOTED, "45.20260920.1", false)),
            rollback: Some(deployment("sha256:older", "45.20260910.1", false)),
            ..HostView::default()
        },
        remote: Some(RemoteView {
            digest: REMOTE.into(),
            version: Version::parse("45.20260924.1"),
        }),
        now: 1_790_000_000,
        ..SystemSnapshot::default()
    }
}

/// [`healthy`] with the remote downloaded and locked.
pub(crate) fn with_locked_stage() -> SystemSnapshot {
    let mut s = healthy();
    s.host.staged = Some(deployment(REMOTE, "45.20260924.1", true));
    s
}

pub(crate) fn intent(digest: &str) -> Intent {
    Intent {
        digest: digest.into(),
        created: 1_790_000_000,
    }
}

pub(crate) fn deny(digest: &str) -> DenyEntry {
    DenyEntry {
        digest: digest.into(),
        reason: "required checks failed".into(),
        at: 1_789_000_000,
    }
}
