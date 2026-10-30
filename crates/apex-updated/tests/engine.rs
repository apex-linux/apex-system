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
    BootEntry, BootcCommand, BootcError, CancelToken, CommandOutput, CommandRunner, Invocation,
    Scripted,
};
use apex_dbus::{BootVerdict, ErrorKind, State};
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

/// Spelled out, not taken from apex-boot, so a rename on either side fails here.
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

    h.push(status(&before));
    let error = h
        .run(Op::Download {
            override_soft_gates: false,
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert_eq!(error.message(), properties.deferred_reason);

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
    // Power already fails, so no download follows the check.
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

const MOVED_CSUM: &str = "6666666666666666666666666666666666666666666666666666666666666666";

fn with_moved_build(version: &str) -> Harness {
    let mut state = fake_state();
    state
        .manifests
        .insert(MOVED_CSUM.into(), manifest(version, Vec::new()));
    Harness::with(state, SharedGrubWriter::new())
}

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

const RELOCKED: &str = "Image downloaded, but will not be applied on reboot\n";

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

fn assert_still_armed(h: &Harness, digest: &str) {
    let env = h.grub.env();
    assert_eq!(env.get_str("apex_target").unwrap(), Some(digest));
    assert_eq!(env.get_str("apex_counter").unwrap(), Some("3"));
}

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

    h.push(status(&queued));
    let error = h.run(Op::Rollback { mark_bad: false }).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::NothingToDo));
    assert_eq!(
        error.message(),
        "Another version is already set to start at the next restart."
    );
    h.fake.assert_finished();
}

/// apex-boot-assess writes the hedge `apex_counter=3` before reporting: the full arming.
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
    assert_eq!(h.grub.calls(), [disarm_call()]);
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), None);
    assert_eq!(h.grub.env().get_str("apex_counter").unwrap(), None);

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

#[tokio::test(start_paused = true)]
async fn every_check_reaches_the_registry() {
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

    h.system.update(|s| s.now = NOW + 1);
    h.push(check()).push(status(&current));
    h.run(Op::Check).await.expect("second check");
    assert_eq!(h.properties().last_check, NOW + 1);
    h.fake.assert_finished();
}

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

    h.system.update(|s| s.now = NOW + 20);
    h.push(Scripted::new(BootcCommand::UpgradeCheck).stdout(format!(
        "No changes in: ostree-image-signed:docker://{STABLE_IMAGE}\n"
    )))
    .push(status(&current));
    h.run(Op::Check).await.expect("the retry runs");
    let properties = h.properties();
    assert_eq!(properties.last_check, NOW + 20);
    assert_eq!(properties.last_error, "", "cleared by the success");
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

/// For "no changes" bootc prints no digest, while the booted entry's `cachedUpdate` still
/// names what an old check found.
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
    // bootc stored the first check's find on the booted commit and never clears it.
    let mut stale_booted = booted();
    stale_booted.cached_update = Some(image(STAGED, STAGED_VERSION));
    let first = host(stale_booted.clone(), Some(staged(true)), None);
    let newer_stage = BootEntry {
        download_only: true,
        ..entry(REMOTE, NEWER, NEWER_CSUM)
    };
    let second = host(stale_booted.clone(), Some(newer_stage), None);

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

    // The restart drops the locked stage; the tag still points at the image pulled last.
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

#[tokio::test(start_paused = true)]
async fn operations_run_one_at_a_time() {
    let h = Harness::new();
    let current = host(booted(), None, Some(rollback()));
    let mut queued = current.clone();
    queued.status.rollback_queued = true;
    // A Rollback started during the check would take the check's Status and then fail on
    // the script's order.
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
        let check = h.run(Op::Check).await;
        (apply, second, check)
    };
    let (result, (apply, second, check)) = tokio::join!(download, others);
    result.expect("download");
    assert_eq!(kind(apply), Some(ErrorKind::Busy));
    assert_eq!(kind(second), Some(ErrorKind::Busy));
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
        // Aged, so every read would like a refresh.
        tokio::time::sleep(REFRESH_MAX_AGE).await;
        assert!(engine.refresh_due());
        assert!(!h.daemon.read_may_refresh());
        h.daemon.refresh_if_due();
        h.daemon.ensure_fresh().await;
        let error = engine.refresh(&CancelToken::new()).await.unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::Busy));
        assert_eq!(error.message(), STATUS_SKIPPED);
        let properties = h.properties();
        assert_eq!(properties.state, State::Downloading);
        assert_eq!(properties.last_error, "");
        assert_eq!(h.fake.calls(), [BootcCommand::Status, download_only(true)]);
    };
    let (result, ()) = tokio::join!(download, meanwhile);
    result.expect("download");
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
    assert_eq!(
        properties.staged.held,
        Some(vec!["gnome-shell".to_owned(), "nautilus".to_owned()]),
        "sorted and deduplicated"
    );
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

/// Every write and removal happens, then reports `NotFlushed`.
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
    fn read_settings(&self) -> Result<apex_update::UpdateSettings, StoreError> {
        self.0.read_settings()
    }
    fn write_settings(&self, settings: &apex_update::UpdateSettings) -> Result<(), StoreError> {
        self.0.write_settings(settings)?;
        Err(unflushed("settings.json"))
    }
    fn read_fallback(&self) -> Result<Option<apex_update::FallbackRecord>, StoreError> {
        self.0.read_fallback()
    }
    fn write_fallback(&self, record: &apex_update::FallbackRecord) -> Result<(), StoreError> {
        self.0.write_fallback(record)?;
        Err(unflushed("last-fallback.json"))
    }
    fn clear_fallback(&self) -> Result<(), StoreError> {
        self.0.clear_fallback()?;
        Err(unflushed("last-fallback.json"))
    }
    fn read_pins(&self) -> Result<apex_update::Pins, StoreError> {
        self.0.read_pins()
    }
    fn write_pins(&self, pins: &apex_update::Pins) -> Result<(), StoreError> {
        self.0.write_pins(pins)?;
        Err(unflushed("pinned.json"))
    }
    fn read_major_upgrades(&self) -> Result<apex_update::MajorUpgrades, StoreError> {
        self.0.read_major_upgrades()
    }
    fn write_major_upgrades(&self, record: &apex_update::MajorUpgrades) -> Result<(), StoreError> {
        self.0.write_major_upgrades(record)?;
        Err(unflushed("major-upgrade.json"))
    }
}

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

#[derive(Debug)]
struct PanickingRunner;

impl CommandRunner for PanickingRunner {
    async fn run(&self, _invocation: Invocation) -> Result<CommandOutput, BootcError> {
        // Part-way through the operation, as a real bug would be.
        tokio::task::yield_now().await;
        panic!("a bug in an operation")
    }
}

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
