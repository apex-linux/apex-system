use apex_bootc::{BootEntry, CheckOutcome, Host, ImageStatus};
use apex_dbus::Deployment;
use apex_update::{DeploymentView, HostView, LastCheck, Manifest, Version};

#[derive(Debug, Clone, Copy, Default)]
pub struct Manifests<'a> {
    pub booted: Option<&'a Manifest>,
    pub staged: Option<&'a Manifest>,
    pub rollback: Option<&'a Manifest>,
}

/// The version comes from the deployment's own manifest, which is verified content, before
/// the image's label.
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

pub fn image_timestamp(image: &ImageStatus) -> Option<u64> {
    image.timestamp_unix().and_then(|t| u64::try_from(t).ok())
}

/// What the channel tag points at, as learned by a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteImage {
    pub digest: String,
    pub version: Option<String>,
    pub timestamp: Option<u64>,
}

fn known_image<'h>(host: &'h Host, digest: &str) -> Option<&'h ImageStatus> {
    [host.staged(), host.booted(), host.rollback()]
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.image.as_ref())
        .find(|image| image.image_digest == digest)
        .or_else(|| host.cached_update_for(digest))
}

/// bootc prints a digest only when the tag differs from the image pulled last, so "no changes"
/// means the image `last` records. `cachedUpdate` is never cleared: only a matching digest counts.
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
        let with_cache = host(Some(image(REMOTE, "45.20260924.3")), false);
        assert_eq!(
            remote_after_check(&outcome, &with_cache, &none),
            Some(RemoteImage {
                digest: REMOTE.into(),
                version: Some("45.20260924.3".into()),
                timestamp: Some(1_790_208_000),
            })
        );
        let other_cache = host(Some(image(STAGED, "45.20260922.1")), false);
        assert_eq!(
            remote_after_check(&outcome, &other_cache, &none),
            Some(RemoteImage {
                digest: REMOTE.into(),
                version: None,
                timestamp: None,
            })
        );
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

    #[test]
    fn remote_when_bootc_says_no_changes_is_the_recorded_image() {
        let outcome = CheckOutcome::UpToDate;
        // A reboot dropped the download of REMOTE; cachedUpdate still names STAGED.
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
