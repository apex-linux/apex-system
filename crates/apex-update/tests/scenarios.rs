//! End-to-end decision flows through the public API, and a snapshot of every text the
//! engine can show a user.

#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use apex_update::gates::{
    GIB, Operation, Trigger, gate_boot_space, gate_bootc_flags, gate_denylist, gate_disk_headroom,
    gate_disk_space, gate_intent, gate_metered, gate_power, gate_version,
};
use apex_update::{
    AfterCheck, ApplyError, AutomaticPlan, Deferral, DenyEntry, DeploymentView, DiscardError,
    DownloadError, Gate, HostView, Intent, Power, RemoteView, State, SystemSnapshot, Version,
    derive_state, intent_is_orphaned, plan_after_check, plan_automatic, validate_apply,
    validate_discard, validate_download,
};

const V1: &str = "45.20260920.1";
const V2: &str = "45.20260924.117";
const V3: &str = "45.20260925.1";

fn v(s: &str) -> Version {
    Version::parse(s).expect("valid")
}

fn deployment(digest: &str, version: &str, download_only: bool) -> DeploymentView {
    DeploymentView {
        digest: digest.into(),
        version: Version::parse(version),
        download_only,
        pinned: false,
    }
}

fn remote(digest: &str, version: &str) -> RemoteView {
    RemoteView {
        digest: digest.into(),
        version: Version::parse(version),
    }
}

/// A laptop on AC with room to spare, booted on `v1`, never checked.
fn laptop() -> SystemSnapshot {
    SystemSnapshot {
        power: Power {
            on_ac: true,
            battery_percent: Some(64),
        },
        metered: None,
        sysroot_free: 200 * GIB,
        boot_free: 700 * 1024 * 1024,
        host: HostView {
            booted: Some(deployment("sha256:v1", V1, false)),
            ..HostView::default()
        },
        now: 1_790_000_000,
        ..SystemSnapshot::default()
    }
}

#[test]
fn update_from_check_to_green_boot() {
    let mut s = laptop();
    assert_eq!(derive_state(&s), State::UpToDate);

    // The timer fires: check, then download.
    assert_eq!(plan_automatic(&s), AutomaticPlan::CheckThenDownload);
    s.remote = Some(remote("sha256:v2", V2));
    assert_eq!(derive_state(&s), State::Available);
    assert_eq!(plan_after_check(&s), AfterCheck::Download);

    // Downloaded and locked.
    s.host.staged = Some(deployment("sha256:v2", V2, true));
    assert_eq!(derive_state(&s), State::Staged);
    assert_eq!(plan_after_check(&s), AfterCheck::NothingToDo);

    // The user reviews and applies on battery.
    s.power = Power {
        on_ac: false,
        battery_percent: Some(40),
    };
    assert_eq!(validate_apply(&s, "sha256:v2", Some(&v(V2))), Ok(()));
    s.host.staged.as_mut().expect("staged").download_only = false;
    s.intent = Some(Intent {
        digest: "sha256:v2".into(),
        created: s.now,
    });
    assert_eq!(derive_state(&s), State::ReadyForReboot);
    assert!(!intent_is_orphaned(&s));

    // A newer build appears: nothing automatic touches the applied stage, and the user
    // must cancel before downloading it.
    s.remote = Some(remote("sha256:v3", V3));
    assert!(matches!(
        plan_automatic(&s),
        AutomaticPlan::Skip(Deferral {
            gate: Gate::Intent,
            ..
        })
    ));
    assert!(matches!(
        validate_download(&s, true),
        Err(DownloadError::Gate(Deferral {
            gate: Gate::Intent,
            ..
        }))
    ));
    assert_eq!(derive_state(&s), State::ReadyForReboot);

    // The user changes their mind. The channel has moved on to v3, and bootc re-locks by
    // pulling the tag, so cancelling would download v3 in place of v2: on 40 % battery
    // that waits for the charger, like any download.
    assert!(matches!(
        validate_discard(&s),
        Err(DiscardError::Gate(Deferral {
            gate: Gate::Power,
            ..
        }))
    ));
    // While the tag still points at v2, cancelling only re-locks it.
    s.remote = Some(remote("sha256:v2", V2));
    assert_eq!(validate_discard(&s), Ok(()));
    s.remote = Some(remote("sha256:v3", V3));

    // The user applies again after all.

    // Reboot into v2; the assessment is green and clears the intent.
    s.host.rollback = s.host.booted.take();
    s.host.booted = s.host.staged.take();
    assert!(!intent_is_orphaned(&s), "booted digest awaits assessment");
    s.intent = None;
    assert_eq!(derive_state(&s), State::Available);

    // Still on 40 % battery: v3 waits for the charger.
    assert!(matches!(
        plan_after_check(&s),
        AfterCheck::Defer(Deferral {
            gate: Gate::Power,
            ..
        })
    ));
    s.power.on_ac = true;
    assert_eq!(plan_after_check(&s), AfterCheck::Download);
}

#[test]
fn failed_update_is_not_offered_again() {
    // GRUB fell back from v2 to v1; apex-boot-assess denylisted v2.
    let mut s = laptop();
    s.remote = Some(remote("sha256:v2", V2));
    s.denylist = [DenyEntry {
        digest: "sha256:v2".into(),
        reason: "boot counter exhausted".into(),
        at: s.now,
    }]
    .into_iter()
    .collect();
    s.intent = Some(Intent {
        digest: "sha256:v2".into(),
        created: s.now - 600,
    });
    assert!(
        intent_is_orphaned(&s),
        "the daemon clears a fallen-back intent"
    );
    s.intent = None;

    assert_eq!(derive_state(&s), State::UpToDate);
    match plan_after_check(&s) {
        AfterCheck::Defer(d) => assert_eq!(d.gate, Gate::Denylist),
        other => panic!("expected a denylist deferral, got {other:?}"),
    }
    assert!(matches!(
        validate_download(&s, true),
        Err(DownloadError::Gate(Deferral {
            gate: Gate::Denylist,
            ..
        }))
    ));

    // A newer build is offered normally.
    s.remote = Some(remote("sha256:v3", V3));
    assert_eq!(derive_state(&s), State::Available);
    assert_eq!(plan_after_check(&s), AfterCheck::Download);
}

#[test]
fn tag_moved_backwards_is_refused_everywhere() {
    let mut s = laptop();
    s.host.booted = Some(deployment("sha256:v2", V2, false));
    s.remote = Some(remote("sha256:v1", V1));
    assert_eq!(derive_state(&s), State::UpToDate);
    assert!(matches!(
        plan_after_check(&s),
        AfterCheck::Defer(Deferral {
            gate: Gate::Version,
            ..
        })
    ));
    assert!(matches!(
        validate_download(&s, true),
        Err(DownloadError::Gate(Deferral {
            gate: Gate::Version,
            ..
        }))
    ));
    // Even if an older image got staged some other way, it cannot be applied.
    s.host.staged = Some(deployment("sha256:v1", V1, true));
    assert!(matches!(
        validate_apply(&s, "sha256:v1", Some(&v(V1))),
        Err(ApplyError::Downgrade { .. })
    ));
}

#[test]
fn metered_network_defers_automatic_but_not_forced_download() {
    let mut s = laptop();
    s.metered = Some(true);
    assert!(matches!(
        plan_automatic(&s),
        AutomaticPlan::Check(Deferral {
            gate: Gate::Metered,
            ..
        })
    ));
    s.remote = Some(remote("sha256:v2", V2));
    assert!(matches!(
        plan_after_check(&s),
        AfterCheck::Defer(Deferral {
            gate: Gate::Metered,
            ..
        })
    ));
    assert!(validate_download(&s, false).is_err());
    assert_eq!(validate_download(&s, true), Ok(()));
}

/// Every text the engine can show, one per line, so wording changes are reviewed.
#[test]
fn every_user_visible_text() {
    let on_battery = |percent| Power {
        on_ac: false,
        battery_percent: percent,
    };
    let denied = [DenyEntry {
        digest: "sha256:bad".into(),
        reason: String::new(),
        at: 0,
    }]
    .into_iter()
    .collect();
    let flags = |incompatible, usr_overlay, rollback_queued| HostView {
        incompatible,
        usr_overlay,
        rollback_queued,
        ..HostView::default()
    };
    let unlocked = HostView {
        staged: Some(deployment("sha256:s", V2, false)),
        ..HostView::default()
    };
    let intent = Intent {
        digest: "sha256:s".into(),
        created: 0,
    };
    let deferrals = [
        gate_power(on_battery(Some(30)), Operation::Download),
        gate_power(on_battery(Some(10)), Operation::Apply),
        gate_power(on_battery(None), Operation::Download),
        gate_power(on_battery(None), Operation::Apply),
        gate_metered(Some(true)),
        gate_disk_space(4 * GIB, None),
        gate_disk_headroom(GIB),
        gate_boot_space(100 * 1024 * 1024, Some(120 * 1024 * 1024)),
        gate_denylist(&denied, "sha256:bad"),
        gate_bootc_flags(&flags(true, false, false)),
        gate_bootc_flags(&flags(false, true, false)),
        gate_bootc_flags(&flags(false, false, true)),
        gate_version(Some(&v(V2)), Some(&v(V1)), Trigger::User),
        gate_version(Some(&v(V1)), None, Trigger::Automatic),
        gate_version(None, Some(&v(V2)), Trigger::Automatic),
        gate_intent(Some(&intent), &HostView::default(), Trigger::Automatic),
        gate_intent(None, &unlocked, Trigger::User),
    ];
    let mut applied = laptop();
    applied.host.staged = Some(deployment("sha256:v2", V2, false));
    applied.metered = Some(true);
    applied.remote = Some(remote("sha256:v3", V3));
    let discard_moved = validate_discard(&applied);
    applied.remote = None;
    let discard_unknown = validate_discard(&applied);
    let discards = [discard_moved, discard_unknown].map(|r| match r {
        Err(DiscardError::Gate(d)) => Err(d),
        other => panic!("expected a discard deferral, got {other:?}"),
    });
    let mut lines: Vec<String> = deferrals
        .into_iter()
        .chain(discards)
        .map(|r| {
            let d = r.expect_err("each case defers");
            format!("{:<11} {}", d.gate.as_str(), d.reason)
        })
        .collect();

    let mut staged = laptop();
    staged.host.staged = Some(deployment("sha256:v2", V2, true));
    let no_version = validate_apply(&staged, "sha256:v2", None).expect_err("no version");
    let errors: Vec<String> = vec![
        no_version.to_string(),
        ApplyError::NothingStaged.to_string(),
        ApplyError::DigestMismatch {
            expected: "sha256:a".into(),
            staged: "sha256:b".into(),
        }
        .to_string(),
        ApplyError::AlreadyApplied.to_string(),
        ApplyError::Downgrade {
            booted: v(V2),
            staged: v(V1),
        }
        .to_string(),
        DownloadError::NothingToDownload.to_string(),
        DiscardError::NothingStaged.to_string(),
        DiscardError::NotApplied.to_string(),
    ];
    lines.extend(errors.into_iter().map(|e| format!("{:<11} {e}", "error")));
    insta::assert_snapshot!("user_visible_texts", lines.join("\n"));
}
