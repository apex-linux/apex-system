//! Engine-level tests: the real engine, queue and worker, with bootc scripted by
//! `FakeRunner`, the system faked by `FakeSystem`, grubenv recorded, and the state files in
//! a temporary directory. Every test ends with `assert_finished`, so a bootc command that
//! was not expected (or one that was expected but not run) fails it.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests: a failed expectation is the test failure"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use apex_boot::writer::EditenvCall;
use apex_bootc::{
    APEX_TESTING, BootEntry, BootcCommand, BootcError, CancelToken, CommandOutput, CommandRunner,
    ImageRef, Invocation, Scripted,
};
use apex_dbus::{BootVerdict, Channel, ErrorKind, State};
use apex_update::{HealthFailureRecord, Intent, StateDir, StateStore, StoreError};
use apex_updated::REFRESH_MAX_AGE;
use apex_updated::daemon::Daemon;
use apex_updated::engine::{
    Engine, EngineParts, FALLBACK_REASON, MARK_BAD_REASON, PING_WAIT, STATUS_SKIPPED,
};
use apex_updated::providers::ASSESS_UNIT;
use apex_updated::queue::{Op, OpKind};
use apex_updated::store::Store as _;
use apex_updated::testing::{FakeSystem, SharedGrubWriter};
use common::*;
use tokio::time::Instant;

/// The arming `Apply` writes, with the variable names of M1 contract §5 spelled out (not
/// taken from apex-boot), so a rename on either side fails here.
fn arm_call(digest: &str) -> EditenvCall {
    EditenvCall::Set(vec![
        ("apex_target".into(), digest.into()),
        ("apex_counter".into(), "3".into()),
        ("apex_ok".into(), "0".into()),
    ])
}

fn disarm_call() -> EditenvCall {
    EditenvCall::Unset(vec!["apex_target".into(), "apex_counter".into()])
}

fn kind(result: Result<(), apex_dbus::ApexError>) -> Option<ErrorKind> {
    result.expect_err("the call must fail").kind()
}

fn download_only(progress: bool) -> BootcCommand {
    BootcCommand::UpgradeDownloadOnly { progress }
}

// ----- download gating and DeferredReason ----------------------------------------------

#[tokio::test(start_paused = true)]
async fn metered_network_defers_and_the_override_downloads() {
    let h = Harness::new();
    h.system.update(|s| s.metered = Some(true));
    h.last_check(REMOTE, STAGED_VERSION, 3600);
    let before = host(booted(), None, Some(rollback()));
    h.refresh(&before).await;
    let properties = h.properties();
    assert_eq!(properties.state, State::Available);
    assert_eq!(properties.available.digest.as_deref(), Some(REMOTE));
    assert!(
        properties.deferred_reason.contains("metered"),
        "{}",
        properties.deferred_reason
    );

    // A user download without the override is refused with the gate's reason.
    h.push(status(&before));
    let error = h
        .run(Op::Download {
            override_soft_gates: false,
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert_eq!(error.message(), properties.deferred_reason);

    // With the override it downloads, reporting progress while bootc runs.
    let after = host(
        booted(),
        Some(entry(REMOTE, STAGED_VERSION, STAGED_CSUM)).map(|mut e| {
            e.download_only = true;
            e
        }),
        Some(rollback()),
    );
    h.push(status(&before))
        .push(
            Scripted::new(download_only(true))
                .progress_bytes(50, 100)
                .delay(Duration::from_secs(10)),
        )
        .push(status(&after));
    let download = h.run(Op::Download {
        override_soft_gates: true,
    });
    let observe = async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        h.properties()
    };
    let (result, during) = tokio::join!(download, observe);
    result.expect("download");
    assert_eq!(during.state, State::Downloading);
    assert!((during.progress - 0.45).abs() < 1e-9, "{}", during.progress);

    let done = h.properties();
    assert_eq!(done.state, State::Staged);
    assert_eq!(done.staged.digest.as_deref(), Some(REMOTE));
    assert_eq!(done.staged.download_only, Some(true));
    assert_eq!(done.progress.to_bits(), (-1.0f64).to_bits());
    assert_eq!(done.deferred_reason, "");
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn automatic_run_checks_and_records_why_it_did_not_download() {
    let h = Harness::new();
    h.system.update(|s| {
        s.power.on_ac = false;
        s.power.battery_percent = Some(30);
    });
    let current = host(booted(), None, None);
    // Refresh, then (power already fails, so only a check) check and status.
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
            "Update available for: ostree-image-signed:docker://{STABLE_IMAGE}\n  Version: {STAGED_VERSION}\n  Digest: {REMOTE}\n"
        )))
        .push(status(&current));
    h.run(Op::Automatic)
        .await
        .expect("a deferral is not an error");
    let properties = h.properties();
    assert_eq!(properties.state, State::Available);
    assert_eq!(properties.last_check, NOW);
    assert!(
        properties.deferred_reason.contains("battery is below 50%"),
        "{}",
        properties.deferred_reason
    );
    let saved = h.store.read_last_check().unwrap();
    assert_eq!(saved.remote_digest.as_deref(), Some(REMOTE));
    assert_eq!(saved.remote_version.as_deref(), Some(STAGED_VERSION));
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn automatic_run_downloads_when_every_gate_passes() {
    let h = Harness::new();
    let current = host(booted(), None, None);
    let after = host(booted(), Some(staged(true)), None);
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
            "Update available for: x\n  Version: {STAGED_VERSION}\n  Digest: {STAGED}\n"
        )))
        .push(status(&current))
        .push(Scripted::new(download_only(true)))
        .push(status(&after));
    h.run(Op::Automatic).await.expect("automatic");
    assert_eq!(h.properties().state, State::Staged);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn automatic_run_never_touches_an_applied_update() {
    let h = Harness::new();
    h.store
        .write_intent(&Intent {
            digest: STAGED.into(),
            created: NOW,
        })
        .unwrap();
    h.push(status(&host(booted(), Some(staged(false)), None)));
    h.run(Op::Automatic).await.expect("skipped");
    assert_eq!(h.properties().state, State::ReadyForReboot);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_download_that_stages_another_digest_updates_the_check_record() {
    let h = Harness::new();
    // The last check saw REMOTE, but the tag has moved to STAGED since.
    h.last_check(REMOTE, "45.20260923.1", 3600);
    let current = host(booted(), None, None);
    h.push(status(&current))
        .push(Scripted::new(download_only(true)))
        .push(status(&host(booted(), Some(staged(true)), None)));
    h.run(Op::Download {
        override_soft_gates: false,
    })
    .await
    .expect("download");
    let check = h.store.read_last_check().unwrap();
    assert_eq!(check.remote_digest.as_deref(), Some(STAGED));
    assert_eq!(check.remote_version.as_deref(), Some(STAGED_VERSION));
    assert!(h.properties().available.is_empty());
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_failed_download_keeps_the_check_record_and_reports_the_error() {
    let h = Harness::new();
    h.last_check(REMOTE, "45.20260925.1", 3600);
    // An older download is still staged; the new one fails.
    let current = host(booted(), Some(staged(true)), None);
    h.push(status(&current))
        .push(Scripted::new(download_only(true)).exit_code(1).stderr(
            "error: Pulling: dial tcp 140.82.114.34:443: connect: network is unreachable\n",
        ))
        .push(status(&current));
    assert_eq!(
        kind(
            h.run(Op::Download {
                override_soft_gates: false,
            })
            .await
        ),
        Some(ErrorKind::Network)
    );
    assert_eq!(
        h.store.read_last_check().unwrap().remote_digest.as_deref(),
        Some(REMOTE)
    );
    let properties = h.properties();
    assert_eq!(properties.state, State::Staged);
    assert_eq!(properties.available.digest.as_deref(), Some(REMOTE));
    assert!(properties.last_error.contains("network is unreachable"));
    h.fake.assert_finished();
}

// ----- what a download actually staged ---------------------------------------------------

const MOVED_CSUM: &str = "6666666666666666666666666666666666666666666666666666666666666666";

/// A harness whose `MOVED_CSUM` deployment holds `version` (the build the tag moved to).
fn with_moved_build(version: &str) -> Harness {
    let mut state = fake_state();
    state
        .manifests
        .insert(MOVED_CSUM.into(), manifest(version, Vec::new()));
    Harness::with(state, SharedGrubWriter::new())
}

/// The tag's new image, `REMOTE` at `version`, staged locked or not.
fn moved_stage(version: &str, download_only: bool) -> BootEntry {
    BootEntry {
        download_only,
        ..entry(REMOTE, version, MOVED_CSUM)
    }
}

fn deny(h: &Harness, digest: &str) {
    let mut denylist = apex_update::Denylist::new();
    denylist.insert(apex_update::DenyEntry {
        digest: digest.into(),
        reason: FALLBACK_REASON.into(),
        at: NOW - 86_400,
    });
    h.store.write_denylist(&denylist).unwrap();
}

/// The check found a good build, but the tag moved to one that failed before while the
/// automatic run downloaded. It stays locked, is never offered or installable, and
/// `DeferredReason` says why; the timer's run itself succeeded.
#[tokio::test(start_paused = true)]
async fn an_automatic_download_that_staged_a_denylisted_build_keeps_it_locked() {
    let newer = "45.20260925.1";
    let h = with_moved_build(newer);
    deny(&h, REMOTE);
    let current = host(booted(), None, None);
    let staged_denied = host(booted(), Some(moved_stage(newer, true)), None);
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
            "Update available for: x\n  Version: {STAGED_VERSION}\n  Digest: {STAGED}\n"
        )))
        .push(status(&current))
        .push(Scripted::new(download_only(true)).stdout(format!(
            "Staged but not queued for next boot: x\n  Version: {newer}\n  Digest: {REMOTE}\n"
        )))
        .push(status(&staged_denied));
    h.run(Op::Automatic)
        .await
        .expect("the timer's run did its job");
    let properties = h.properties();
    assert_eq!(properties.state, State::Staged);
    assert_eq!(properties.staged.digest.as_deref(), Some(REMOTE));
    assert_eq!(properties.staged.download_only, Some(true));
    assert!(properties.available.is_empty(), "never offered");
    assert!(
        properties
            .deferred_reason
            .contains("failed to start correctly before"),
        "{}",
        properties.deferred_reason
    );
    assert_eq!(properties.last_error, "");
    assert_eq!(
        h.store.read_last_check().unwrap().remote_digest.as_deref(),
        Some(REMOTE)
    );

    // Never ready: Apply refuses it, and nothing is armed.
    h.push(status(&staged_denied));
    let error = h
        .run(Op::Apply {
            expected_digest: REMOTE.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert!(h.grub.calls().is_empty());
    h.fake.assert_finished();
}

/// A user's download staged an older build than the check found (the tag moved back): the
/// call fails with the reason, which `LastError` and `DeferredReason` keep as well.
#[tokio::test(start_paused = true)]
async fn a_user_download_that_staged_an_older_build_says_it_will_not_be_installed() {
    let h = with_moved_build(OLDER_VERSION);
    h.last_check(STAGED, STAGED_VERSION, 3600);
    h.push(status(&host(booted(), None, None)))
        .push(Scripted::new(download_only(true)))
        .push(status(&host(
            booted(),
            Some(moved_stage(OLDER_VERSION, true)),
            None,
        )));
    let error = h
        .run(Op::Download {
            override_soft_gates: false,
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert_eq!(
        error.message(),
        format!(
            "The downloaded update {OLDER_VERSION} is not newer than the installed version {BOOTED_VERSION}, so it will not be installed."
        )
    );
    let properties = h.properties();
    assert_eq!(properties.last_error, error.message());
    assert_eq!(properties.deferred_reason, error.message());
    assert_eq!(properties.state, State::Staged);
    assert!(properties.available.is_empty());
    h.fake.assert_finished();
}

/// A bootc that ignored `--download-only` left the new stage set to install, and nothing
/// armed the counter for it: it is locked again at once.
#[tokio::test(start_paused = true)]
async fn a_download_left_unlocked_is_locked_again() {
    let newer = "45.20260925.1";
    let h = with_moved_build(newer);
    deny(&h, REMOTE);
    h.last_check(STAGED, STAGED_VERSION, 3600);
    h.push(status(&host(booted(), None, None)))
        .push(Scripted::new(download_only(true)).stdout(format!(
            "Queued for next boot: x\n  Version: {newer}\n  Digest: {REMOTE}\n"
        )))
        .push(status(&host(
            booted(),
            Some(moved_stage(newer, false)),
            None,
        )))
        .push(Scripted::new(download_only(false)).stdout(RELOCKED))
        .push(status(&host(
            booted(),
            Some(moved_stage(newer, true)),
            None,
        )));
    let error = h
        .run(Op::Download {
            override_soft_gates: false,
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    let properties = h.properties();
    assert_eq!(properties.state, State::Staged);
    assert_eq!(properties.staged.download_only, Some(true));
    assert!(h.grub.calls().is_empty());
    h.fake.assert_finished();
}

// ----- Apply -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn apply_refuses_a_digest_that_is_not_staged() {
    let h = Harness::new();
    h.push(status(&host(
        booted(),
        Some(staged(true)),
        Some(rollback()),
    )));
    let error = h
        .run(Op::Apply {
            expected_digest: REMOTE.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::DigestMismatch));
    assert!(error.message().contains(STAGED), "{}", error.message());
    assert!(h.grub.calls().is_empty(), "nothing armed");
    assert_eq!(h.intent(), None);
    // A refusal is the caller's answer, not an operation failure.
    assert_eq!(h.properties().last_error, "");
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn apply_arms_then_unlocks_then_records_the_intent() {
    let h = Harness::new();
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(booted(), Some(staged(false)), None)));
    h.run(Op::Apply {
        expected_digest: STAGED.into(),
    })
    .await
    .expect("apply");

    // Armed with one grub2-editenv call, before the unlock (the only other command).
    assert_eq!(h.grub.calls(), [arm_call(STAGED)]);
    assert_eq!(
        h.fake.calls(),
        [
            BootcCommand::Status,
            BootcCommand::UpgradeFromDownloaded,
            BootcCommand::Status
        ]
    );
    assert_eq!(
        h.grub.env().get_str("apex_target").unwrap(),
        Some(STAGED),
        "the counter is armed for the reviewed digest"
    );
    let intent = h.intent().expect("intent written");
    assert_eq!(intent.digest, STAGED);
    assert_eq!(intent.created, NOW);
    assert_eq!(h.properties().state, State::ReadyForReboot);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn apply_disarms_when_the_unlock_fails() {
    let h = Harness::new();
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(
            Scripted::new(BootcCommand::UpgradeFromDownloaded)
                .exit_code(1)
                .stderr("error: Upgrading: staged deployment vanished\n"),
        )
        .push(status(&host(booted(), Some(staged(true)), None)));
    let error = h
        .run(Op::Apply {
            expected_digest: STAGED.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Bootc));
    assert_eq!(h.grub.calls(), [arm_call(STAGED), disarm_call()]);
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), None);
    assert_eq!(h.intent(), None);
    let properties = h.properties();
    assert!(
        properties.last_error.contains("staged deployment vanished"),
        "{}",
        properties.last_error
    );
    assert_eq!(properties.state, State::Staged);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn apply_does_not_arm_when_arming_fails() {
    let h = Harness::with(fake_state(), SharedGrubWriter::failing_at(0));
    h.push(status(&host(booted(), Some(staged(true)), None)));
    let error = h
        .run(Op::Apply {
            expected_digest: STAGED.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Internal));
    assert!(
        error.message().contains("boot counter"),
        "{}",
        error.message()
    );
    assert_eq!(h.intent(), None);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn apply_refuses_a_downgrade_read_from_the_staged_manifest() {
    let h = Harness::new();
    // The label claims a newer version; the staged image's own manifest says older.
    h.system.update(|s| {
        s.manifests
            .insert(STAGED_CSUM.into(), manifest(OLDER_VERSION, Vec::new()));
    });
    h.push(status(&host(booted(), Some(staged(true)), None)));
    let error = h
        .run(Op::Apply {
            expected_digest: STAGED.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert!(
        error.message().contains(OLDER_VERSION),
        "{}",
        error.message()
    );
    assert!(h.grub.calls().is_empty());
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn apply_checks_boot_space_and_waits_for_the_boot_assessment() {
    let h = Harness::new();
    h.system.update(|s| s.boot_free = Some(100 * 1024 * 1024));
    h.push(status(&host(booted(), Some(staged(true)), None)));
    let apply = || Op::Apply {
        expected_digest: STAGED.into(),
    };
    let error = h.run(apply()).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert!(
        error.message().contains("boot partition"),
        "{}",
        error.message()
    );

    h.system.update(|s| {
        s.boot_free = Some(600 * 1024 * 1024);
        s.units.insert(ASSESS_UNIT.into(), "activating".into());
    });
    h.push(status(&host(booted(), Some(staged(true)), None)));
    assert_eq!(kind(h.run(apply()).await), Some(ErrorKind::Busy));
    assert!(h.grub.calls().is_empty());
    h.fake.assert_finished();
}

/// grubenv with Apex's variables set to `vars`.
fn grubenv_with(vars: &[(&str, &str)]) -> SharedGrubWriter {
    let mut env = SharedGrubWriter::new().env();
    for (name, value) in vars {
        env.set(name, value).unwrap();
    }
    SharedGrubWriter::with_env(env)
}

/// The arming precondition's second half (`apex_boot::check_arming`, M1 contract §5): the
/// running version must be confirmed, because finalizing the stage keeps only the new
/// deployment and the running one. Each refusal is `PolicyDenied` in plain words, and
/// nothing is armed, unlocked or recorded.
#[tokio::test(start_paused = true)]
async fn apply_refuses_to_arm_on_top_of_an_unconfirmed_or_failed_running_version() {
    let apply = || Op::Apply {
        expected_digest: STAGED.into(),
    };
    let ready = host(booted(), Some(staged(true)), None);

    // grubenv is still armed for the running version: its startup check has not passed
    // (GRUB counted this boot, the assessment has not confirmed it).
    let h = Harness::with(
        fake_state(),
        grubenv_with(&[
            ("apex_target", BOOTED),
            ("apex_counter", "2"),
            ("apex_ok", "0"),
        ]),
    );
    h.push(status(&ready));
    let error = h.run(apply()).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert!(
        error
            .message()
            .contains("has not passed its startup check yet"),
        "{}",
        error.message()
    );
    assert!(h.grub.calls().is_empty());
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), Some(BOOTED));
    assert_eq!(h.intent(), None);
    assert_eq!(
        h.properties().last_error,
        "",
        "a refusal is the caller's answer"
    );
    h.fake.assert_finished();

    // health-failure.json names the running version: it failed its startup check.
    let h = Harness::new();
    h.store
        .write_health_failure(&HealthFailureRecord {
            digest: BOOTED.into(),
            failed_checks: vec!["session".into()],
            at: NOW - 60,
        })
        .unwrap();
    h.push(status(&ready));
    let error = h.run(apply()).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert!(
        error.message().contains("failed its startup check"),
        "{}",
        error.message()
    );
    assert!(h.grub.calls().is_empty());
    assert_eq!(h.intent(), None);
    h.fake.assert_finished();

    // The counter's state cannot be interpreted: the precondition cannot be shown.
    let h = Harness::with(
        fake_state(),
        grubenv_with(&[("apex_target", BOOTED), ("apex_counter", "two")]),
    );
    h.push(status(&ready));
    let error = h.run(apply()).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Internal));
    assert!(
        error.message().contains("startup check could not be read"),
        "{}",
        error.message()
    );
    assert!(h.grub.calls().is_empty());
    h.fake.assert_finished();
}

/// An arming left for another update (one cancelled while its stage was already locked
/// again) is not about the running version: it does not block, and the new arming
/// replaces it. A health failure recorded for another digest does not block either.
#[tokio::test(start_paused = true)]
async fn apply_arms_over_an_arming_for_another_update() {
    let h = Harness::with(
        fake_state(),
        grubenv_with(&[
            ("apex_target", REMOTE),
            ("apex_counter", "3"),
            ("apex_ok", "0"),
        ]),
    );
    h.store
        .write_health_failure(&HealthFailureRecord {
            digest: ROLLBACK.into(),
            failed_checks: Vec::new(),
            at: NOW - 86_400,
        })
        .unwrap();
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(booted(), Some(staged(false)), None)));
    h.run(Op::Apply {
        expected_digest: STAGED.into(),
    })
    .await
    .expect("apply");
    assert_eq!(h.grub.calls(), [arm_call(STAGED)]);
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), Some(STAGED));
    h.fake.assert_finished();
}

// ----- DiscardStaged -------------------------------------------------------------------

/// `bootc upgrade --download-only` stdout when it locked the applied stage again.
const RELOCKED: &str = "Image downloaded, but will not be applied on reboot\n";

fn applied_intent(h: &Harness) {
    h.store
        .write_intent(&Intent {
            digest: STAGED.into(),
            created: NOW - 60,
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn discard_relocks_then_disarms_and_forgets_the_intent() {
    let h = Harness::with(fake_state(), armed_for(STAGED));
    applied_intent(&h);
    h.push(status(&host(booted(), Some(staged(false)), None)))
        .push(Scripted::new(download_only(false)).stdout(RELOCKED))
        .push(status(&host(booted(), Some(staged(true)), None)));
    h.run(Op::Discard).await.expect("discard");
    assert_eq!(h.grub.calls(), [disarm_call()]);
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), None);
    assert_eq!(h.grub.env().get_str("apex_counter").unwrap(), None);
    assert_eq!(h.intent(), None);
    assert_eq!(h.properties().state, State::Staged);
    assert!(
        !h.fake.calls().contains(&BootcCommand::Rollback),
        "discarding never rolls back"
    );

    // Nothing applied: nothing to discard, and nothing is touched.
    h.push(status(&host(booted(), Some(staged(true)), None)));
    assert_eq!(kind(h.run(Op::Discard).await), Some(ErrorKind::NothingToDo));
    assert_eq!(h.grub.calls(), [disarm_call()]);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn discard_that_stages_a_newer_locked_digest_still_disarms() {
    // The re-lock pulls the tag, which has moved on: bootc downloads the newer image and
    // stages it locked in place of the applied one. Nothing boots next, so the arming for
    // the applied digest goes, and Staged and the check record follow the new digest.
    let h = Harness::new();
    applied_intent(&h);
    h.last_check(STAGED, STAGED_VERSION, 3600);
    let newer = "45.20260925.1";
    let replaced = host(
        booted(),
        Some(BootEntry {
            download_only: true,
            ..entry(REMOTE, newer, STAGED_CSUM)
        }),
        None,
    );
    h.push(status(&host(booted(), Some(staged(false)), None)))
        .push(Scripted::new(download_only(false)).stdout(format!(
            "Staged but not queued for next boot: ostree-image-signed:docker://{STABLE_IMAGE}\n  Version: {newer}\n  Digest: {REMOTE}\n"
        )))
        .push(status(&replaced));
    h.run(Op::Discard).await.expect("discard");
    assert_eq!(h.grub.calls(), [disarm_call()]);
    assert_eq!(h.intent(), None);
    let properties = h.properties();
    assert_eq!(properties.state, State::Staged);
    assert_eq!(properties.staged.digest.as_deref(), Some(REMOTE));
    assert_eq!(properties.staged.download_only, Some(true));
    let check = h.store.read_last_check().unwrap();
    assert_eq!(check.remote_digest.as_deref(), Some(REMOTE));
    assert_eq!(check.remote_version.as_deref(), Some(newer));
    h.fake.assert_finished();
}

/// grubenv as `Apply` leaves it for `digest`: target set, three attempts, not confirmed.
fn armed_for(digest: &str) -> SharedGrubWriter {
    let grub = SharedGrubWriter::new();
    let mut env = grub.env();
    for (name, value) in [
        ("apex_target", digest),
        ("apex_counter", "3"),
        ("apex_ok", "0"),
    ] {
        env.set(name, value).unwrap();
    }
    SharedGrubWriter::with_env(env)
}

/// The arming `Apply` wrote is still in grubenv.
fn assert_still_armed(h: &Harness, digest: &str) {
    let env = h.grub.env();
    assert_eq!(env.get_str("apex_target").unwrap(), Some(digest));
    assert_eq!(env.get_str("apex_counter").unwrap(), Some("3"));
}

/// Regression (the discard blocker): when the channel tag points back at the running image,
/// bootc prints "No update available.", changes nothing and exits 0, so the applied stage
/// still installs at the next restart. The arming and the intent must stay, whatever a
/// status read says, and the call must fail in plain words.
#[tokio::test(start_paused = true)]
async fn no_update_available_keeps_the_update_applied_and_armed() {
    let applied = host(booted(), Some(staged(false)), None);
    let relocked = host(booted(), Some(staged(true)), None);
    // The status after it shows the stage still applied (what bootc does), or locked (a
    // stale or foreign status): the cancellation is not confirmed either way.
    for after in [&applied, &relocked] {
        let h = Harness::with(fake_state(), armed_for(STAGED));
        applied_intent(&h);
        h.push(status(&applied))
            .push(Scripted::new(download_only(false)).stdout("No update available.\n"))
            .push(status(after));
        let error = h.run(Op::Discard).await.unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::Bootc));
        assert!(
            error.message().starts_with(
                "The update channel now points at the installed version, so the update could not be cancelled."
            ),
            "{}",
            error.message()
        );
        assert!(h.grub.calls().is_empty(), "nothing disarmed");
        assert_still_armed(&h, STAGED);
        if after == &applied {
            assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(STAGED));
            assert_eq!(h.properties().state, State::ReadyForReboot);
            assert!(h.properties().last_error.contains("could not be cancelled"));
            // The tag points at the running image, as bootc said.
            assert_eq!(
                h.store.read_last_check().unwrap().remote_digest.as_deref(),
                Some(BOOTED)
            );
        }
        h.fake.assert_finished();
    }
}

#[tokio::test(start_paused = true)]
async fn discard_offline_keeps_the_update_applied_and_armed() {
    let h = Harness::with(fake_state(), armed_for(STAGED));
    applied_intent(&h);
    let applied = host(booted(), Some(staged(false)), None);
    h.push(status(&applied))
        .push(Scripted::new(download_only(false)).exit_code(1).stderr(
            "error: Pulling: dial tcp 140.82.114.34:443: connect: network is unreachable\n",
        ))
        .push(status(&applied));
    let error = h.run(Op::Discard).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Network));
    assert_eq!(
        error.message(),
        "Cancelling needs a network connection. The update is still set to install at the next restart."
    );
    assert!(h.grub.calls().is_empty());
    assert_still_armed(&h, STAGED);
    assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(STAGED));
    let properties = h.properties();
    assert_eq!(properties.state, State::ReadyForReboot);
    assert_eq!(properties.last_error, error.message());
    h.fake.assert_finished();
}

/// bootc's report and a fresh status must both say the stage is locked. With either one
/// missing, nothing is disarmed.
#[tokio::test(start_paused = true)]
async fn discard_needs_both_bootcs_report_and_the_status() {
    let busy = || {
        Scripted::new(BootcCommand::Status).error(apex_bootc::BootcError::Busy {
            detail: "lock held by another process".into(),
        })
    };
    let applied = host(booted(), Some(staged(false)), None);
    let relocked = host(booted(), Some(staged(true)), None);

    // bootc reported the stage locked again, but no status confirms it.
    let h = Harness::with(fake_state(), armed_for(STAGED));
    applied_intent(&h);
    h.push(status(&applied))
        .push(Scripted::new(download_only(false)).stdout(RELOCKED))
        .push(busy());
    let error = h.run(Op::Discard).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Busy));
    assert!(
        error.message().contains("could not be confirmed"),
        "{}",
        error.message()
    );
    assert!(h.grub.calls().is_empty());
    assert_still_armed(&h, STAGED);
    assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(STAGED));
    h.fake.assert_finished();

    // The status shows the stage locked, but bootc's output does not say so.
    let h = Harness::with(fake_state(), armed_for(STAGED));
    applied_intent(&h);
    h.push(status(&applied))
        .push(Scripted::new(download_only(false)).stdout("Something new\n"))
        .push(status(&relocked));
    let error = h.run(Op::Discard).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Bootc));
    assert!(
        error
            .message()
            .contains("did not report the update as cancelled"),
        "{}",
        error.message()
    );
    assert!(h.grub.calls().is_empty());
    assert_still_armed(&h, STAGED);
    h.fake.assert_finished();

    // bootc reported the lock, the status says the update is still applied.
    let h = Harness::with(fake_state(), armed_for(STAGED));
    applied_intent(&h);
    h.push(status(&applied))
        .push(Scripted::new(download_only(false)).stdout(RELOCKED))
        .push(status(&applied));
    let error = h.run(Op::Discard).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Bootc));
    assert!(h.grub.calls().is_empty());
    assert_still_armed(&h, STAGED);
    assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(STAGED));
    h.fake.assert_finished();
}

/// A bootc that ignored `--download-only` staged the tag's newer image unlocked: that is what
/// boots next, so the counter is armed for it, and the call fails.
#[tokio::test(start_paused = true)]
async fn discard_arms_for_another_update_bootc_set_to_install() {
    let h = Harness::with(fake_state(), armed_for(STAGED));
    applied_intent(&h);
    let newer = "45.20260925.1";
    let other = host(booted(), Some(entry(REMOTE, newer, STAGED_CSUM)), None);
    h.push(status(&host(booted(), Some(staged(false)), None)))
        .push(Scripted::new(download_only(false)).stdout(format!(
            "Queued for next boot: ostree-image-signed:docker://{STABLE_IMAGE}\n  Version: {newer}\n  Digest: {REMOTE}\n"
        )))
        .push(status(&other));
    let error = h.run(Op::Discard).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Bootc));
    assert!(error.message().contains(REMOTE), "{}", error.message());
    assert_eq!(h.grub.calls(), [arm_call(REMOTE)]);
    assert_still_armed(&h, REMOTE);
    h.fake.assert_finished();
}

// ----- Rollback and ReportBoot ---------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn rollback_marks_the_booted_digest_bad_and_disarms() {
    let h = Harness::new();
    let current = host(booted(), None, Some(rollback()));
    let mut queued = current.clone();
    queued.status.rollback_queued = true;
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::Rollback))
        .push(status(&queued));
    h.run(Op::Rollback { mark_bad: true })
        .await
        .expect("rollback");
    let denied = h.store.read_denylist().unwrap();
    assert_eq!(
        denied.get(BOOTED).map(|e| e.reason.as_str()),
        Some(MARK_BAD_REASON)
    );
    assert_eq!(h.grub.calls(), [disarm_call()]);
    assert_eq!(h.properties().state, State::RollbackQueued);

    // Asking again would swap the order back. The queued version may be older or newer
    // than the running one, so the answer names neither.
    h.push(status(&queued));
    let error = h.run(Op::Rollback { mark_bad: false }).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::NothingToDo));
    assert_eq!(
        error.message(),
        "Another version is already set to start at the next restart."
    );
    h.fake.assert_finished();
}

/// grubenv as `apex-boot-assess` leaves it when it reports a fallback from `digest`: the
/// arming, with the hedge `apex_counter=3` written just before the report.
fn hedged_for(digest: &str) -> SharedGrubWriter {
    armed_for(digest)
}

#[tokio::test(start_paused = true)]
async fn fallback_report_rolls_back_once_and_denies_the_target() {
    let h = Harness::with(fake_state(), hedged_for(STAGED));
    // GRUB fell back: the previous deployment runs, the failed target is still first.
    let mut fell_back = host(
        booted(),
        None,
        Some(entry(STAGED, STAGED_VERSION, STAGED_CSUM)),
    );
    fell_back.status.rollback_queued = true;
    let mut fixed = fell_back.clone();
    fixed.status.rollback_queued = false;
    h.store
        .write_intent(&Intent {
            digest: STAGED.into(),
            created: NOW - 600,
        })
        .unwrap();
    h.push(status(&fell_back))
        .push(Scripted::new(BootcCommand::Rollback))
        .push(status(&fixed));
    h.run(Op::Fallback {
        digest: STAGED.into(),
    })
    .await
    .expect("fallback");
    let denied = h.store.read_denylist().unwrap();
    assert_eq!(
        denied.get(STAGED).map(|e| (e.reason.as_str(), e.at)),
        Some((FALLBACK_REASON, NOW))
    );
    assert_eq!(h.intent(), None);
    // Disarmed by the daemon itself before it replied, so a lost reply cannot leave GRUB
    // counting (or choosing) the failed target; apex-boot-assess's own disarm afterwards
    // is the same unset.
    assert_eq!(h.grub.calls(), [disarm_call()]);
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), None);
    assert_eq!(h.grub.env().get_str("apex_counter").unwrap(), None);

    // The same report again (the assessment retried): no second rollback, and nothing
    // left to disarm.
    h.push(status(&fixed)).push(status(&fixed));
    h.run(Op::Fallback {
        digest: STAGED.into(),
    })
    .await
    .expect("repeated fallback");
    assert_eq!(h.store.read_denylist().unwrap().len(), 1);
    assert_eq!(
        h.fake
            .calls()
            .iter()
            .filter(|c| **c == BootcCommand::Rollback)
            .count(),
        1
    );
    assert_eq!(h.grub.calls(), [disarm_call()]);
    h.fake.assert_finished();
}

/// The reply to the first report was lost, and the assessment wrote its hedge again before
/// reporting once more: the rollback already happened, so the daemon only disarms.
#[tokio::test(start_paused = true)]
async fn a_repeated_fallback_report_disarms_a_hedge_written_again() {
    let h = Harness::with(fake_state(), hedged_for(STAGED));
    let fixed = host(
        booted(),
        None,
        Some(entry(STAGED, STAGED_VERSION, STAGED_CSUM)),
    );
    h.push(status(&fixed)).push(status(&fixed));
    h.run(Op::Fallback {
        digest: STAGED.into(),
    })
    .await
    .expect("fallback");
    assert!(!h.fake.calls().contains(&BootcCommand::Rollback));
    assert_eq!(h.grub.calls(), [disarm_call()]);
    assert_eq!(h.grub.env().get_str("apex_counter").unwrap(), None);
    h.fake.assert_finished();
}

/// The counter is disarmed only once the failed target is no longer the default: a failed
/// `bootc rollback` leaves the hedge in place, and an arming for another update is not the
/// fallback's to remove.
#[tokio::test(start_paused = true)]
async fn a_fallback_disarms_only_its_own_target_and_only_after_the_rollback() {
    let mut fell_back = host(
        booted(),
        None,
        Some(entry(STAGED, STAGED_VERSION, STAGED_CSUM)),
    );
    fell_back.status.rollback_queued = true;

    let h = Harness::with(fake_state(), hedged_for(STAGED));
    h.push(status(&fell_back))
        .push(
            Scripted::new(BootcCommand::Rollback)
                .exit_code(1)
                .stderr("error: Rollback: sysroot is busy\n"),
        )
        .push(status(&fell_back));
    assert!(
        h.run(Op::Fallback {
            digest: STAGED.into(),
        })
        .await
        .is_err()
    );
    assert!(h.grub.calls().is_empty(), "the hedge stays");
    assert_still_armed(&h, STAGED);
    h.fake.assert_finished();

    let h = Harness::with(fake_state(), armed_for(REMOTE));
    let mut fixed = fell_back.clone();
    fixed.status.rollback_queued = false;
    h.push(status(&fell_back))
        .push(Scripted::new(BootcCommand::Rollback))
        .push(status(&fixed));
    h.run(Op::Fallback {
        digest: STAGED.into(),
    })
    .await
    .expect("fallback");
    assert!(h.grub.calls().is_empty());
    assert_still_armed(&h, REMOTE);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn green_and_red_reports_update_the_health_failure() {
    let h = Harness::new();
    h.refresh(&host(booted(), None, Some(rollback()))).await;
    let engine = h.daemon.engine();

    engine
        .report_boot(BootVerdict::Red, Some(BOOTED))
        .expect("red");
    assert_eq!(h.properties().health_failure, BOOTED);
    assert_eq!(h.properties().state, State::HealthFailure);
    let record = h.store.read_health_failure().unwrap().unwrap();
    assert_eq!(record.digest, BOOTED);

    // A record apex-boot-assess wrote (with check names) is kept as it is.
    h.store
        .write_health_failure(&HealthFailureRecord {
            digest: BOOTED.into(),
            failed_checks: vec!["session".into()],
            at: 1,
        })
        .unwrap();
    engine.report_boot(BootVerdict::Red, Some(BOOTED)).unwrap();
    assert_eq!(
        h.store
            .read_health_failure()
            .unwrap()
            .unwrap()
            .failed_checks,
        ["session"]
    );

    h.store
        .write_intent(&Intent {
            digest: BOOTED.into(),
            created: NOW - 60,
        })
        .unwrap();
    engine
        .report_boot(BootVerdict::Green, Some(BOOTED))
        .expect("green");
    assert_eq!(h.properties().health_failure, "");
    assert_eq!(h.store.read_health_failure().unwrap(), None);
    assert_eq!(h.intent(), None);
    assert_eq!(h.properties().state, State::UpToDate);

    assert_eq!(
        engine
            .report_boot(BootVerdict::Red, None)
            .unwrap_err()
            .kind(),
        Some(ErrorKind::InvalidArgument)
    );
    h.fake.assert_finished();
}

// ----- Check -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn check_is_rate_limited_to_once_a_minute() {
    let h = Harness::new();
    let current = host(booted(), None, None);
    let check = || {
        Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
            "No changes in: ostree-image-signed:docker://{STABLE_IMAGE}\n"
        ))
    };
    h.push(check()).push(status(&current));
    h.run(Op::Check).await.expect("first check");
    assert_eq!(h.properties().last_check, NOW);
    assert_eq!(h.properties().state, State::UpToDate);

    // Within 60 s: answered at once, bootc is not run.
    h.system.update(|s| s.now = NOW + 59);
    h.run(Op::Check).await.expect("rate-limited check");
    assert_eq!(h.fake.calls().len(), 2);

    h.system.update(|s| s.now = NOW + 60);
    h.push(check()).push(status(&current));
    h.run(Op::Check).await.expect("check after a minute");
    assert_eq!(h.properties().last_check, NOW + 60);
    h.fake.assert_finished();
}

/// The rate limit counts successful checks only (`LastCheck`): a retry right after a failed
/// check (the network is back) must contact the registry, not report a success that never
/// happened.
#[tokio::test(start_paused = true)]
async fn a_failed_check_is_reported_and_a_retry_runs_at_once() {
    let h = Harness::new();
    let current = host(booted(), None, None);
    h.push(
        Scripted::new(BootcCommand::UpgradeCheck)
            .exit_code(1)
            .stderr("error: Fetching: dial tcp: lookup ghcr.io: no such host\n"),
    );
    assert_eq!(kind(h.run(Op::Check).await), Some(ErrorKind::Network));
    assert!(h.properties().last_error.contains("no such host"));
    assert!(!h.daemon.engine().check_is_recent());

    h.system.update(|s| s.now = NOW + 20);
    h.push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
        "No changes in: ostree-image-signed:docker://{STABLE_IMAGE}\n"
    )))
    .push(status(&current));
    h.run(Op::Check).await.expect("the retry runs");
    let properties = h.properties();
    assert_eq!(properties.last_check, NOW + 20);
    assert_eq!(properties.last_error, "", "cleared by the success");
    // Now the limit applies.
    h.run(Op::Check).await.expect("rate-limited");
    assert_eq!(
        h.fake
            .calls()
            .iter()
            .filter(|c| **c == BootcCommand::UpgradeCheck)
            .count(),
        2
    );
    h.fake.assert_finished();
}

/// Two checks at the same time query the registry once, and both callers get the result
/// of that query, a failure included.
#[tokio::test(start_paused = true)]
async fn concurrent_checks_share_one_query_and_its_result() {
    let h = Harness::new();
    h.push(
        Scripted::new(BootcCommand::UpgradeCheck)
            .exit_code(1)
            .stderr("error: Fetching: dial tcp: lookup ghcr.io: no such host\n")
            .delay(Duration::from_secs(5)),
    );
    let (first, second) = tokio::join!(h.run(Op::Check), async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        h.run(Op::Check).await
    });
    assert_eq!(kind(first), Some(ErrorKind::Network));
    assert_eq!(kind(second), Some(ErrorKind::Network));
    h.fake.assert_finished();
}

/// Regression for the remote after "no changes": bootc compares the tag with the image
/// pulled last and prints no digest, while the booted entry's `cachedUpdate` keeps
/// describing what an old check found. Download an update, then a newer one that replaces
/// it (the tag moved), restart (the locked stage is dropped), and check: the channel still
/// offers the newer one, not the one the stale `cachedUpdate` names.
#[tokio::test(start_paused = true)]
async fn after_a_replaced_download_and_a_restart_the_check_offers_what_was_pulled_last() {
    const NEWER: &str = "45.20260925.1";
    const NEWER_CSUM: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    let mut state = fake_state();
    state
        .manifests
        .insert(NEWER_CSUM.into(), manifest(NEWER, Vec::new()));
    let h = Harness::with(state, SharedGrubWriter::new());
    let available = |version: &str, digest: &str| {
        Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
            "Update available for: ostree-image-signed:docker://{STABLE_IMAGE}\n  Version: {version}\n  Digest: {digest}\n"
        ))
    };
    let staged_but_locked = |version: &str, digest: &str| {
        Scripted::new(download_only(true)).stdout(format!(
            "Staged but not queued for next boot: ostree-image-signed:docker://{STABLE_IMAGE}\n  Version: {version}\n  Digest: {digest}\n"
        ))
    };
    // bootc stored the first check's find on the commit the reference pointed at then:
    // the booted one. Nothing ever clears it.
    let mut stale_booted = booted();
    stale_booted.cached_update = Some(image(STAGED, STAGED_VERSION));
    let first = host(stale_booted.clone(), Some(staged(true)), None);
    let newer_stage = BootEntry {
        download_only: true,
        ..entry(REMOTE, NEWER, NEWER_CSUM)
    };
    let second = host(stale_booted.clone(), Some(newer_stage), None);

    // Check, then download the update.
    h.push(available(STAGED_VERSION, STAGED))
        .push(status(&host(stale_booted.clone(), None, None)));
    h.run(Op::Check).await.expect("check");
    h.push(status(&host(stale_booted.clone(), None, None)))
        .push(staged_but_locked(STAGED_VERSION, STAGED))
        .push(status(&first));
    h.run(Op::Download {
        override_soft_gates: false,
    })
    .await
    .expect("download");

    // The tag moves on: check, and download the newer build in place of the first.
    h.system.update(|s| s.now = NOW + 120);
    h.push(available(NEWER, REMOTE)).push(status(&first));
    h.run(Op::Check).await.expect("second check");
    h.push(status(&first))
        .push(staged_but_locked(NEWER, REMOTE))
        .push(status(&second));
    h.run(Op::Download {
        override_soft_gates: false,
    })
    .await
    .expect("second download");
    assert_eq!(h.properties().staged.digest.as_deref(), Some(REMOTE));

    // Restart: the locked stage is dropped. The tag still points at the newer build, which
    // is the image pulled last, so bootc says "no changes".
    let h = h.reboot(NOW + 3600);
    h.push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
        "No changes in: ostree-image-signed:docker://{STABLE_IMAGE}\n"
    )))
    .push(status(&host(stale_booted, None, None)));
    h.run(Op::Check).await.expect("check after the restart");
    let properties = h.properties();
    assert_eq!(properties.state, State::Available);
    assert_eq!(properties.available.digest.as_deref(), Some(REMOTE));
    assert_eq!(properties.available.version.as_deref(), Some(NEWER));
    assert_eq!(properties.last_check, NOW + 3600);
    let record = h.store.read_last_check().unwrap();
    assert_eq!(record.remote_digest.as_deref(), Some(REMOTE));
    assert_eq!(record.remote_version.as_deref(), Some(NEWER));
    h.fake.assert_finished();
}

/// "No changes" with nothing recorded (a fresh installation): the tag's image is unknown,
/// and a `cachedUpdate` is not taken for it.
#[tokio::test(start_paused = true)]
async fn no_changes_without_a_record_leaves_the_remote_unknown() {
    let h = Harness::new();
    let mut stale_booted = booted();
    stale_booted.cached_update = Some(image(REMOTE, "45.20260925.1"));
    h.push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
        "No changes in: ostree-image-signed:docker://{STABLE_IMAGE}\n"
    )))
    .push(status(&host(stale_booted, None, None)));
    h.run(Op::Check).await.expect("check");
    let properties = h.properties();
    assert_eq!(properties.state, State::UpToDate);
    assert!(properties.available.is_empty());
    assert_eq!(properties.last_check, NOW);
    assert_eq!(h.store.read_last_check().unwrap().remote_digest, None);
    h.fake.assert_finished();
}

// ----- the queue -------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn operations_run_one_at_a_time() {
    let h = Harness::new();
    let current = host(booted(), None, Some(rollback()));
    let mut queued = current.clone();
    queued.status.rollback_queued = true;
    // If Rollback started while Check waits for bootc, it would take the Status meant for
    // the check and then find Status, not Rollback, at the head of the script.
    h.push(
        Scripted::new(BootcCommand::UpgradeCheck)
            .stdout("No changes in: x\n")
            .delay(Duration::from_secs(5)),
    )
    .push(status(&current))
    .push(status(&current))
    .push(Scripted::new(BootcCommand::Rollback))
    .push(status(&queued));
    let start = tokio::time::Instant::now();
    let check = async {
        h.run(Op::Check).await.expect("check");
        start.elapsed()
    };
    let rollback = async {
        tokio::task::yield_now().await;
        h.run(Op::Rollback { mark_bad: false })
            .await
            .expect("rollback");
        start.elapsed()
    };
    let (check_done, rollback_done) = tokio::join!(check, rollback);
    assert!(check_done >= Duration::from_secs(5));
    assert!(rollback_done >= check_done);
    h.fake.assert_finished();
}

/// A Check during a download is `Busy`, not a success: the download may stop at a gate
/// without contacting the registry, so it answers nothing about the channel.
#[tokio::test(start_paused = true)]
async fn a_download_makes_other_operations_busy() {
    let h = Harness::new();
    h.last_check(STAGED, STAGED_VERSION, 3600);
    let current = host(booted(), None, None);
    h.push(status(&current))
        .push(Scripted::new(download_only(true)).delay(Duration::from_secs(30)))
        .push(status(&host(booted(), Some(staged(true)), None)));
    let download = h.run(Op::Download {
        override_soft_gates: false,
    });
    let others = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            h.daemon.queue().running(),
            Some(apex_updated::queue::OpKind::Download)
        );
        let apply = h
            .run(Op::Apply {
                expected_digest: STAGED.into(),
            })
            .await;
        let second = h
            .run(Op::Download {
                override_soft_gates: true,
            })
            .await;
        let switch = h.run(Op::SwitchChannel(Channel::Testing)).await;
        let check = h.run(Op::Check).await;
        (apply, second, switch, check)
    };
    let (result, (apply, second, switch, check)) = tokio::join!(download, others);
    result.expect("download");
    assert_eq!(kind(apply), Some(ErrorKind::Busy));
    assert_eq!(kind(second), Some(ErrorKind::Busy));
    assert_eq!(kind(switch), Some(ErrorKind::Busy));
    assert_eq!(kind(check), Some(ErrorKind::Busy));
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn no_status_is_read_while_the_daemons_own_bootc_command_runs() {
    let h = Harness::new();
    h.last_check(STAGED, STAGED_VERSION, 3600);
    h.push(status(&host(booted(), None, None)))
        .push(Scripted::new(download_only(true)).delay(Duration::from_secs(600)))
        .push(status(&host(booted(), Some(staged(true)), None)));
    let download = h.run(Op::Download {
        override_soft_gates: false,
    });
    let meanwhile = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let engine = h.daemon.engine();
        assert!(engine.bootc_busy());
        assert_eq!(h.daemon.queue().running(), Some(OpKind::Download));
        // Let the published state age, so every read would like a refresh.
        tokio::time::sleep(REFRESH_MAX_AGE).await;
        assert!(engine.refresh_due());
        assert!(!h.daemon.read_may_refresh());
        // What a property read and GetChanges do: nothing is queued, nothing waits.
        h.daemon.refresh_if_due();
        h.daemon.ensure_fresh().await;
        // Even a refresh that bypasses the queue does not start bootc status.
        let error = engine.refresh(&CancelToken::new()).await.unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::Busy));
        assert_eq!(error.message(), STATUS_SKIPPED);
        // The skip is not a failure: the state and LastError stay as they were.
        let properties = h.properties();
        assert_eq!(properties.state, State::Downloading);
        assert_eq!(properties.last_error, "");
        assert_eq!(h.fake.calls(), [BootcCommand::Status, download_only(true)]);
    };
    let (result, ()) = tokio::join!(download, meanwhile);
    result.expect("download");
    // The operation read the status itself once bootc had finished.
    assert!(!h.daemon.engine().bootc_busy());
    assert_eq!(h.properties().state, State::Staged);
    assert_eq!(
        h.fake.calls(),
        [
            BootcCommand::Status,
            download_only(true),
            BootcCommand::Status
        ]
    );
    h.fake.assert_finished();
}

// ----- SwitchChannel ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn switch_channel_uses_the_compile_time_reference() {
    let h = Harness::new();
    let current = host(booted(), None, None);
    let mut switched = host(
        booted(),
        Some(entry(REMOTE, STAGED_VERSION, STAGED_CSUM)),
        None,
    );
    if let Some(staged) = &mut switched.status.staged {
        staged.download_only = true;
        if let Some(image) = &mut staged.image {
            image.image.image = APEX_TESTING.to_string();
        }
    }
    switched.spec.image = switched
        .status
        .staged
        .as_ref()
        .and_then(|s| s.image.as_ref())
        .map(|i| i.image.clone());
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::SwitchDownloadOnly {
            target: APEX_TESTING,
        }))
        .push(status(&switched));
    h.run(Op::SwitchChannel(Channel::Testing))
        .await
        .expect("switch");
    // The booted deployment still follows stable until the switch is applied and booted.
    let properties = h.properties();
    assert_eq!(properties.channel, Channel::Stable);
    assert_eq!(properties.state, State::Staged);
    assert_eq!(
        h.store.read_last_check().unwrap().remote_digest.as_deref(),
        Some(REMOTE)
    );

    h.push(status(&switched));
    assert_eq!(
        kind(h.run(Op::SwitchChannel(Channel::Testing)).await),
        Some(ErrorKind::NothingToDo)
    );

    h.fake.assert_finished();

    // An unknown Fedora release has no compile-time reference.
    let fresh = Harness::with(
        {
            let mut state = fake_state();
            state.fedora = Some(46);
            if let Some(m) = &mut state.running_manifest {
                m.fedora = 46;
            }
            state
        },
        SharedGrubWriter::new(),
    );
    fresh.push(status(&current));
    assert_eq!(
        kind(fresh.run(Op::SwitchChannel(Channel::Testing)).await),
        Some(ErrorKind::PolicyDenied)
    );
    fresh.fake.assert_finished();
}

// ----- refresh, Ping and GetChanges -------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_unreadable_status_is_an_error_state_and_fails_ping() {
    let h = Harness::new();
    h.push(Scripted::new(BootcCommand::Status).stdout("{\"not\": \"bootc\"}"));
    assert_eq!(kind(h.run(Op::Refresh).await), Some(ErrorKind::Internal));
    assert_eq!(h.properties().state, State::Error);
    assert_eq!(
        h.daemon.engine().ping().await.unwrap_err().kind(),
        Some(ErrorKind::Internal)
    );

    // A good status clears it.
    h.refresh(&host(booted(), None, None)).await;
    assert_eq!(h.properties().state, State::UpToDate);
    h.daemon.engine().ping().await.expect("ping");
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_busy_sysroot_keeps_the_last_known_state() {
    let h = Harness::new();
    h.refresh(&host(booted(), Some(staged(true)), None)).await;
    h.push(
        Scripted::new(BootcCommand::Status).error(apex_bootc::BootcError::Busy {
            detail: "lock held by rpm-ostree".into(),
        }),
    );
    assert_eq!(kind(h.run(Op::Refresh).await), Some(ErrorKind::Busy));
    assert_eq!(h.properties().state, State::Staged);
    h.daemon.engine().ping().await.expect("ping");
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn changes_between_booted_and_staged_manifests() {
    let h = Harness::new();
    // Before the first refresh nothing is known: that is not "nothing staged".
    assert_eq!(
        h.daemon.engine().get_changes().await.unwrap_err().kind(),
        Some(ErrorKind::Busy)
    );
    h.refresh(&host(booted(), None, None)).await;
    assert_eq!(h.daemon.engine().get_changes().await.unwrap(), []);
    h.refresh(&host(booted(), Some(staged(true)), None)).await;
    let changes = h.daemon.engine().get_changes().await.unwrap();
    insta::assert_debug_snapshot!(changes);
    let properties = h.properties();
    assert_eq!(properties.staged.version.as_deref(), Some(STAGED_VERSION));
    assert_eq!(properties.staged.degraded, Some(vec!["audio".to_owned()]));
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn an_orphaned_intent_is_cleared_at_refresh() {
    let h = Harness::new();
    // Applied, but the stage is gone (finalization failed or it was replaced).
    h.store
        .write_intent(&Intent {
            digest: STAGED.into(),
            created: NOW - 3600,
        })
        .unwrap();
    h.system.update(|s| s.finalize_stamp = true);
    h.refresh(&host(booted(), None, None)).await;
    assert_eq!(h.intent(), None);
    assert_eq!(h.properties().state, State::FinalizeFailed);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn published_properties_snapshot() {
    let h = Harness::new();
    h.last_check(REMOTE, "45.20260924.9", 120);
    h.refresh(&host(booted(), None, Some(rollback()))).await;
    insta::assert_debug_snapshot!(h.properties());
    h.fake.assert_finished();
}

// ----- Apply: what the status says after the unlock ----------------------------------------

/// bootc unlocks the stage in one step early on. A bootc that dies afterwards (killed at
/// shutdown, a timeout) has still unlocked it: the arming protects that restart and stays,
/// and the call reports what will happen.
#[tokio::test(start_paused = true)]
async fn apply_keeps_the_arming_when_bootc_fails_after_unlocking() {
    let h = Harness::new();
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded).killed_by(15))
        .push(status(&host(booted(), Some(staged(false)), None)));
    h.run(Op::Apply {
        expected_digest: STAGED.into(),
    })
    .await
    .expect("the update is set to install at the next restart");
    assert_eq!(h.grub.calls(), [arm_call(STAGED)]);
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), Some(STAGED));
    assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(STAGED));
    assert_eq!(h.properties().state, State::ReadyForReboot);
    h.fake.assert_finished();
}

/// bootc failed and the status cannot be read: the stage may be unlocked, so the arming
/// stays (an arming whose stage does not boot is cleared by the next boot assessment).
#[tokio::test(start_paused = true)]
async fn apply_keeps_the_arming_when_the_outcome_cannot_be_confirmed() {
    let h = Harness::new();
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded).killed_by(9))
        .push(Scripted::new(BootcCommand::Status).error(BootcError::Busy {
            detail: "lock held by another process".into(),
        }));
    let error = h
        .run(Op::Apply {
            expected_digest: STAGED.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Bootc));
    assert!(
        error.message().contains("could not be confirmed"),
        "{}",
        error.message()
    );
    assert_eq!(h.grub.calls(), [arm_call(STAGED)], "nothing disarmed");
    assert_eq!(h.intent(), None);
    h.fake.assert_finished();
}

/// `SIGTERM` during `Apply` (`systemctl stop`, shutdown): closing the queue cancels bootc.
/// What the arming does then is decided by a status read with a token of its own.
#[tokio::test(start_paused = true)]
async fn a_shutdown_during_apply_decides_on_a_fresh_status() {
    for (unlocked, grub, intent) in [
        (true, vec![arm_call(STAGED)], Some(STAGED)),
        (false, vec![arm_call(STAGED), disarm_call()], None),
    ] {
        let h = Harness::new();
        h.push(status(&host(booted(), Some(staged(true)), None)))
            .push(Scripted::new(BootcCommand::UpgradeFromDownloaded).delay(Duration::from_secs(30)))
            .push(status(&host(booted(), Some(staged(!unlocked)), None)));
        let (result, ()) = tokio::join!(
            h.run(Op::Apply {
                expected_digest: STAGED.into(),
            }),
            async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                h.daemon.shutdown();
            }
        );
        assert_eq!(result.is_ok(), unlocked, "{result:?}");
        assert_eq!(h.grub.calls(), grub);
        assert_eq!(h.intent().map(|i| i.digest).as_deref(), intent);
        h.fake.assert_finished();
    }
}

/// Another root bootc run restaged between the status read and the unlock, so bootc
/// unlocked another digest. The counter follows what actually boots next, so a failure of it
/// falls back and is denylisted under its own digest; the call does not claim success.
#[tokio::test(start_paused = true)]
async fn apply_arms_for_another_digest_that_bootc_unlocked() {
    let h = Harness::new();
    let other = BootEntry {
        download_only: false,
        ..entry(REMOTE, "45.20260925.1", STAGED_CSUM)
    };
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(booted(), Some(other), None)));
    let error = h
        .run(Op::Apply {
            expected_digest: STAGED.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::DigestMismatch));
    assert!(error.message().contains(REMOTE), "{}", error.message());
    assert_eq!(h.grub.calls(), [arm_call(STAGED), arm_call(REMOTE)]);
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), Some(REMOTE));
    assert_eq!(h.intent(), None);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn apply_disarms_when_bootc_claims_success_but_nothing_is_applied() {
    let h = Harness::new();
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(booted(), Some(staged(true)), None)));
    let error = h
        .run(Op::Apply {
            expected_digest: STAGED.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Bootc));
    assert_eq!(h.grub.calls(), [arm_call(STAGED), disarm_call()]);
    assert_eq!(h.intent(), None);
    h.fake.assert_finished();
}

// ----- channel switches --------------------------------------------------------------------

/// `entry` following `reference` instead of the stable image.
fn following(mut entry: BootEntry, reference: &ImageRef) -> BootEntry {
    if let Some(image) = &mut entry.image {
        image.image.image = reference.to_string();
    }
    entry
}

/// stable and testing point at the same build (stable builds are promoted testing builds),
/// so `SwitchChannel` stages the running image under the other reference. `Apply` installs
/// it without arming: identical content has nothing to prove, and the arming precondition
/// needs a different image.
#[tokio::test(start_paused = true)]
async fn a_channel_switch_to_the_running_build_installs_without_arming() {
    let mut state = fake_state();
    // A second deployment of the booted commit: same checksum, same manifest.
    let running = state.running_manifest.clone().expect("running manifest");
    state.manifests.insert(BOOTED_CSUM.into(), running);
    let h = Harness::with(state, SharedGrubWriter::new());
    let switched = BootEntry {
        download_only: true,
        ..following(booted(), &APEX_TESTING)
    };
    let applied = BootEntry {
        download_only: false,
        ..switched.clone()
    };
    h.push(status(&host(booted(), Some(switched), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(booted(), Some(applied), None)));
    h.run(Op::Apply {
        expected_digest: BOOTED.into(),
    })
    .await
    .expect("the switch is installed at the next restart");
    assert!(
        h.grub.calls().is_empty(),
        "nothing new boots, nothing is armed"
    );
    assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(BOOTED));
    assert_eq!(h.properties().staged.download_only, Some(false));
    h.fake.assert_finished();
}

/// Switching from testing to stable, which is at an older build: the administrator
/// confirmed the switch, so the version check is waived for this stage, while every other
/// gate still applies, and the counter is armed as for any other image.
#[tokio::test(start_paused = true)]
async fn a_channel_switch_to_an_older_build_installs_behind_the_other_gates() {
    let mut state = fake_state();
    state
        .manifests
        .insert(STAGED_CSUM.into(), manifest(OLDER_VERSION, Vec::new()));
    state.boot_free = Some(100 * 1024 * 1024);
    let h = Harness::with(state, SharedGrubWriter::new());
    let testing = following(booted(), &APEX_TESTING);
    // `entry` follows the stable reference.
    let stable = BootEntry {
        download_only: true,
        ..entry(STAGED, OLDER_VERSION, STAGED_CSUM)
    };
    let apply = || Op::Apply {
        expected_digest: STAGED.into(),
    };

    h.push(status(&host(testing.clone(), Some(stable.clone()), None)));
    let error = h.run(apply()).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert!(
        error.message().contains("boot partition"),
        "{}",
        error.message()
    );
    assert!(h.grub.calls().is_empty());

    h.system.update(|s| s.boot_free = Some(600 * 1024 * 1024));
    let applied = BootEntry {
        download_only: false,
        ..stable.clone()
    };
    h.push(status(&host(testing.clone(), Some(stable), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(testing, Some(applied), None)));
    h.run(apply()).await.expect("the switch is installed");
    assert_eq!(h.grub.calls(), [arm_call(STAGED)]);
    assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(STAGED));
    h.fake.assert_finished();
}

// ----- Rollback after a failed update --------------------------------------------------------

/// After a fallback the failed update is still installed, as the rollback deployment, and
/// newer than the running one. Rolling back to it would boot a denylisted image without
/// the boot counter.
#[tokio::test(start_paused = true)]
async fn rollback_refuses_a_deployment_that_failed_before() {
    let h = Harness::new();
    let mut fell_back = host(
        booted(),
        None,
        Some(entry(STAGED, STAGED_VERSION, STAGED_CSUM)),
    );
    fell_back.status.rollback_queued = true;
    let mut fixed = fell_back.clone();
    fixed.status.rollback_queued = false;
    h.push(status(&fell_back))
        .push(Scripted::new(BootcCommand::Rollback))
        .push(status(&fixed));
    h.run(Op::Fallback {
        digest: STAGED.into(),
    })
    .await
    .expect("fallback");
    assert_eq!(
        h.properties().rollback.digest.as_deref(),
        Some(STAGED),
        "the failed update is what Rollback would start"
    );

    for mark_bad in [false, true] {
        h.push(status(&fixed));
        let error = h.run(Op::Rollback { mark_bad }).await.unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
        assert!(error.message().contains(STAGED), "{}", error.message());
        assert!(error.message().contains(FALLBACK_REASON));
    }
    assert_eq!(
        h.fake
            .calls()
            .iter()
            .filter(|c| **c == BootcCommand::Rollback)
            .count(),
        1,
        "only the fallback's own rollback ran"
    );
    assert!(h.grub.calls().is_empty());
    assert_eq!(
        h.store.read_denylist().unwrap().len(),
        1,
        "nothing else denied"
    );
    h.fake.assert_finished();
}

// ----- the first refresh ---------------------------------------------------------------------

/// Reads right after activation wait for the first refresh, whose `bootc status` takes a
/// while, instead of answering with placeholders; a first refresh that never ends delays
/// them by at most `PING_WAIT` in total.
#[tokio::test(start_paused = true)]
async fn reads_wait_for_the_first_refresh_but_not_forever() {
    let h = Harness::new();
    h.push(status(&host(booted(), Some(staged(true)), None)).delay(Duration::from_secs(3)));
    // As main does: the startup refresh is queued, not awaited.
    drop(h.daemon.queue().submit(Op::Refresh));
    let start = Instant::now();
    h.daemon.engine().first_refresh().await;
    assert!(start.elapsed() >= Duration::from_secs(3));
    assert_eq!(h.properties().state, State::Staged);
    assert!(!h.daemon.engine().get_changes().await.unwrap().is_empty());
    h.daemon.ping().await.expect("ping");
    h.fake.assert_finished();

    // The sysroot lock is held: the first status does not return.
    let h = Harness::new();
    h.push(status(&host(booted(), None, None)).delay(Duration::from_secs(600)));
    drop(h.daemon.queue().submit(Op::Refresh));
    let start = Instant::now();
    h.daemon.engine().first_refresh().await;
    let waited = start.elapsed();
    assert!(waited >= PING_WAIT && waited < PING_WAIT + Duration::from_secs(1));
    assert_eq!(h.properties().state, State::Unknown);
    assert_eq!(
        h.daemon.engine().get_changes().await.unwrap_err().kind(),
        Some(ErrorKind::Busy)
    );
    // The deadline is absolute: later reads do not wait again.
    h.daemon.engine().first_refresh().await;
    assert_eq!(start.elapsed(), waited);
    h.daemon.shutdown();
}

// ----- Apply requests that cannot succeed ------------------------------------------------------

/// Any active-session process may call `Apply` without a prompt; a digest the fresh state
/// refutes is answered at once, without queueing a full refresh.
#[tokio::test(start_paused = true)]
async fn an_apply_the_fresh_state_refutes_never_reaches_the_queue() {
    let h = Harness::new();
    let engine = h.daemon.engine();
    engine
        .precheck_apply(REMOTE)
        .expect("nothing known: the queued operation decides");
    h.refresh(&host(booted(), Some(staged(true)), None)).await;
    assert_eq!(
        engine.precheck_apply(REMOTE).unwrap_err().kind(),
        Some(ErrorKind::DigestMismatch)
    );
    engine.precheck_apply(STAGED).expect("the staged digest");
    tokio::time::advance(REFRESH_MAX_AGE).await;
    engine
        .precheck_apply(REMOTE)
        .expect("an old state is not trusted");
    h.refresh(&host(booted(), Some(staged(false)), None)).await;
    assert_eq!(
        engine.precheck_apply(STAGED).unwrap_err().kind(),
        Some(ErrorKind::NothingToDo)
    );
    h.refresh(&host(booted(), None, None)).await;
    assert_eq!(
        engine.precheck_apply(STAGED).unwrap_err().kind(),
        Some(ErrorKind::NothingToDo)
    );
    h.fake.assert_finished();
}

// ----- text from outside -----------------------------------------------------------------------

/// A registry (or a proxy) controls part of bootc's error line. NUL would make the
/// property undecodable for zbus clients; CR and C1 controls would rewrite the terminal.
#[tokio::test(start_paused = true)]
async fn text_from_bootc_reaches_last_error_without_control_characters() {
    let h = Harness::new();
    h.push(
        Scripted::new(BootcCommand::UpgradeCheck)
            .exit_code(1)
            .stderr("error: Fetching: manifest unknown: x\0\r\u{9b}2J\u{7}y\n"),
    );
    let error = h.run(Op::Check).await.unwrap_err();
    assert!(
        error.message().contains('\0'),
        "the engine keeps the raw text"
    );
    let last_error = h.properties().last_error;
    assert!(last_error.contains("manifest unknown"), "{last_error:?}");
    assert!(!last_error.chars().any(char::is_control), "{last_error:?}");
    h.fake.assert_finished();
}

// ----- state files whose directory flush fails --------------------------------------------------

/// The real state directory, except that every write and removal reports
/// `StoreError::NotFlushed` after it happened: the rename (or removal) is done and visible,
/// only the directory `fsync` failed.
#[derive(Debug)]
struct UnflushedStore(StateDir);

fn unflushed(name: &str) -> StoreError {
    StoreError::NotFlushed {
        path: name.into(),
        source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
    }
}

impl StateStore for UnflushedStore {
    fn read_intent(&self) -> Result<Option<Intent>, StoreError> {
        self.0.read_intent()
    }
    fn write_intent(&self, intent: &Intent) -> Result<(), StoreError> {
        self.0.write_intent(intent)?;
        Err(unflushed("intent.json"))
    }
    fn clear_intent(&self) -> Result<(), StoreError> {
        self.0.clear_intent()?;
        Err(unflushed("intent.json"))
    }
    fn read_denylist(&self) -> Result<apex_update::Denylist, StoreError> {
        self.0.read_denylist()
    }
    fn write_denylist(&self, denylist: &apex_update::Denylist) -> Result<(), StoreError> {
        self.0.write_denylist(denylist)?;
        Err(unflushed("denylist.json"))
    }
    fn read_last_check(&self) -> Result<apex_update::LastCheck, StoreError> {
        self.0.read_last_check()
    }
    fn write_last_check(&self, check: &apex_update::LastCheck) -> Result<(), StoreError> {
        self.0.write_last_check(check)?;
        Err(unflushed("last-check.json"))
    }
    fn read_health_failure(&self) -> Result<Option<HealthFailureRecord>, StoreError> {
        self.0.read_health_failure()
    }
    fn clear_health_failure(&self) -> Result<(), StoreError> {
        self.0.clear_health_failure()?;
        Err(unflushed("health-failure.json"))
    }
}

impl apex_updated::store::Store for UnflushedStore {
    fn write_health_failure(&self, record: &HealthFailureRecord) -> Result<(), StoreError> {
        self.0.write_health_failure(record)?;
        Err(unflushed("health-failure.json"))
    }
}

/// A change whose directory flush failed is made (readers see it), so the daemon goes on as
/// if it had been saved and only warns: marking a version bad does not stop the rollback,
/// and acknowledging or reporting a boot does not fail.
#[tokio::test(start_paused = true)]
async fn a_change_whose_directory_flush_failed_counts_as_made() {
    let dir = tempfile::tempdir().unwrap();
    let files = StateDir::new(dir.path());
    let fake = Arc::new(apex_bootc::FakeRunner::new());
    let system = FakeSystem::new(fake_state());
    let grub = SharedGrubWriter::new();
    let daemon = Daemon::new(Engine::new(EngineParts {
        runner: Arc::clone(&fake),
        store: Arc::new(UnflushedStore(files.clone())),
        grub: grub.boxed(),
        grubenv: grub.source(),
        providers: system.providers(),
    }));
    daemon.spawn_worker();

    let current = host(booted(), None, Some(rollback()));
    let mut queued = current.clone();
    queued.status.rollback_queued = true;
    fake.push(status(&current))
        .push(Scripted::new(BootcCommand::Rollback))
        .push(status(&queued));
    daemon
        .submit(Op::Rollback { mark_bad: true })
        .await
        .expect("the denylist entry is saved, so the rollback runs");
    assert!(files.read_denylist().unwrap().contains(BOOTED));
    assert_eq!(daemon.engine().properties().state, State::RollbackQueued);

    files
        .write_health_failure(&HealthFailureRecord {
            digest: BOOTED.into(),
            failed_checks: Vec::new(),
            at: NOW,
        })
        .unwrap();
    daemon
        .engine()
        .acknowledge_health_failure()
        .expect("the record is gone");
    assert_eq!(files.read_health_failure().unwrap(), None);
    daemon
        .engine()
        .report_boot(BootVerdict::Red, Some(BOOTED))
        .expect("the record is written");
    assert_eq!(daemon.engine().properties().health_failure, BOOTED);
    daemon
        .engine()
        .report_boot(BootVerdict::Green, Some(BOOTED))
        .expect("the record is gone");
    assert_eq!(files.read_health_failure().unwrap(), None);
    fake.assert_finished();
}

// ----- a panicking operation -------------------------------------------------------------------

/// A runner with a bug.
#[derive(Debug)]
struct PanickingRunner;

impl CommandRunner for PanickingRunner {
    async fn run(&self, _invocation: Invocation) -> Result<CommandOutput, BootcError> {
        // Part-way through the operation, as a real bug would be.
        tokio::task::yield_now().await;
        panic!("a bug in an operation")
    }
}

/// A panic ends the worker. Nothing may then wait forever: callers of the panicking
/// operation (and those that joined it) get an error, new work fails at once, `Ping`
/// fails, and the worker's handle completes, which `main` treats as fatal.
#[tokio::test(start_paused = true)]
async fn a_panicking_operation_stops_the_service_instead_of_wedging_it() {
    let dir = tempfile::tempdir().unwrap();
    let system = FakeSystem::new(fake_state());
    let daemon = Daemon::new(Engine::new(EngineParts {
        runner: PanickingRunner,
        store: Arc::new(StateDir::new(dir.path())),
        grub: SharedGrubWriter::new().boxed(),
        grubenv: SharedGrubWriter::new().source(),
        providers: system.providers(),
    }));
    let worker = daemon.spawn_worker();
    assert!(daemon.worker_alive());
    let (first, joined) = tokio::join!(daemon.submit(Op::Check), daemon.submit(Op::Check));
    for result in [first, joined] {
        assert_eq!(result.unwrap_err().kind(), Some(ErrorKind::Internal));
    }
    assert!(worker.await.unwrap_err().is_panic());
    assert!(!daemon.worker_alive());
    assert_eq!(
        daemon
            .submit(Op::Rollback { mark_bad: false })
            .await
            .unwrap_err()
            .kind(),
        Some(ErrorKind::Busy)
    );
    assert_eq!(
        daemon.ping().await.unwrap_err().kind(),
        Some(ErrorKind::Internal)
    );
}
