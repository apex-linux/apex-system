#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; clippy's allow-unwrap-in-tests does not cover them"
)]

use std::path::{Path, PathBuf};

use apex_bootc::{
    BootEntry, BootOrder, BootcError, FilesystemOverlay, FilesystemOverlayAccessMode,
    FilesystemOverlayPersistence, Host, HostType, ImageSignature, Store,
};
use serde_json::{Value, json};

fn fixture_dir(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/status")
        .join(sub)
}

fn fixtures(sub: &str) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(fixture_dir(sub))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), p))
        .collect();
    out.sort();
    out
}

fn load(sub: &str, stem: &str) -> Host {
    let path = fixture_dir(sub).join(format!("{stem}.json"));
    Host::from_json(std::fs::read(&path).unwrap()).unwrap_or_else(|e| panic!("{stem}: {e}"))
}

fn entry(e: Option<&BootEntry>) -> Value {
    let Some(e) = e else { return Value::Null };
    json!({
        "image": e.image_reference(),
        "digest": e.digest(),
        "version": e.version(),
        "timestamp_unix": e.image.as_ref().and_then(apex_bootc::ImageStatus::timestamp_unix),
        "download_only": e.is_download_only(),
        "pinned": e.pinned,
        "incompatible": e.incompatible,
        "soft_reboot_capable": e.soft_reboot_capable,
        "root": match e.deployment_root() {
            Ok(p) => p.display().to_string(),
            Err(err) => format!("<{err}>"),
        },
        "cached_update": e.cached_update.as_ref().map(|c| json!({
            "digest": c.image_digest,
            "version": c.version,
        })),
    })
}

fn project(h: &Host) -> Value {
    json!({
        "api_version": h.api_version,
        "bootc_host": h.is_bootc_host(),
        "spec_image": h.spec.image.as_ref().map(|i| format!("{}:{}", i.transport, i.image)),
        "spec_signature": h.spec.image.as_ref().and_then(|i| i.signature.as_ref()).map(|s| format!("{s:?}")),
        "boot_order": format!("{:?}", h.spec.boot_order),
        "type": h.status.host_type.map(|t| format!("{t:?}")),
        "rollback_queued": h.rollback_queued(),
        "incompatible": h.incompatible(),
        "usr_overlay": h.usr_overlay().map(|o| format!("{:?}/{:?}", o.access_mode, o.persistence)),
        "staged_download_only": h.staged_download_only(),
        "booted": entry(h.booted()),
        "staged": entry(h.staged()),
        "rollback": entry(h.rollback()),
        "other_deployments": h.status.other_deployments.iter().map(|e| entry(Some(e))).collect::<Vec<_>>(),
    })
}

#[test]
fn every_upstream_fixture_parses_and_matches_its_snapshot() {
    let all = fixtures("bootc-upstream");
    assert_eq!(all.len(), 16, "all of bootc's fixtures are converted");
    for (stem, path) in all {
        let host = Host::from_json(std::fs::read(&path).unwrap())
            .unwrap_or_else(|e| panic!("{stem}: {e}"));
        insta::assert_json_snapshot!(format!("upstream__{stem}"), project(&host));
    }
}

#[test]
fn every_synthetic_fixture_parses_and_matches_its_snapshot() {
    for (stem, path) in fixtures("synthetic") {
        let host = Host::from_json(std::fs::read(&path).unwrap())
            .unwrap_or_else(|e| panic!("{stem}: {e}"));
        insta::assert_json_snapshot!(format!("synthetic__{stem}"), project(&host));
    }
}

#[test]
fn staged_download_only() {
    let h = load("bootc-upstream", "spec-staged-download-only");
    assert_eq!(h.staged_download_only(), Some(true));
    assert_eq!(
        h.staged_digest(),
        Some("sha256:16dc2b6256b4ff0d2ec18d2dbfb06d117904010c8cf9732cdb022818cf7a7566")
    );
    assert_eq!(
        h.booted_digest(),
        Some("sha256:736b359467c9437c1ac915acaae952aad854e07eb4a16a94999a48af08c83c34")
    );
    assert!(h.is_bootc_host());
    let h = load("bootc-upstream", "spec-staged-booted");
    assert_eq!(h.staged_download_only(), Some(false));
}

#[test]
fn cached_update_after_check() {
    let h = load("bootc-upstream", "spec-booted-with-update");
    let update = h.cached_update().unwrap();
    assert_eq!(
        update.image_digest,
        "sha256:a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0"
    );
    assert_eq!(update.version.as_deref(), Some("stream9.20240901.0"));
    assert_eq!(update.timestamp_unix(), Some(1_725_192_000));
    let h = load("bootc-upstream", "spec-booted-with-update-no-version");
    assert_eq!(h.cached_update().unwrap().version, None);
    let h = load("bootc-upstream", "spec-booted-update-same-digest");
    assert_eq!(
        h.cached_update().map(|c| c.image_digest.as_str()),
        h.booted_digest()
    );
}

#[test]
fn not_a_bootc_host() {
    let h = load("bootc-upstream", "spec-v1-null");
    assert_eq!(h.api_version, apex_bootc::API_VERSION_V1);
    assert!(!h.is_bootc_host());
    assert!(matches!(
        h.require_bootc_host(),
        Err(BootcError::NotBootcHost { .. })
    ));
    assert_eq!(h.booted_digest(), None);
    assert_eq!(h.staged_download_only(), None);

    // Booted from a plain ostree commit, with an incompatible (rpm-ostree) stage.
    let h = load("bootc-upstream", "spec-rfe-ostree-deployment");
    assert!(!h.is_bootc_host());
    assert!(h.incompatible());
    assert!(h.require_bootc_host().is_err());

    // Rolled back to a deployment with no booted entry at all.
    let h = load("bootc-upstream", "spec-staged-rollback");
    assert!(h.booted().is_none());
    assert_eq!(
        h.rollback_digest(),
        Some("sha256:736b359467c9437c1ac915acaae952aad854e07eb4a16a94999a48af08c83c34")
    );
}

#[test]
fn flags_and_extra_deployments() {
    let h = load("bootc-upstream", "spec-booted-usroverlay");
    assert_eq!(
        h.usr_overlay(),
        Some(&FilesystemOverlay {
            access_mode: FilesystemOverlayAccessMode::ReadWrite,
            persistence: FilesystemOverlayPersistence::Transient,
        })
    );
    let h = load("bootc-upstream", "spec-booted-pinned");
    assert!(h.booted().unwrap().pinned);
    assert_eq!(h.status.other_deployments.len(), 1);
    assert!(h.status.other_deployments[0].pinned);
    let h = load("bootc-upstream", "spec-ostree-remote");
    assert_eq!(
        h.spec.image.unwrap().signature,
        Some(ImageSignature::OstreeRemote("fedora".into()))
    );
    let h = load("bootc-upstream", "spec-ostree-to-bootc");
    assert_eq!(h.staged().unwrap().store, Some(Store::OstreeContainer));
    assert_eq!(h.booted().unwrap().store, None);
}

#[test]
fn synthetic_apex_download_only_stage() {
    let h = load("synthetic", "apex-staged-download-only");
    let (booted, image) = h.require_bootc_host().unwrap();
    assert_eq!(image.version.as_deref(), Some("45.20260923.114"));
    assert_eq!(booted.image_reference(), Some("ghcr.io/apex-linux/apex:45"));
    assert_eq!(h.status.host_type, Some(HostType::BootcHost));
    assert_eq!(h.staged_download_only(), Some(true));
    assert_eq!(
        h.staged().and_then(BootEntry::version),
        Some("45.20260924.117")
    );
    assert_eq!(
        h.cached_update().map(|c| c.image_digest.as_str()),
        h.staged_digest()
    );
    assert_eq!(
        h.staged().unwrap().deployment_root().unwrap(),
        Path::new(
            "/sysroot/ostree/deploy/default/deploy/c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00.0"
        )
    );
    assert!(!h.rollback_queued() && !h.incompatible() && h.usr_overlay().is_none());
    assert_eq!(image.timestamp_unix(), Some(1_790_133_002));
}

#[test]
fn synthetic_flags_and_future_fields() {
    let h = load("synthetic", "apex-flags-and-drift");
    assert!(h.rollback_queued());
    assert!(h.incompatible(), "the staged entry is incompatible");
    assert!(h.usr_overlay().is_some());
    assert_eq!(h.spec.boot_order, BootOrder::Rollback);
    assert_eq!(h.booted().unwrap().store, Some(Store::Unknown));
    assert!(h.booted().unwrap().soft_reboot_capable);
    assert_eq!(
        h.rollback()
            .unwrap()
            .image
            .as_ref()
            .unwrap()
            .image
            .signature,
        Some(ImageSignature::Unknown)
    );
    assert_eq!(h.status.other_deployments.len(), 1);
    // `+02:00` offset: 03:10:02 local is 01:10:02 UTC.
    assert_eq!(
        h.booted().unwrap().image.as_ref().unwrap().timestamp_unix(),
        Some(1_790_125_802)
    );
    assert_eq!(
        h.metadata
            .annotations
            .as_ref()
            .map(std::collections::BTreeMap::len),
        Some(1)
    );
}
