use std::path::{Path, PathBuf};

use crate::error::BootcError;
use crate::status::{BootEntry, FilesystemOverlay, Host, HostType, ImageStatus};
use crate::time::parse_rfc3339_unix;

pub const SYSROOT: &str = "/sysroot";

impl Host {
    pub fn booted(&self) -> Option<&BootEntry> {
        self.status.booted.as_ref()
    }

    pub fn staged(&self) -> Option<&BootEntry> {
        self.status.staged.as_ref()
    }

    pub fn rollback(&self) -> Option<&BootEntry> {
        self.status.rollback.as_ref()
    }

    pub fn booted_digest(&self) -> Option<&str> {
        self.booted().and_then(BootEntry::digest)
    }

    pub fn staged_digest(&self) -> Option<&str> {
        self.staged().and_then(BootEntry::digest)
    }

    pub fn rollback_digest(&self) -> Option<&str> {
        self.rollback().and_then(BootEntry::digest)
    }

    /// `Some(false)` when the stage will be finalized at shutdown; `None` when nothing is staged.
    pub fn staged_download_only(&self) -> Option<bool> {
        self.staged().map(BootEntry::is_download_only)
    }

    pub fn rollback_queued(&self) -> bool {
        self.status.rollback_queued
    }

    /// The condition on which bootc refuses `bootc upgrade` (bootc `crates/lib/src/cli.rs`).
    pub fn incompatible(&self) -> bool {
        self.booted().is_some_and(|b| b.incompatible)
            || self.staged().is_some_and(|s| s.incompatible)
    }

    pub fn usr_overlay(&self) -> Option<&FilesystemOverlay> {
        self.status.usr_overlay.as_ref()
    }

    /// Often stale, never the remote image: bootc writes it on the commit last pulled, only when
    /// a check finds an update, and never clears it (ostree-ext `cache_pending`).
    pub fn cached_update(&self) -> Option<&ImageStatus> {
        self.staged()
            .and_then(|s| s.cached_update.as_ref())
            .or_else(|| self.booted().and_then(|b| b.cached_update.as_ref()))
    }

    /// The reliable use of `cachedUpdate`: metadata of a digest `bootc upgrade --check` printed.
    pub fn cached_update_for(&self, digest: &str) -> Option<&ImageStatus> {
        self.staged()
            .into_iter()
            .chain(self.booted())
            .chain(self.rollback())
            .chain(&self.status.other_deployments)
            .filter_map(|entry| entry.cached_update.as_ref())
            .find(|cached| cached.image_digest == digest)
    }

    pub fn is_bootc_host(&self) -> bool {
        self.booted().is_some_and(|b| b.image.is_some())
            && !matches!(self.status.host_type, Some(HostType::Unknown))
    }

    pub fn require_bootc_host(&self) -> Result<(&BootEntry, &ImageStatus), BootcError> {
        let booted = self.booted().ok_or_else(|| BootcError::NotBootcHost {
            detail: "bootc status reports no booted deployment".to_owned(),
        })?;
        let image = booted
            .image
            .as_ref()
            .ok_or_else(|| BootcError::NotBootcHost {
                detail: "the booted deployment was not created from a container image".to_owned(),
            })?;
        Ok((booted, image))
    }
}

impl BootEntry {
    pub fn digest(&self) -> Option<&str> {
        self.image.as_ref().map(|i| i.image_digest.as_str())
    }

    pub fn version(&self) -> Option<&str> {
        self.image.as_ref().and_then(|i| i.version.as_deref())
    }

    pub fn image_reference(&self) -> Option<&str> {
        self.image.as_ref().map(|i| i.image.image.as_str())
    }

    pub fn is_download_only(&self) -> bool {
        self.download_only
    }

    pub fn location(&self) -> Result<DeploymentLocation, LocationError> {
        let ostree = self.ostree.as_ref().ok_or(LocationError::NotOstree)?;
        DeploymentLocation::new(&ostree.stateroot, &ostree.checksum, ostree.deploy_serial)
    }

    pub fn deployment_root(&self) -> Result<PathBuf, LocationError> {
        Ok(self.location()?.root())
    }
}

impl ImageStatus {
    pub fn timestamp_unix(&self) -> Option<i64> {
        self.timestamp.as_deref().and_then(parse_rfc3339_unix)
    }
}

/// Validated although bootc produced it: its parts become path components of a directory
/// that a root daemon reads from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeploymentLocation {
    stateroot: String,
    checksum: String,
    serial: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocationError {
    #[error("deployment has no ostree location")]
    NotOstree,
    #[error("invalid ostree stateroot {0:?}")]
    InvalidStateroot(String),
    #[error("invalid ostree commit checksum {0:?}")]
    InvalidChecksum(String),
}

impl DeploymentLocation {
    pub fn new(stateroot: &str, checksum: &str, serial: u32) -> Result<Self, LocationError> {
        let stateroot_ok = !stateroot.is_empty()
            && stateroot != "."
            && stateroot != ".."
            && !stateroot.contains(['/', '\0']);
        if !stateroot_ok {
            return Err(LocationError::InvalidStateroot(stateroot.to_owned()));
        }
        let checksum_ok = checksum.len() == 64
            && checksum
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !checksum_ok {
            return Err(LocationError::InvalidChecksum(checksum.to_owned()));
        }
        Ok(Self {
            stateroot: stateroot.to_owned(),
            checksum: checksum.to_owned(),
            serial,
        })
    }

    pub fn stateroot(&self) -> &str {
        &self.stateroot
    }

    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    pub fn serial(&self) -> u32 {
        self.serial
    }

    pub fn root(&self) -> PathBuf {
        self.root_under(Path::new(SYSROOT))
    }

    pub fn root_under(&self, sysroot: &Path) -> PathBuf {
        sysroot
            .join("ostree/deploy")
            .join(&self.stateroot)
            .join("deploy")
            .join(format!("{}.{}", self.checksum, self.serial))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::BootEntryOstree;

    const CSUM: &str = "26836632adf6228d64ef07a26fd3efaf177104efd1f341a2cf7909a3e4e2c72c";

    fn entry(stateroot: &str, checksum: &str, serial: u32) -> BootEntry {
        BootEntry {
            ostree: Some(BootEntryOstree {
                stateroot: stateroot.into(),
                checksum: checksum.into(),
                deploy_serial: serial,
            }),
            ..BootEntry::default()
        }
    }

    #[test]
    fn deployment_root_path() {
        let e = entry("default", CSUM, 1);
        assert_eq!(
            e.deployment_root().unwrap(),
            PathBuf::from(format!("/sysroot/ostree/deploy/default/deploy/{CSUM}.1"))
        );
        let loc = e.location().unwrap();
        assert_eq!(
            (loc.stateroot(), loc.checksum(), loc.serial()),
            ("default", CSUM, 1)
        );
        assert_eq!(
            loc.root_under(Path::new("/tmp/x")),
            PathBuf::from(format!("/tmp/x/ostree/deploy/default/deploy/{CSUM}.1"))
        );
    }

    #[test]
    fn location_is_validated() {
        assert_eq!(
            BootEntry::default().location(),
            Err(LocationError::NotOstree)
        );
        for bad in ["", ".", "..", "a/b", "../etc", "x\0y"] {
            assert!(matches!(
                entry(bad, CSUM, 0).location(),
                Err(LocationError::InvalidStateroot(_))
            ));
        }
        for bad in ["", "abc", &CSUM.to_uppercase(), &format!("{CSUM}/..")] {
            assert!(matches!(
                entry("default", bad, 0).location(),
                Err(LocationError::InvalidChecksum(_))
            ));
        }
    }

    fn image(digest: &str, version: &str) -> ImageStatus {
        ImageStatus {
            image: crate::status::ImageReference {
                image: "ghcr.io/sekirokenjii/apex:45".into(),
                transport: "registry".into(),
                signature: None,
            },
            version: Some(version.into()),
            timestamp: None,
            image_digest: digest.into(),
            architecture: "amd64".into(),
        }
    }

    fn deployed(digest: &str, cached: Option<ImageStatus>) -> BootEntry {
        BootEntry {
            image: Some(image(digest, "45.1.1")),
            cached_update: cached,
            ..BootEntry::default()
        }
    }

    const V1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const V2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const V3: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    /// Checks after a download write onto the staged commit.
    #[test]
    fn cached_update_prefers_the_staged_entry() {
        let mut h = Host::default();
        h.status.booted = Some(deployed(V1, Some(image(V2, "45.1.2"))));
        h.status.staged = Some(deployed(V2, Some(image(V3, "45.1.3"))));
        assert_eq!(h.cached_update().map(|c| c.image_digest.as_str()), Some(V3));
        assert_eq!(
            h.cached_update_for(V3).and_then(|c| c.version.as_deref()),
            Some("45.1.3")
        );
        assert_eq!(
            h.cached_update_for(V2).and_then(|c| c.version.as_deref()),
            Some("45.1.2")
        );
    }

    /// A reboot dropped the locked V2 stage; the booted entry still says V2 for good.
    #[test]
    fn booted_cached_update_goes_stale_after_a_dropped_stage() {
        let mut h = Host::default();
        h.status.booted = Some(deployed(V1, Some(image(V2, "45.1.2"))));
        assert_eq!(
            h.cached_update().map(|c| c.image_digest.as_str()),
            Some(V2),
            "fallback to the booted entry, which may be stale"
        );
        assert_eq!(h.cached_update_for(V3), None);
    }

    #[test]
    fn cached_update_for_searches_every_deployment() {
        let mut h = Host::default();
        h.status.booted = Some(deployed(V1, None));
        h.status.rollback = Some(deployed(V2, None));
        h.status.other_deployments = vec![deployed(V2, Some(image(V3, "45.1.3")))];
        assert_eq!(h.cached_update(), None);
        assert_eq!(
            h.cached_update_for(V3).map(|c| c.image_digest.as_str()),
            Some(V3)
        );
        assert_eq!(h.cached_update_for(V1), None);
    }

    #[test]
    fn empty_host_is_not_a_bootc_host() {
        let h = Host::default();
        assert!(!h.is_bootc_host());
        assert!(matches!(
            h.require_bootc_host(),
            Err(BootcError::NotBootcHost { .. })
        ));
        assert_eq!(h.booted_digest(), None);
        assert_eq!(h.staged_download_only(), None);
        assert!(!h.incompatible());
        assert!(!h.rollback_queued());
    }
}
