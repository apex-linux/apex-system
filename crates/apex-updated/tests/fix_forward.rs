#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

//! S4's exit rehearsal: pin before arm fixes a failed build forward with one reboot. GRUB's
//! counter model, the assessment's decision and the daemon share one grubenv.

mod common;

use std::fmt::Write as _;

use apex_boot::checks::names;
use apex_boot::snippet::{BootEntry as GrubEntry, grub_boot, grub_menu};
use apex_boot::{
    AssessInput, BootVars, CheckResult, DefaultDeployment, Digest, GrubEnvWriter as _, Verdict,
    apply_ops, decide,
};
use apex_bootc::{BootEntry, BootcCommand, Host, Scripted};
use apex_dbus::BootVerdict;
use apex_update::{Pins, UpdateSettings};
use apex_updated::engine::PIN_REASON;
use apex_updated::queue::Op;
use apex_updated::store::Store as _;
use common::*;

/// The good version that was running before the bad build.
const GOOD: &str = BOOTED;
const GOOD_CSUM: &str = BOOTED_CSUM;
/// The build that fails its startup check.
const BAD: &str = STAGED;
const BAD_CSUM: &str = STAGED_CSUM;
const FIX: &str = "sha256:6666666666666666666666666666666666666666666666666666666666666666";
const FIX_CSUM: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const FIX_VERSION: &str = "45.20260930.1";

fn good() -> BootEntry {
    booted()
}

fn bad() -> BootEntry {
    entry(BAD, STAGED_VERSION, BAD_CSUM)
}

fn fix(download_only: bool) -> BootEntry {
    BootEntry {
        download_only,
        ..entry(FIX, FIX_VERSION, FIX_CSUM)
    }
}

fn pinned(entry: BootEntry) -> BootEntry {
    BootEntry {
        pinned: true,
        ..entry
    }
}

fn digest(text: &str) -> Digest {
    Digest::parse(text).unwrap()
}

fn grub_var(h: &Harness, name: &str) -> Option<String> {
    h.grub.env().get_str(name).unwrap().map(str::to_owned)
}

/// What `08_apex-bootcount.cfg` does on a power-on, written back through the daemon's
/// grubenv.
fn power_on(h: &Harness) -> GrubEntry {
    let mut env = h.grub.env();
    let choice = grub_boot(&mut env).unwrap();
    let mut writer = h.grub.clone();
    let assignments: Vec<(&str, &str)> = ["apex_counter", "apex_ok"]
        .into_iter()
        .filter_map(|name| Some((name, env.get_str(name).unwrap()?)))
        .collect();
    writer.set(&assignments).unwrap();
    choice.entry
}

/// apex-boot-assess with a user logged in: its decision applied to grubenv, its verdict
/// reported to the daemon.
fn assess(h: &Harness, booted: &str, rollback: &str, green: bool) -> Verdict {
    let mut required: Vec<CheckResult> = names::REQUIRED
        .iter()
        .map(|name| CheckResult::pass(*name))
        .collect();
    if !green {
        required[0] = CheckResult::fail(names::SESSION, "gnome-shell crashed");
    }
    let decision = decide(&AssessInput {
        vars: BootVars::from_env(&h.grub.env()).unwrap(),
        booted_digest: Some(digest(booted)),
        default_deployment: DefaultDeployment::from_status(false, Some(digest(rollback))),
        required_checks: required,
        user_session_active: true,
    });
    assert!(!decision.reboot(), "a session is open: {decision}");
    apply_ops(&mut h.grub.clone(), &decision.ops()).unwrap();
    let verdict = decision.verdict();
    let reported = match verdict {
        Verdict::Green => BootVerdict::Green,
        Verdict::Red => BootVerdict::Red,
        Verdict::RedUnarmed => BootVerdict::RedUnarmed,
        Verdict::Fallback => BootVerdict::Fallback,
    };
    h.daemon
        .engine()
        .report_boot(reported, Some(booted))
        .expect("report");
    verdict
}

fn listing(rows: &[(&str, bool)]) -> Scripted {
    let mut text = String::new();
    for (index, (checksum, pinned)) in rows.iter().enumerate() {
        let marker = if index == 0 { '*' } else { ' ' };
        writeln!(text, "{marker} default {checksum}.0").unwrap();
        if *pinned {
            text.push_str("    Pinned: yes\n");
        }
    }
    Scripted::new(BootcCommand::OstreeStatus).stdout(text)
}

fn fixed_state() -> apex_updated::testing::FakeState {
    let mut state = fake_state();
    state.running_manifest = Some(manifest(STAGED_VERSION, Vec::new()));
    state
        .manifests
        .insert(FIX_CSUM.to_owned(), manifest(FIX_VERSION, Vec::new()));
    state
        .manifests
        .insert(GOOD_CSUM.to_owned(), manifest(BOOTED_VERSION, Vec::new()));
    state
}

fn with_others(mut host: Host, others: Vec<BootEntry>) -> Host {
    host.status.other_deployments = others;
    host
}

#[tokio::test(start_paused = true)]
async fn pin_before_arm_fixes_a_failed_build_forward_with_one_reboot() {
    // The bad build was applied over the good one and has just booted.
    let h = Harness::with(
        fixed_state(),
        grubenv_with(&[
            ("apex_target", BAD),
            ("apex_counter", "3"),
            ("apex_ok", "0"),
        ]),
    );
    h.store
        .write_settings(&UpdateSettings {
            automatic_downloads: false,
        })
        .unwrap();
    assert_eq!(power_on(&h), GrubEntry::Default);
    let failed = host(bad(), None, Some(good()));
    h.refresh(&failed).await;
    assert_eq!(assess(&h, BAD, GOOD, false), Verdict::Red);
    assert_eq!(h.properties().health_failure, BAD);
    assert!(
        !grub_menu(&h.grub.env()).hidden,
        "an unconfirmed boot shows the menu"
    );

    // The fix is downloaded; applying it keeps the good version first.
    h.refresh(&host(bad(), Some(fix(true)), Some(good()))).await;
    h.push(status(&host(bad(), Some(fix(true)), Some(good()))))
        .push(Scripted::new(BootcCommand::PinRollback))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(bad(), Some(fix(false)), Some(pinned(good())))));
    h.run(Op::Apply {
        expected_digest: FIX.into(),
    })
    .await
    .expect("apply");
    let calls = h.fake.calls();
    let pin = calls.iter().position(|c| *c == BootcCommand::PinRollback);
    let unlock = calls
        .iter()
        .position(|c| *c == BootcCommand::UpgradeFromDownloaded);
    assert!(pin < unlock, "{calls:?}");
    assert_eq!(grub_var(&h, "apex_target").as_deref(), Some(FIX));
    assert_eq!(grub_var(&h, "apex_counter").as_deref(), Some("3"));
    assert_eq!(
        h.store
            .read_pins()
            .unwrap()
            .pins
            .iter()
            .map(|p| p.digest.as_str())
            .collect::<Vec<_>>(),
        [GOOD]
    );
    h.fake.assert_finished();

    // One reboot: finalization keeps the fix, the bad build and the pinned good version.
    let h = h.reboot(NOW + 600);
    assert_eq!(power_on(&h), GrubEntry::Default, "the fix boots");
    let fixed = with_others(host(fix(false), None, Some(bad())), vec![pinned(good())]);
    h.refresh(&fixed).await;
    let kept = h.properties().kept;
    assert_eq!(kept.len(), 1, "the good version waits in the boot menu");
    assert_eq!(kept[0].digest.as_deref(), Some(GOOD));
    assert_eq!(kept[0].pin_reason.as_deref(), Some(PIN_REASON));

    assert_eq!(assess(&h, FIX, BAD, true), Verdict::Green);
    assert_eq!(grub_var(&h, "apex_ok").as_deref(), Some("1"));
    assert_eq!(grub_var(&h, "apex_target"), None);
    assert!(grub_menu(&h.grub.env()).hidden);
    assert_eq!(h.properties().health_failure, "");

    // The daily run lets the good version go once the fix has proved itself.
    let unpinned = with_others(host(fix(false), None, Some(bad())), vec![good()]);
    h.push(status(&fixed))
        .push(listing(&[
            (FIX_CSUM, false),
            (BAD_CSUM, false),
            (GOOD_CSUM, true),
        ]))
        .push(Scripted::new(BootcCommand::Unpin { index: 2 }))
        .push(status(&unpinned))
        .push(Scripted::new(BootcCommand::UpgradeCheck))
        .push(status(&unpinned));
    h.run(Op::Automatic).await.expect("automatic");
    assert_eq!(h.store.read_pins().unwrap(), Pins::default());
    assert!(h.properties().kept.is_empty());
    h.fake.assert_finished();
}
