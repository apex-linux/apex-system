#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test: a failed expectation is the test failure"
)]

mod common;

use apex_bootc::{BootcCommand, Scripted};
use apex_dbus::{BootVerdict, ErrorKind, Hold, State, Stock};
use apex_update::{HealthFailureRecord, StateStore as _};
use apex_updated::queue::Op;
use apex_updated::store::Store as _;
use common::*;

fn check_output(digest: &str) -> String {
    format!(
        "Update available for: ostree-image-signed:docker://{STABLE_IMAGE}\n  Version: \
         {STAGED_VERSION}\n  Digest: {digest}\nTotal new layers: 3    Size: 1.2 GB\n\
         Removed layers:   2    Size: 1.1 GB\n"
    )
}

#[tokio::test(start_paused = true)]
async fn with_automatic_downloads_off_the_automatic_run_only_checks() {
    let h = Harness::new();
    let current = host(booted(), None, None);
    h.refresh(&current).await;
    h.daemon
        .engine()
        .set_automatic_downloads(false)
        .expect("set");
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::UpgradeCheck).stdout(check_output(REMOTE)))
        .push(status(&current));
    h.run(Op::Automatic).await.expect("automatic");
    let properties = h.properties();
    assert_eq!(properties.state, State::Available);
    assert!(!properties.automatic_downloads);
    assert_eq!(properties.available.digest.as_deref(), Some(REMOTE));
    assert_eq!(properties.available.download_size, Some(1_200_000_000));
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn the_automatic_downloads_setting_survives_a_restart() {
    let h = Harness::new();
    let current = host(booted(), None, None);
    h.refresh(&current).await;
    assert!(h.properties().automatic_downloads, "on by default");
    h.daemon
        .engine()
        .set_automatic_downloads(false)
        .expect("set");
    assert!(!h.store.read_settings().unwrap().automatic_downloads);

    let h = h.reboot(NOW + 60);
    h.refresh(&current).await;
    assert!(!h.properties().automatic_downloads);
    h.daemon
        .engine()
        .set_automatic_downloads(true)
        .expect("set");
    assert!(h.properties().automatic_downloads);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn holds_and_download_size_are_in_the_dicts() {
    let mut state = fake_state();
    state.running_lane_lock = Some(
        "family gnome-shell held k1 since=2026-10-12T04:10:00Z newer=gnome-shell-51.1-1.fc45 \
         security=1 side=kernel reason=r\nfamily nautilus built k2\n\
         family kernel-cachyos stock - reason=break-glass: Fedora's kernel\n"
            .to_owned(),
    );
    state.lane_locks.insert(
        STAGED_CSUM.to_owned(),
        "family libadwaita built k3\n".to_owned(),
    );
    let h = Harness::with(state, apex_updated::testing::SharedGrubWriter::new());
    let current = host(booted(), None, Some(rollback()));
    h.refresh(&current).await;
    h.push(Scripted::new(BootcCommand::UpgradeCheck).stdout(check_output(STAGED)))
        .push(status(&current));
    h.run(Op::Check).await.expect("check");
    let properties = h.properties();
    assert_eq!(
        properties.booted.holds,
        Some(vec![Hold {
            family: "gnome-shell".to_owned(),
            newer: "gnome-shell-51.1-1.fc45".to_owned(),
            since: 1_791_778_200,
            security: true,
            limit: 0,
            side: "kernel".to_owned(),
        }])
    );
    assert_eq!(
        properties.booted.stock,
        Some(vec![Stock {
            family: "kernel-cachyos".to_owned(),
            reason: "break-glass: Fedora's kernel".to_owned(),
        }])
    );
    assert_eq!(
        properties.rollback.holds, None,
        "no lane lock in that image"
    );
    assert_eq!(properties.rollback.stock, None);
    assert_eq!(properties.booted.download_size, None, "booted has no size");
    assert_eq!(properties.available.download_size, Some(1_200_000_000));

    h.refresh(&host(booted(), Some(staged(true)), Some(rollback())))
        .await;
    let properties = h.properties();
    assert_eq!(
        properties.staged.holds,
        Some(Vec::new()),
        "a lock, nothing held"
    );
    assert_eq!(properties.staged.stock, Some(Vec::new()));
    assert_eq!(
        properties.staged.download_size,
        Some(1_200_000_000),
        "the stage is the image the check measured"
    );
    assert_eq!(properties.available.digest, None);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_stage_the_check_did_not_measure_reports_zero() {
    let h = Harness::new();
    h.refresh(&host(booted(), Some(staged(true)), None)).await;
    assert_eq!(h.properties().staged.download_size, Some(0));
    assert_eq!(h.properties().booted.holds, None, "no lane lock at all");
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn last_fallback_is_set_kept_and_cleared() {
    let h = Harness::new();
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
    assert_eq!(h.properties().last_fallback, STAGED);
    assert_eq!(
        h.store.read_fallback().unwrap().map(|r| (r.digest, r.at)),
        Some((STAGED.to_owned(), NOW))
    );

    let h = h.reboot(NOW + 60);
    h.refresh(&fixed).await;
    assert_eq!(
        h.properties().last_fallback,
        STAGED,
        "kept across a restart"
    );
    h.daemon
        .engine()
        .acknowledge_health_failure()
        .expect("acknowledge");
    assert_eq!(h.properties().last_fallback, "");
    assert_eq!(h.store.read_fallback().unwrap(), None);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn changes_for_the_staged_or_the_rollback_deployment() {
    let h = Harness::new();
    let engine = h.daemon.engine();
    assert_eq!(
        engine.get_changes_for(STAGED).await.unwrap_err().kind(),
        Some(ErrorKind::Busy),
        "the state is unknown"
    );
    h.refresh(&host(booted(), Some(staged(true)), Some(rollback())))
        .await;
    assert_eq!(
        engine.get_changes_for(STAGED).await.unwrap(),
        engine.get_changes().await.unwrap()
    );
    let removed: Vec<(String, String)> = engine
        .get_changes_for(ROLLBACK)
        .await
        .unwrap()
        .into_iter()
        .map(|(name, _, kind, _, _)| (name, kind))
        .collect();
    assert_eq!(
        removed,
        [
            ("bash".to_owned(), "removed".to_owned()),
            ("kernel".to_owned(), "removed".to_owned()),
        ],
        "the rollback image lists no packages"
    );
    assert_eq!(engine.get_changes_for(BOOTED).await.unwrap(), []);
    let error = engine.get_changes_for(REMOTE).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::InvalidArgument));
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn health_failure_checks_come_from_the_record_and_clear_with_it() {
    let h = Harness::new();
    let current = host(booted(), None, Some(rollback()));
    h.refresh(&current).await;
    let record = |digest: &str| HealthFailureRecord {
        digest: digest.into(),
        failed_checks: vec!["session".into(), "audio".into()],
        at: NOW,
    };
    h.store.write_health_failure(&record(BOOTED)).unwrap();
    let engine = h.daemon.engine();
    engine
        .report_boot(BootVerdict::Red, Some(BOOTED))
        .expect("red");
    assert_eq!(h.properties().health_failure_checks, ["session", "audio"]);

    let h = h.reboot(NOW + 60);
    h.refresh(&current).await;
    assert_eq!(
        h.properties().health_failure_checks,
        ["session", "audio"],
        "read back at refresh"
    );
    h.daemon
        .engine()
        .report_boot(BootVerdict::Green, Some(BOOTED))
        .expect("green");
    assert!(h.properties().health_failure_checks.is_empty());

    h.store.write_health_failure(&record(ROLLBACK)).unwrap();
    h.daemon
        .engine()
        .report_boot(BootVerdict::Red, Some(BOOTED))
        .expect("red");
    assert_eq!(h.properties().health_failure, BOOTED);
    assert!(
        h.properties().health_failure_checks.is_empty(),
        "the record named another digest"
    );
    assert_eq!(
        h.store.read_health_failure().unwrap().map(|r| r.digest),
        Some(BOOTED.to_owned())
    );
    h.daemon
        .engine()
        .acknowledge_health_failure()
        .expect("acknowledge");
    assert!(h.properties().health_failure_checks.is_empty());
    h.fake.assert_finished();
}
