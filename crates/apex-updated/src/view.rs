//! Pure mappings from bootc's model (`apex_bootc::Host`) and the image manifests to the
//! engine's input (`apex_update::HostView`), to the D-Bus deployment dictionaries, and to
//! the remote image found by a check.

use apex_bootc::{BootEntry, CheckOutcome, Host, ImageStatus};
use apex_dbus::Deployment;
use apex_update::{DeploymentView, HostView, LastCheck, Manifest, Version};

/// The manifests read from the deployments, where available.
#[derive(Debug, Clone, Copy, Default)]
pub struct Manifests<'a> {
    /// The running image's manifest.
    pub booted: Option<&'a Manifest>,
    /// The staged deployment's manifest.
    pub staged: Option<&'a Manifest>,
    /// The rollback deployment's manifest.
    pub rollback: Option<&'a Manifest>,
}

/// One deployment as the engine sees it; `None` for an entry without a container image.
///
/// The version comes from the deployment's own manifest (verified content) and falls back
/// to the image's version label.
pub fn deployment_view(entry: &BootEntry, manifest: Option<&Manifest>) -> Option<DeploymentView> {
    let digest = entry.digest()?;
    let version = manifest
        .and_then(Manifest::parsed_version)
        .or_else(|| entry.version().and_then(Version::parse));
    Some(DeploymentView {
        digest: digest.to_owned(),
        version,
        download_only: entry.is_download_only(),
        pinned: entry.pinned,
    })
}

/// The host as the engine sees it.
pub fn host_view(host: &Host, manifests: &Manifests<'_>, finalize_failed: bool) -> HostView {
    HostView {
        booted: host
            .booted()
            .and_then(|e| deployment_view(e, manifests.booted)),
        staged: host
            .staged()
            .and_then(|e| deployment_view(e, manifests.staged)),
        rollback: host
            .rollback()
            .and_then(|e| deployment_view(e, manifests.rollback)),
        rollback_queued: host.rollback_queued(),
        incompatible: host.incompatible(),
        usr_overlay: host.usr_overlay().is_some(),
        finalize_failed,
    }
}

/// A deployment dictionary for the `Booted`, `Staged` and `Rollback` properties; the empty
/// dictionary when there is no such deployment. `download-only` is set for the staged
/// deployment only, as the contract specifies.
pub fn deployment_dict(
    entry: Option<&BootEntry>,
    manifest: Option<&Manifest>,
    staged: bool,
) -> Deployment {
    let Some(entry) = entry else {
        return Deployment::default();
    };
    Deployment {
        image: entry.image_reference().map(str::to_owned),
        digest: entry.digest().map(str::to_owned),
        version: entry.version().map(str::to_owned).or_else(|| {
            manifest
                .map(|m| m.version.clone())
                .filter(|v| !v.is_empty())
        }),
        timestamp: entry.image.as_ref().and_then(image_timestamp),
        pinned: Some(entry.pinned),
        download_only: staged.then_some(entry.download_only),
        degraded: manifest.map(|m| m.degraded.clone()),
    }
}

/// An image's build time in Unix seconds, if it has a valid one.
pub fn image_timestamp(image: &ImageStatus) -> Option<u64> {
    image.timestamp_unix().and_then(|t| u64::try_from(t).ok())
}

/// The image the channel tag points at, as learned by a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteImage {
    /// Manifest digest.
    pub digest: String,
    /// Version label, as found.
    pub version: Option<String>,
    /// Build time, when bootc reported one.
    pub timestamp: Option<u64>,
}

/// What `bootc status` knows about the image with manifest digest `digest`: the image of a
/// deployment that has it (staged, booted, rollback), else a `cachedUpdate` that describes
/// it ([`Host::cached_update_for`]).
fn known_image<'h>(host: &'h Host, digest: &str) -> Option<&'h ImageStatus> {
    [host.staged(), host.booted(), host.rollback()]
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.image.as_ref())
        .find(|image| image.image_digest == digest)
        .or_else(|| host.cached_update_for(digest))
}

/// What the channel tag points at after `bootc upgrade --check`.
///
/// bootc compares the tag with the image **last pulled** for the channel reference, not
/// with the booted or staged deployment, and prints a digest only when they differ
/// (`apex_bootc::CheckOutcome`). `cachedUpdate` is never the remote itself: bootc stores it
/// on whichever commit the reference pointed at and never clears it, so after a
/// download-only stage was dropped at a reboot the booted entry keeps describing an image
/// the tag left long ago. So:
///
/// - `Update available` with a digest: that digest; version from bootc's output, else
///   (like the build time) from what the status knows about that exact digest: a
///   deployment with it, or a `cachedUpdate` that describes it
///   ([`Host::cached_update_for`]);
/// - `No changes`: the image last pulled, which is what `last` records. Every successful
///   check records the tag's image, and every pull (download, re-lock, channel switch)
///   records the image it pulled (`Engine::follow_tag`). Without a recorded digest the
///   remote is unknown;
/// - anything else (no digest printed, or output this build does not recognise): unknown.
///
/// Known limit: a check that found an image and was followed by no pull records an image
/// that was never pulled. If the tag then moves back to the image last pulled, bootc says
/// `No changes` and the record still names the image it left, until the next pull corrects
/// it. `last-check.json` has no field for the pulled image (M1 contract §4).
pub fn remote_after_check(
    outcome: &CheckOutcome,
    host: &Host,
    last: &LastCheck,
) -> Option<RemoteImage> {
    let (digest, version) = match outcome {
        CheckOutcome::Available {
            digest: Some(digest),
            version,
        } => (digest.as_str(), version.clone()),
        CheckOutcome::UpToDate => (last.remote_digest.as_deref()?, last.remote_version.clone()),
        CheckOutcome::Available { digest: None, .. } | CheckOutcome::Unknown => return None,
    };
    let known = known_image(host, digest);
    Some(RemoteImage {
        digest: digest.to_owned(),
        version: version.or_else(|| known.and_then(|image| image.version.clone())),
        timestamp: known.and_then(image_timestamp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_bootc::{BootEntryOstree, Digest, HostStatus, ImageReference};

    const BOOTED: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const STAGED: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const REMOTE: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    fn image(digest: &str, version: &str) -> ImageStatus {
        ImageStatus {
            image: ImageReference {
                image: "ghcr.io/sekirokenjii/apex:45".into(),
                transport: "registry".into(),
                signature: None,
            },
            version: Some(version.into()),
            timestamp: Some("2026-09-24T00:00:00Z".into()),
            image_digest: digest.into(),
            architecture: "amd64".into(),
        }
    }

    fn entry(digest: &str, version: &str) -> BootEntry {
        BootEntry {
            image: Some(image(digest, version)),
            ostree: Some(BootEntryOstree {
                stateroot: "default".into(),
                checksum: "a".repeat(64),
                deploy_serial: 0,
            }),
            ..BootEntry::default()
        }
    }

    fn host(cached: Option<ImageStatus>, staged: bool) -> Host {
        let mut booted = entry(BOOTED, "45.20260920.1");
        booted.cached_update = cached;
        Host {
            status: HostStatus {
                booted: Some(booted),
                staged: staged.then(|| BootEntry {
                    download_only: true,
                    ..entry(STAGED, "45.20260922.1")
                }),
                ..HostStatus::default()
            },
            ..Host::default()
        }
    }

    fn manifest(version: &str) -> Manifest {
        Manifest::from_json(
            format!(r#"{{"schema":1,"version":"{version}","degraded":["fingerprint"]}}"#)
                .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn manifest_version_wins_over_the_label() {
        let e = entry(BOOTED, "45.20260920.1");
        let m = manifest("45.20260921.9");
        let view = deployment_view(&e, Some(&m)).unwrap();
        assert_eq!(view.version, Version::parse("45.20260921.9"));
        let view = deployment_view(&e, None).unwrap();
        assert_eq!(view.version, Version::parse("45.20260920.1"));
        assert_eq!(deployment_view(&BootEntry::default(), None), None);
    }

    #[test]
    fn host_view_maps_flags() {
        let mut h = host(None, true);
        h.status.rollback_queued = true;
        let view = host_view(&h, &Manifests::default(), true);
        assert_eq!(view.booted_digest(), Some(BOOTED));
        assert_eq!(view.staged_digest(), Some(STAGED));
        assert!(view.staged.as_ref().unwrap().download_only);
        assert!(view.rollback_queued && view.finalize_failed);
        assert!(!view.incompatible && !view.usr_overlay);
    }

    #[test]
    fn dictionaries() {
        let h = host(None, true);
        let m = manifest("45.20260922.1");
        let staged = deployment_dict(h.staged(), Some(&m), true);
        assert_eq!(staged.digest.as_deref(), Some(STAGED));
        assert_eq!(staged.download_only, Some(true));
        assert_eq!(staged.degraded, Some(vec!["fingerprint".to_owned()]));
        assert_eq!(staged.timestamp, Some(1_790_208_000));
        let booted = deployment_dict(h.booted(), None, false);
        assert_eq!(booted.download_only, None);
        assert_eq!(booted.degraded, None);
        assert!(deployment_dict(None, None, false).is_empty());
    }

    fn record(digest: Option<&str>, version: Option<&str>) -> LastCheck {
        LastCheck {
            at: 1,
            remote_digest: digest.map(str::to_owned),
            remote_version: version.map(str::to_owned),
        }
    }

    #[test]
    fn remote_from_available_output() {
        let digest = Digest::parse(REMOTE).unwrap();
        let outcome = CheckOutcome::Available {
            version: None,
            digest: Some(digest),
        };
        let none = LastCheck::default();
        // cachedUpdate for exactly that digest adds the version and the build time.
        let with_cache = host(Some(image(REMOTE, "45.20260924.3")), false);
        assert_eq!(
            remote_after_check(&outcome, &with_cache, &none),
            Some(RemoteImage {
                digest: REMOTE.into(),
                version: Some("45.20260924.3".into()),
                timestamp: Some(1_790_208_000),
            })
        );
        // A cachedUpdate for another image says nothing about this one.
        let other_cache = host(Some(image(STAGED, "45.20260922.1")), false);
        assert_eq!(
            remote_after_check(&outcome, &other_cache, &none),
            Some(RemoteImage {
                digest: REMOTE.into(),
                version: None,
                timestamp: None,
            })
        );
        // The tag moved back to a deployed image: its own image status describes it.
        let back = CheckOutcome::Available {
            version: Some("45.20260922.1".into()),
            digest: Some(Digest::parse(STAGED).unwrap()),
        };
        let staged = host(None, true);
        assert_eq!(
            remote_after_check(&back, &staged, &none),
            Some(RemoteImage {
                digest: STAGED.into(),
                version: Some("45.20260922.1".into()),
                timestamp: Some(1_790_208_000),
            })
        );
    }

    /// "No changes" means the tag still points at the image pulled last, which the check
    /// record names; the booted entry's `cachedUpdate` is stale and never used for it.
    #[test]
    fn remote_when_bootc_says_no_changes_is_the_recorded_image() {
        let outcome = CheckOutcome::UpToDate;
        // A download of REMOTE that a reboot dropped: the booted entry still carries the
        // cachedUpdate of an earlier check (STAGED), the record names what was pulled.
        let h = host(Some(image(STAGED, "45.20260922.1")), false);
        let pulled = record(Some(REMOTE), Some("45.20260924.3"));
        assert_eq!(
            remote_after_check(&outcome, &h, &pulled),
            Some(RemoteImage {
                digest: REMOTE.into(),
                version: Some("45.20260924.3".into()),
                timestamp: None,
            })
        );
        // The recorded image is deployed: its build time comes from the deployment.
        let h = host(None, true);
        let staged = record(Some(STAGED), None);
        assert_eq!(
            remote_after_check(&outcome, &h, &staged),
            Some(RemoteImage {
                digest: STAGED.into(),
                version: Some("45.20260922.1".into()),
                timestamp: Some(1_790_208_000),
            })
        );
        // No record: unknown, whatever cachedUpdate says.
        let h = host(Some(image(REMOTE, "45.20260924.3")), true);
        assert_eq!(
            remote_after_check(&outcome, &h, &LastCheck::default()),
            None
        );
        assert_eq!(
            remote_after_check(&outcome, &h, &record(None, Some("45.20260924.3"))),
            None
        );
    }

    #[test]
    fn remote_is_unknown_without_a_printed_digest() {
        let h = host(Some(image(REMOTE, "45.20260924.3")), true);
        let pulled = record(Some(REMOTE), Some("45.20260924.3"));
        assert_eq!(
            remote_after_check(&CheckOutcome::Unknown, &h, &pulled),
            None
        );
        let no_digest = CheckOutcome::Available {
            version: Some("45.20260924.3".into()),
            digest: None,
        };
        assert_eq!(remote_after_check(&no_digest, &h, &pulled), None);
    }
}
