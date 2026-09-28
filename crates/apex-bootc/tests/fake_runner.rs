//! `Bootc` over the scripted `FakeRunner` (feature `testing`), the way `apex-updated`'s
//! tests will drive it.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; clippy's allow-unwrap-in-tests does not cover them"
)]

use std::sync::Arc;
use std::time::Duration;

use apex_bootc::{
    APEX_TESTING_REPOSITORY, Bootc, BootcCommand, BootcError, CancelToken, CheckOutcome, Digest,
    DownloadOutcome, ErrorKind, FakeRunner, ImageRef, Scripted, Stage,
};
use tokio::sync::mpsc;

const DIGEST: &str = "sha256:0b4c83fba9a0e3e2e5a7e9dbb5f4c1f1d7b5d1c6e0a3f9e8d7c6b5a4f3e2d1c0";

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{path}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[tokio::test]
async fn check_download_apply_flow() {
    let fake = Arc::new(FakeRunner::new());
    fake.push(Scripted::new(BootcCommand::Status).stdout(fixture("status/bootc-upstream/spec-only-booted.json")))
        .push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
            "Update available for: ostree-image-signed:docker://ghcr.io/sekirokenjii/apex:45\n  Version: 45.20260924.117\n  Digest: {DIGEST}\n"
        )))
        .push(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: true })
                .progress_line(r#"{"type":"Start","version":"0.1.0"}"#)
                .progress_bytes(0, 400)
                .delay(Duration::from_millis(5))
                .progress_bytes(200, 400)
                .progress_line("garbage that must be ignored")
                .progress_bytes(400, 400)
                .stdout("Staged but not queued for next boot: ghcr.io/sekirokenjii/apex:45\n"),
        )
        .push(
            Scripted::new(BootcCommand::Status)
                .stdout(fixture("status/synthetic/apex-staged-download-only.json")),
        )
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded).stdout(
            "Staged deployment will now be applied on reboot\n",
        ));

    let bootc = Bootc::new(Arc::clone(&fake));
    let cancel = CancelToken::new();

    let host = bootc.status(&cancel).await.unwrap();
    assert!(host.is_bootc_host());
    assert_eq!(host.staged(), None);

    let outcome = bootc.check(&cancel).await.unwrap();
    assert_eq!(
        outcome,
        CheckOutcome::Available {
            version: Some("45.20260924.117".into()),
            digest: Some(Digest::parse(DIGEST).unwrap()),
        }
    );

    let (tx, mut rx) = mpsc::channel(16);
    bootc.download(Some(tx), &cancel).await.unwrap();
    let mut seen = Vec::new();
    while let Some(p) = rx.recv().await {
        assert_eq!(p.stage, Stage::Pulling);
        seen.push(p.fraction.unwrap());
    }
    assert_eq!(seen, [0.0, 0.5, 1.0]);

    let host = bootc.status(&cancel).await.unwrap();
    assert_eq!(host.staged_download_only(), Some(true));
    assert_eq!(host.staged_digest(), Some(DIGEST));

    bootc.apply_downloaded(&cancel).await.unwrap();
    fake.assert_finished();
    assert_eq!(fake.calls().len(), 5);
}

#[tokio::test]
async fn download_falls_back_when_progress_fd_is_unsupported() {
    let fake = FakeRunner::new()
        .with(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: true })
                .exit_code(2)
                .stderr("error: unexpected argument '--progress-fd' found\n\nUsage: bootc upgrade [OPTIONS]\n"),
        )
        .with(Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false }));
    let (tx, _rx) = mpsc::channel(1);
    Bootc::new(&fake)
        .download(Some(tx), &CancelToken::new())
        .await
        .unwrap();
    fake.assert_finished();
}

/// Cancelling an applied update re-locks it with `upgrade --download-only`. When the
/// channel tag has moved back to the booted image, bootc prints "No update available.",
/// exits 0 and leaves the stage unlocked; the outcome must say so.
#[tokio::test]
async fn relocking_is_reported_not_assumed() {
    let fake = FakeRunner::new()
        .with(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false })
                .stdout("Fetched layers: 0 B in 0 seconds (0 B/s)\nNo update available.\n"),
        )
        .with(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false }).stdout(format!(
                "No changes in ghcr.io/sekirokenjii/apex:45 => {DIGEST}\n\
                 Image downloaded, but will not be applied on reboot\n"
            )),
        )
        .with(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false }).stdout(format!(
                "Staged but not queued for next boot: ghcr.io/sekirokenjii/apex:45\n  \
                 Version: 45.20260925.1\n  Digest: {DIGEST}\n"
            )),
        )
        .with(Scripted::new(BootcCommand::UpgradeDownloadOnly {
            progress: false,
        }));
    let bootc = Bootc::new(&fake);
    let cancel = CancelToken::new();

    let tag_moved_back = bootc.upgrade_download_only(None, &cancel).await.unwrap();
    assert_eq!(tag_moved_back, DownloadOutcome::NoUpdate);
    assert!(!tag_moved_back.stage_locked());

    let same_digest = bootc.upgrade_download_only(None, &cancel).await.unwrap();
    assert_eq!(same_digest, DownloadOutcome::Relocked);
    assert!(same_digest.stage_locked());

    let tag_moved_on = bootc.upgrade_download_only(None, &cancel).await.unwrap();
    assert_eq!(
        tag_moved_on,
        DownloadOutcome::Staged {
            locked: true,
            version: Some("45.20260925.1".into()),
            digest: Some(Digest::parse(DIGEST).unwrap()),
        }
    );

    // Empty output is not taken as success, although bootc exited 0.
    let silent = bootc.upgrade_download_only(None, &cancel).await.unwrap();
    assert_eq!(silent, DownloadOutcome::Unknown);
    assert!(!silent.stage_locked());
    fake.assert_finished();
}

#[tokio::test]
async fn the_fallback_without_progress_reports_its_outcome() {
    let fake = FakeRunner::new()
        .with(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: true })
                .exit_code(2)
                .stderr("error: unexpected argument '--progress-fd' found\n\nUsage: bootc upgrade [OPTIONS]\n"),
        )
        .with(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false })
                .stdout("Staged update present, not changed\n"),
        );
    let (tx, _rx) = mpsc::channel(1);
    let outcome = Bootc::new(&fake)
        .upgrade_download_only(Some(tx), &CancelToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, DownloadOutcome::StageUnchanged);
    fake.assert_finished();
}

/// A pull retry delivered through the runner: the overall value the daemon forwards to
/// the `Progress` property does not jump back.
#[tokio::test]
async fn progress_through_a_retry_does_not_go_backwards() {
    let retry = r#"{"description":"Container image pull failed; retrying in 2 seconds (1/3): Pulling","id":"pull-retry","steps":1,"stepsCached":0,"stepsTotal":3,"subtasks":[],"task":"pulling","type":"ProgressSteps"}"#;
    let importing = r#"{"description":"Importing Image","id":"sha256:fake","steps":1,"stepsCached":0,"stepsTotal":1,"subtasks":[],"task":"importing","type":"ProgressSteps"}"#;
    let fake = FakeRunner::new().with(
        Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: true })
            .progress_bytes(0, 400)
            .progress_bytes(300, 400)
            .progress_line(importing)
            .progress_line(retry)
            .progress_bytes(0, 200)
            .progress_bytes(200, 200)
            .progress_line(importing),
    );
    let (tx, mut rx) = mpsc::channel(16);
    Bootc::new(&fake)
        .download(Some(tx), &CancelToken::new())
        .await
        .unwrap();
    let mut overall = Vec::new();
    while let Some(p) = rx.recv().await {
        overall.push(p.overall().map(|o| (o * 1000.0).round() / 1000.0));
    }
    assert_eq!(
        overall,
        [
            Some(0.0),
            Some(0.675),
            None,
            None,
            Some(0.675),
            Some(0.9),
            Some(0.95)
        ]
    );
}

#[tokio::test]
async fn failures_are_classified() {
    let fake = FakeRunner::new()
        .with(
            Scripted::new(BootcCommand::UpgradeCheck)
                .exit_code(1)
                .stderr("error: Upgrading: pinging container registry ghcr.io: dial tcp: lookup ghcr.io: no such host\n"),
        )
        .with(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false })
                .exit_code(1)
                .stderr("error: Upgrading: Pulling: A signature was required, but no signature exists\n"),
        )
        .with(Scripted::new(BootcCommand::Rollback).error(BootcError::Busy {
            detail: "scripted".into(),
        }))
        .with(Scripted::new(BootcCommand::Status).stdout("{\"kind\":\"Pod\"}"))
        .with(Scripted::new(BootcCommand::Status).stdout(fixture("status/bootc-upstream/spec-v1-null.json")))
        .with(Scripted::new(BootcCommand::UpgradeFromDownloaded).killed_by(9));
    let bootc = Bootc::new(&fake);
    let cancel = CancelToken::new();

    assert_eq!(
        bootc.check(&cancel).await.unwrap_err().kind(),
        ErrorKind::Network
    );
    assert_eq!(
        bootc.download(None, &cancel).await.unwrap_err().kind(),
        ErrorKind::SignatureRejected
    );
    assert_eq!(
        bootc.rollback(&cancel).await.unwrap_err().kind(),
        ErrorKind::Busy
    );
    assert_eq!(
        bootc.status(&cancel).await.unwrap_err().kind(),
        ErrorKind::InvalidStatus
    );
    // A host bootc does not manage is a successful status; the caller decides.
    let host = bootc.status(&cancel).await.unwrap();
    assert_eq!(
        host.require_bootc_host().unwrap_err().kind(),
        ErrorKind::NotBootcHost
    );
    let err = bootc.apply_downloaded(&cancel).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Other);
    assert!(err.to_string().contains("killed by signal 9"), "{err}");
    fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_download_longer_than_two_hours_times_out() {
    let fake = FakeRunner::new().with(
        Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false })
            .delay(Duration::from_hours(3)),
    );
    let err = Bootc::new(&fake)
        .download(None, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, BootcError::Timeout { timeout, .. } if timeout == Duration::from_hours(2)),
        "{err}"
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_stops_a_running_download() {
    let fake = FakeRunner::new().with(
        Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: true })
            .progress_bytes(1, 10)
            .delay(Duration::from_mins(30)),
    );
    let cancel = CancelToken::new();
    let (tx, mut rx) = mpsc::channel(4);
    let bootc = Bootc::new(&fake);
    let download = bootc.download(Some(tx), &cancel);
    let canceller = async {
        let first = rx.recv().await.unwrap();
        assert_eq!(first.fraction, Some(0.1));
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(download, canceller);
    assert_eq!(result.unwrap_err().kind(), ErrorKind::Cancelled);
}

#[tokio::test]
async fn switch_passes_exactly_the_requested_reference() {
    let target = ImageRef::at_digest(APEX_TESTING_REPOSITORY, DIGEST).unwrap();
    let fake = FakeRunner::new().with(Scripted::new(BootcCommand::SwitchDownloadOnly {
        target: target.clone(),
    }));
    Bootc::new(&fake)
        .switch_download_only(target, &CancelToken::new())
        .await
        .unwrap();
    let calls = fake.calls();
    let argv = calls[0].argv();
    assert_eq!(
        argv.last().unwrap(),
        &std::ffi::OsString::from(format!("ghcr.io/sekirokenjii/apex-testing@{DIGEST}"))
    );
    fake.assert_finished();
}

#[tokio::test]
async fn unexpected_commands_are_reported() {
    let fake = FakeRunner::new().with(Scripted::new(BootcCommand::Status));
    let err = Bootc::new(&fake)
        .rollback(&CancelToken::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("FakeRunner"), "{err}");
    assert_eq!(fake.remaining(), 1);
    assert_eq!(fake.unexpected().len(), 1);
    let result = std::panic::catch_unwind(|| fake.assert_finished());
    assert!(result.is_err());
}
