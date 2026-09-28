//! Questions the update engine asks of a parsed [`Host`].

use std::path::{Path, PathBuf};

use crate::error::BootcError;
use crate::status::{BootEntry, FilesystemOverlay, Host, HostType, ImageStatus};
use crate::time::parse_rfc3339_unix;

/// Mount point of the physical root on an ostree system.
pub const SYSROOT: &str = "/sysroot";

impl Host {
    /// The booted deployment, if bootc manages this host.
    pub fn booted(&self) -> Option<&BootEntry> {
        self.status.booted.as_ref()
    }

    /// The staged deployment, if any.
    pub fn staged(&self) -> Option<&BootEntry> {
        self.status.staged.as_ref()
    }

    /// The rollback deployment, if any.
    pub fn rollback(&self) -> Option<&BootEntry> {
        self.status.rollback.as_ref()
    }

    /// Manifest digest of the booted image.
    pub fn booted_digest(&self) -> Option<&str> {
        self.booted().and_then(BootEntry::digest)
    }

    /// Manifest digest of the staged image.
    pub fn staged_digest(&self) -> Option<&str> {
        self.staged().and_then(BootEntry::digest)
    }

    /// Manifest digest of the rollback image.
    pub fn rollback_digest(&self) -> Option<&str> {
        self.rollback().and_then(BootEntry::digest)
    }

    /// `Some(true)` when a staged deployment exists and is locked (`--download-only`),
    /// `Some(false)` when it will be finalized at shutdown, `None` when nothing is staged.
    pub fn staged_download_only(&self) -> Option<bool> {
        self.staged().map(BootEntry::is_download_only)
    }

    /// The rollback deployment is queued for the next boot.
    pub fn rollback_queued(&self) -> bool {
        self.status.rollback_queued
    }

    /// The booted or staged deployment has origin changes bootc does not understand.
    ///
    /// Same condition bootc checks before refusing `bootc upgrade`
    /// (bootc `crates/lib/src/cli.rs`, `upgrade`: booted || staged `incompatible`).
    pub fn incompatible(&self) -> bool {
        self.booted().is_some_and(|b| b.incompatible)
            || self.staged().is_some_and(|s| s.incompatible)
    }

    /// The overlay mounted on `/usr`, if any.
    pub fn usr_overlay(&self) -> Option<&FilesystemOverlay> {
        self.status.usr_overlay.as_ref()
    }

    /// The `cachedUpdate` of the staged entry, else of the booted entry.
    ///
    /// **Not the remote image.** `bootc upgrade --check` stores what it found on the
    /// commit that the image reference's ostree ref points at, which is the image last
    /// pulled for that reference, and only when it finds an update; bootc's check,
    /// download and status paths never clear it (ostree-ext
    /// `ImageImporter::prepare_internal` and `cache_pending`). `bootc
    /// status` then reports it on whichever deployment has that commit (bootc
    /// `status.rs`, `imagestatus`). So:
    ///
    /// - after a download, later checks write onto the staged commit, which is why the
    ///   staged entry is preferred;
    /// - once a download-only stage has been dropped by a reboot, checks write onto a
    ///   commit no deployment uses, and the booted entry keeps the value from before the
    ///   first download for good;
    /// - an update found and then withdrawn from the channel stays here.
    ///
    /// The digest printed by `bootc upgrade --check` ([`crate::CheckOutcome`]) is the
    /// primary source. Use [`Self::cached_update_for`] to get the version and build time
    /// of a digest learned there.
    pub fn cached_update(&self) -> Option<&ImageStatus> {
        self.staged()
            .and_then(|s| s.cached_update.as_ref())
            .or_else(|| self.booted().and_then(|b| b.cached_update.as_ref()))
    }

    /// The `cachedUpdate` of any deployment (staged, booted, rollback, then the others)
    /// that describes the image with manifest digest `digest`.
    ///
    /// This is the reliable use of `cachedUpdate`: the digest is known from elsewhere
    /// (the output of `bootc upgrade --check`), and bootc's cached copy of that image's
    /// config adds its version and build time.
    pub fn cached_update_for(&self, digest: &str) -> Option<&ImageStatus> {
        self.staged()
            .into_iter()
            .chain(self.booted())
            .chain(self.rollback())
            .chain(&self.status.other_deployments)
            .filter_map(|entry| entry.cached_update.as_ref())
            .find(|cached| cached.image_digest == digest)
    }

    /// The host is booted from a container image that bootc manages.
    pub fn is_bootc_host(&self) -> bool {
        self.booted().is_some_and(|b| b.image.is_some())
            && !matches!(self.status.host_type, Some(HostType::Unknown))
    }

    /// The booted entry and its image, or [`BootcError::NotBootcHost`].
    ///
    /// bootc reports a host it does not manage with exit status 0 and an empty status
    /// (bootc fixture `spec-v1-null.json`), or with a booted ostree commit that has no
    /// image (`spec-rfe-ostree-deployment.yaml`).
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
    /// Manifest digest of this deployment's image.
    pub fn digest(&self) -> Option<&str> {
        self.image.as_ref().map(|i| i.image_digest.as_str())
    }

    /// `org.opencontainers.image.version` of this deployment's image.
    pub fn version(&self) -> Option<&str> {
        self.image.as_ref().and_then(|i| i.version.as_deref())
    }

    /// The image reference this deployment follows, e.g. `ghcr.io/sekirokenjii/apex:45`.
    pub fn image_reference(&self) -> Option<&str> {
        self.image.as_ref().map(|i| i.image.image.as_str())
    }

    /// The deployment is staged but locked (`bootc upgrade --download-only`).
    pub fn is_download_only(&self) -> bool {
        self.download_only
    }

    /// Where the deployment lives on disk.
    pub fn location(&self) -> Result<DeploymentLocation, LocationError> {
        let ostree = self.ostree.as_ref().ok_or(LocationError::NotOstree)?;
        DeploymentLocation::new(&ostree.stateroot, &ostree.checksum, ostree.deploy_serial)
    }

    /// The deployment's root directory under [`SYSROOT`],
    /// `/sysroot/ostree/deploy/<stateroot>/deploy/<checksum>.<serial>`.
    pub fn deployment_root(&self) -> Result<PathBuf, LocationError> {
        Ok(self.location()?.root())
    }
}

impl ImageStatus {
    /// The build timestamp in seconds since the Unix epoch, if present and valid.
    pub fn timestamp_unix(&self) -> Option<i64> {
        self.timestamp.as_deref().and_then(parse_rfc3339_unix)
    }
}

/// Validated on-disk identity of an ostree deployment.
///
/// Both parts become path components of a directory a root daemon reads from, so they
/// are validated even though bootc produced them: the stateroot must be a single
/// ordinary path component and the checksum a 64-digit lowercase hex sha256.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeploymentLocation {
    stateroot: String,
    checksum: String,
    serial: u32,
}

/// Why a [`BootEntry`] has no usable [`DeploymentLocation`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocationError {
    /// The entry has no `ostree` section (not the ostree backend).
    #[error("deployment has no ostree location")]
    NotOstree,
    /// The stateroot is empty, `.`/`..`, or contains `/` or NUL.
    #[error("invalid ostree stateroot {0:?}")]
    InvalidStateroot(String),
    /// The checksum is not 64 lowercase hex digits.
    #[error("invalid ostree commit checksum {0:?}")]
    InvalidChecksum(String),
}

impl DeploymentLocation {
    /// Validates the parts of a deployment location.
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

    /// The stateroot (usually `default`).
    pub fn stateroot(&self) -> &str {
        &self.stateroot
    }

    /// The ostree commit checksum.
    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    /// The deployment serial.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// The deployment root under the real [`SYSROOT`].
    pub fn root(&self) -> PathBuf {
        self.root_under(Path::new(SYSROOT))
    }

    /// The deployment root under another physical root, e.g. a test directory:
    /// `<sysroot>/ostree/deploy/<stateroot>/deploy/<checksum>.<serial>`.
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

    /// V1 booted; a check found V2 (cached on V1's commit), V2 was downloaded, and a
    /// later check found V3, which ostree-ext cached on V2's commit, the staged one.
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

    /// The same host after the locked V2 stage was dropped by a reboot: the booted entry
    /// still says V2, although the channel has moved on. Only a digest-exact lookup is
    /// safe.
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
