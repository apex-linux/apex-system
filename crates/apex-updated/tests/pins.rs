#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests: a failed expectation is the test failure"
)]

mod common;

use std::fmt::Write as _;

use apex_boot::writer::EditenvCall;
use apex_bootc::{BootEntry, BootcCommand, Scripted};
use apex_dbus::ErrorKind;
use apex_update::{HealthFailureRecord, PinRecord, Pins, UpdateSettings};
use apex_updated::engine::PIN_REASON;
use apex_updated::queue::Op;
use apex_updated::store::Store as _;
use apex_updated::testing::SharedGrubWriter;
use common::*;

/// Spelled out, not taken from apex-boot, so a rename on either side fails here.
fn arm_call(digest: &str) -> EditenvCall {
    EditenvCall::Set(vec![
        ("apex_target".into(), digest.into()),
        ("apex_counter".into(), "3".into()),
        ("apex_ok".into(), "0".into()),
    ])
}

fn apply() -> Op {
    Op::Apply {
        expected_digest: STAGED.into(),
    }
}

fn pinned(entry: BootEntry) -> BootEntry {
    BootEntry {
        pinned: true,
        ..entry
    }
}

/// Still armed for the running version: its startup check has not passed.
fn unconfirmed() -> SharedGrubWriter {
    grubenv_with(&[
        ("apex_target", BOOTED),
        ("apex_counter", "2"),
        ("apex_ok", "0"),
    ])
}

fn rollback_record() -> PinRecord {
    PinRecord {
        checksum: ROLLBACK_CSUM.into(),
        serial: 0,
        digest: ROLLBACK.into(),
        at: NOW,
    }
}

#[tokio::test(start_paused = true)]
async fn apply_over_an_unconfirmed_boot_pins_the_last_good_version_then_arms_then_applies() {
    let h = Harness::with(fake_state(), unconfirmed());
    h.push(status(&host(
        booted(),
        Some(staged(true)),
        Some(rollback()),
    )))
    .push(Scripted::new(BootcCommand::PinRollback))
    .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
    .push(status(&host(
        booted(),
        Some(staged(false)),
        Some(pinned(rollback())),
    )));
    h.run(apply()).await.expect("apply");
    assert_eq!(
        h.fake.calls(),
        [
            BootcCommand::Status,
            BootcCommand::PinRollback,
            BootcCommand::UpgradeFromDownloaded,
            BootcCommand::Status,
        ]
    );
    assert_eq!(h.grub.calls(), [arm_call(STAGED)]);
    assert_eq!(
        h.store.read_pins().unwrap(),
        Pins {
            pins: vec![rollback_record()]
        }
    );
    let properties = h.properties();
    assert_eq!(properties.rollback.pinned, Some(true));
    assert_eq!(properties.rollback.pin_reason.as_deref(), Some(PIN_REASON));
    assert_eq!(properties.booted.pin_reason, None);
    assert!(h.intent().is_some());
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn the_pin_comes_before_the_arming() {
    let h = Harness::with(fake_state(), SharedGrubWriter::failing_at(0));
    h.push(status(&host(
        booted(),
        Some(staged(true)),
        Some(rollback()),
    )))
    .push(Scripted::new(BootcCommand::PinRollback));
    let error = h.run(apply()).await.unwrap_err();
    assert!(
        error.message().contains("boot counter could not be armed"),
        "{}",
        error.message()
    );
    assert_eq!(
        h.fake.calls(),
        [BootcCommand::Status, BootcCommand::PinRollback],
        "pinned, then the arming failed, so nothing was applied"
    );
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn apply_over_a_failed_boot_pins_too() {
    let h = Harness::new();
    h.store
        .write_health_failure(&HealthFailureRecord {
            digest: BOOTED.into(),
            failed_checks: vec!["session".into()],
            at: NOW - 60,
        })
        .unwrap();
    h.push(status(&host(
        booted(),
        Some(staged(true)),
        Some(rollback()),
    )))
    .push(Scripted::new(BootcCommand::PinRollback))
    .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
    .push(status(&host(
        booted(),
        Some(staged(false)),
        Some(pinned(rollback())),
    )));
    h.run(apply()).await.expect("apply");
    assert_eq!(h.store.read_pins().unwrap().pins, [rollback_record()]);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_failed_pin_fails_apply_before_anything_is_armed() {
    let h = Harness::with(fake_state(), unconfirmed());
    h.push(status(&host(
        booted(),
        Some(staged(true)),
        Some(rollback()),
    )))
    .push(
        Scripted::new(BootcCommand::PinRollback)
            .exit_code(1)
            .stderr("error: Deployment type not found: rollback\n"),
    );
    let error = h.run(apply()).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::Bootc));
    assert!(
        error
            .message()
            .starts_with("The last version known to work could not be kept"),
        "{}",
        error.message()
    );
    assert!(error.message().contains("ostree admin pin rollback"));
    assert!(h.grub.calls().is_empty(), "nothing armed");
    assert_eq!(h.grub.env().get_str("apex_target").unwrap(), Some(BOOTED));
    assert_eq!(h.store.read_pins().unwrap(), Pins::default());
    assert_eq!(h.intent(), None);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn without_a_rollback_apply_logs_and_goes_on() {
    let h = Harness::with(fake_state(), unconfirmed());
    h.push(status(&host(booted(), Some(staged(true)), None)))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(booted(), Some(staged(false)), None)));
    h.run(apply()).await.expect("apply");
    assert_eq!(h.grub.calls(), [arm_call(STAGED)]);
    assert_eq!(h.store.read_pins().unwrap(), Pins::default());
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_rollback_pinned_already_is_not_pinned_again_or_recorded() {
    let h = Harness::with(fake_state(), unconfirmed());
    let ready = host(booted(), Some(staged(true)), Some(pinned(rollback())));
    h.push(status(&ready))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(
            booted(),
            Some(staged(false)),
            Some(pinned(rollback())),
        )));
    h.run(apply()).await.expect("apply");
    assert_eq!(h.store.read_pins().unwrap(), Pins::default());
    let properties = h.properties();
    assert_eq!(properties.rollback.pinned, Some(true));
    assert_eq!(properties.rollback.pin_reason, None, "the owner's pin");
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_boot_needs_no_pin() {
    let h = Harness::new();
    h.push(status(&host(
        booted(),
        Some(staged(true)),
        Some(rollback()),
    )))
    .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
    .push(status(&host(
        booted(),
        Some(staged(false)),
        Some(rollback()),
    )));
    h.run(apply()).await.expect("apply");
    assert_eq!(h.grub.calls(), [arm_call(STAGED)]);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_startup_check_still_refuses() {
    let h = Harness::with(
        fake_state(),
        grubenv_with(&[("apex_target", BOOTED), ("apex_counter", "two")]),
    );
    h.push(status(&host(
        booted(),
        Some(staged(true)),
        Some(rollback()),
    )));
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

const NEW: &str = "sha256:6666666666666666666666666666666666666666666666666666666666666666";
const NEW_CSUM: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const OWNER_CSUM: &str = "5555555555555555555555555555555555555555555555555555555555555555";

/// `ostree admin status` with the staged deployment, if any, first.
fn listing(rows: &[(&str, u32, bool)]) -> Scripted {
    let mut text = String::new();
    for (index, (checksum, serial, pinned)) in rows.iter().enumerate() {
        let marker = if index == 0 { '*' } else { ' ' };
        writeln!(text, "{marker} default {checksum}.{serial}").unwrap();
        text.push_str("    Version: 45.1\n");
        if *pinned {
            text.push_str("    Pinned: yes\n");
        }
        text.push_str("    origin: <unknown origin type>\n");
    }
    Scripted::new(BootcCommand::OstreeStatus).stdout(text)
}

/// Booted the update that came after the pin; the kept version is now third in line.
fn after_the_update() -> apex_bootc::Host {
    host(entry(NEW, STAGED_VERSION, NEW_CSUM), None, Some(booted()))
}

fn check_output() -> String {
    format!(
        "Update available for: ostree-image-signed:docker://{STABLE_IMAGE}\n  Version: \
         45.20260930.1\n  Digest: {REMOTE}\nTotal new layers: 3    Size: 1.2 GB\n"
    )
}

/// The daily run, with automatic downloads off so it ends after the check.
fn harness_for_the_daily_run(grub: SharedGrubWriter, pins: Vec<PinRecord>) -> Harness {
    let h = Harness::with(fake_state(), grub);
    h.store
        .write_settings(&UpdateSettings {
            automatic_downloads: false,
        })
        .unwrap();
    h.store.write_pins(&Pins { pins }).unwrap();
    h
}

fn push_the_check(h: &Harness, current: &apex_bootc::Host) {
    h.push(Scripted::new(BootcCommand::UpgradeCheck).stdout(check_output()))
        .push(status(current));
}

#[tokio::test(start_paused = true)]
async fn the_daily_run_unpins_the_kept_version_once_a_later_one_is_confirmed() {
    let owner_pin = (OWNER_CSUM, 0, true);
    let h = harness_for_the_daily_run(grubenv_with(&[("apex_ok", "1")]), vec![rollback_record()]);
    let current = after_the_update();
    h.push(status(&current))
        .push(listing(&[
            (NEW_CSUM, 0, false),
            (BOOTED_CSUM, 0, false),
            (ROLLBACK_CSUM, 0, true),
            owner_pin,
        ]))
        .push(Scripted::new(BootcCommand::Unpin { index: 2 }))
        .push(status(&current));
    push_the_check(&h, &current);
    h.run(Op::Automatic).await.expect("automatic");
    assert_eq!(h.store.read_pins().unwrap(), Pins::default());
    assert!(
        !h.fake.calls().contains(&BootcCommand::Unpin { index: 3 }),
        "the owner's pin stays"
    );
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn the_kept_version_stays_pinned_until_a_later_one_is_confirmed() {
    let h = harness_for_the_daily_run(
        grubenv_with(&[
            ("apex_target", NEW),
            ("apex_counter", "2"),
            ("apex_ok", "0"),
        ]),
        vec![rollback_record()],
    );
    let current = after_the_update();
    h.push(status(&current));
    push_the_check(&h, &current);
    h.run(Op::Automatic).await.expect("automatic");
    assert_eq!(h.store.read_pins().unwrap().pins, [rollback_record()]);
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn the_kept_version_stays_pinned_while_it_is_the_rollback() {
    let h = harness_for_the_daily_run(grubenv_with(&[("apex_ok", "1")]), vec![rollback_record()]);
    let current = host(booted(), None, Some(pinned(rollback())));
    h.push(status(&current));
    push_the_check(&h, &current);
    h.run(Op::Automatic).await.expect("automatic");
    assert_eq!(h.store.read_pins().unwrap().pins, [rollback_record()]);
    assert_eq!(
        h.properties().rollback.pin_reason.as_deref(),
        Some(PIN_REASON)
    );
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_pin_the_owner_removed_or_a_deleted_deployment_is_forgotten() {
    let gone = PinRecord {
        checksum: OWNER_CSUM.into(),
        ..rollback_record()
    };
    let h = harness_for_the_daily_run(
        grubenv_with(&[("apex_ok", "1")]),
        vec![rollback_record(), gone],
    );
    let current = after_the_update();
    h.push(status(&current))
        .push(listing(&[
            (NEW_CSUM, 0, false),
            (BOOTED_CSUM, 0, false),
            (ROLLBACK_CSUM, 0, false),
        ]))
        .push(status(&current));
    push_the_check(&h, &current);
    h.run(Op::Automatic).await.expect("automatic");
    assert_eq!(h.store.read_pins().unwrap(), Pins::default());
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_failed_unpin_keeps_the_record_for_the_next_run() {
    let h = harness_for_the_daily_run(grubenv_with(&[("apex_ok", "1")]), vec![rollback_record()]);
    let current = after_the_update();
    h.push(status(&current))
        .push(listing(&[
            (NEW_CSUM, 0, false),
            (BOOTED_CSUM, 0, false),
            (ROLLBACK_CSUM, 0, true),
        ]))
        .push(
            Scripted::new(BootcCommand::Unpin { index: 2 })
                .exit_code(1)
                .stderr("error: boom\n"),
        )
        .push(status(&current));
    push_the_check(&h, &current);
    h.run(Op::Automatic).await.expect("the daily run goes on");
    assert_eq!(h.store.read_pins().unwrap().pins, [rollback_record()]);
    h.fake.assert_finished();
}
