#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

//! S6's move to the next Fedora major behind fakes: download first, then pin the running
//! version, save `/var`, and apply armed (ARCHITECTURE §5.4).

mod common;

use apex_boot::GrubEnvWriter as _;
use apex_bootc::{BootEntry, BootcCommand, Host, ImageReference, Scripted, apex_stable};
use apex_dbus::ErrorKind;
use apex_update::{MajorPinRecord, MajorUpgrades, Pins, SnapshotRecord};
use apex_updated::engine::major_pin_reason;
use apex_updated::queue::Op;
use apex_updated::store::Store as _;
use common::*;

const NEXT: &str = "sha256:4646464646464646464646464646464646464646464646464646464646464646";
const NEXT_CSUM: &str = "4646464646464646464646464646464646464646464646464646464646464646";
const NEXT_VERSION: &str = "46.20270415.1";
const SNAPSHOT: &str = "/var/lib/apex/snapshots/var-fedora45-1790300000";

fn next(download_only: bool) -> BootEntry {
    let mut entry = entry(NEXT, NEXT_VERSION, NEXT_CSUM);
    if let Some(image) = &mut entry.image {
        image.image = ImageReference {
            image: "ghcr.io/apex-linux/apex:46".into(),
            ..image.image.clone()
        };
    }
    BootEntry {
        download_only,
        ..entry
    }
}

fn pinned(entry: BootEntry) -> BootEntry {
    BootEntry {
        pinned: true,
        ..entry
    }
}

fn state() -> apex_updated::testing::FakeState {
    let mut state = fake_state();
    let mut manifest = manifest(NEXT_VERSION, Vec::new());
    manifest.fedora = 46;
    state.manifests.insert(NEXT_CSUM.to_owned(), manifest);
    state
}

fn harness() -> Harness {
    Harness::with(state(), grubenv_with(&[("apex_ok", "1")]))
}

fn before() -> Host {
    host(booted(), None, Some(rollback()))
}

fn switch() -> Scripted {
    Scripted::new(BootcCommand::SwitchDownloadOnly {
        target: apex_stable(46),
    })
}

fn snapshot_var() -> Scripted {
    Scripted::new(BootcCommand::SnapshotVar {
        major: 45,
        unix: NOW,
    })
}

const MOVE: Op = Op::MajorUpgrade {
    target_major: 46,
    override_soft_gates: false,
};

fn grub_var(h: &Harness, name: &str) -> Option<String> {
    h.grub.env().get_str(name).unwrap().map(str::to_owned)
}

#[tokio::test(start_paused = true)]
async fn the_move_downloads_pins_saves_var_and_applies_armed() {
    let h = harness();
    let locked = host(booted(), Some(next(true)), Some(rollback()));
    h.push(status(&before()))
        .push(switch())
        .push(status(&locked))
        .push(Scripted::new(BootcCommand::PinBooted))
        .push(snapshot_var())
        .push(status(&host(
            pinned(booted()),
            Some(next(true)),
            Some(rollback()),
        )))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&host(
            pinned(booted()),
            Some(next(false)),
            Some(rollback()),
        )));
    h.run(MOVE).await.expect("major upgrade");
    h.fake.assert_finished();

    assert_eq!(grub_var(&h, "apex_target").as_deref(), Some(NEXT));
    assert_eq!(grub_var(&h, "apex_counter").as_deref(), Some("3"));
    let record = h.store.read_major_upgrades().unwrap();
    assert_eq!(
        record.pins,
        [MajorPinRecord {
            checksum: BOOTED_CSUM.into(),
            serial: 0,
            digest: BOOTED.into(),
            from_major: 45,
            to_major: 46,
            at: NOW,
        }]
    );
    assert_eq!(
        record.snapshots,
        [SnapshotRecord {
            path: SNAPSHOT.into(),
            major: 45,
            at: NOW,
        }]
    );
    assert_eq!(
        h.store.read_pins().unwrap(),
        Pins::default(),
        "never in pinned.json, so the daily unpin leaves it"
    );
    let properties = h.properties();
    assert_eq!(properties.snapshots, [(SNAPSHOT.to_owned(), 45, NOW)]);
    assert_eq!(
        properties.booted.pin_reason,
        Some(major_pin_reason(46)),
        "Kept by the move to Apex 46."
    );
    assert_eq!(h.intent().map(|i| i.digest).as_deref(), Some(NEXT));
}

#[tokio::test(start_paused = true)]
async fn the_daily_run_on_apex_46_leaves_the_kept_45_pinned() {
    let h = harness();
    let locked = host(booted(), Some(next(true)), Some(rollback()));
    let kept = host(pinned(booted()), Some(next(true)), Some(rollback()));
    let applied = host(pinned(booted()), Some(next(false)), Some(rollback()));
    h.push(status(&before()))
        .push(switch())
        .push(status(&locked))
        .push(Scripted::new(BootcCommand::PinBooted))
        .push(snapshot_var())
        .push(status(&kept))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&applied));
    h.run(MOVE).await.expect("major upgrade");

    let h = h.reboot(NOW + 600);
    let mut assessed = h.grub.clone();
    assessed.unset(&["apex_target", "apex_counter"]).unwrap();
    assessed.set(&[("apex_ok", "1")]).unwrap();
    assert!(h.store.read_pins().unwrap().pins.is_empty());
    h.system.update(|state| {
        let mut manifest = manifest(NEXT_VERSION, Vec::new());
        manifest.fedora = 46;
        state.running_manifest = Some(manifest);
    });
    let on_46 = host(next(false), None, Some(pinned(booted())));
    h.push(status(&on_46));
    h.run(Op::Automatic).await.expect("automatic");
    h.fake.assert_finished();
    assert!(
        !h.fake
            .calls()
            .iter()
            .any(|c| matches!(c, BootcCommand::Unpin { .. })),
        "{:?}",
        h.fake.calls()
    );
    assert_eq!(
        h.properties().rollback.pin_reason,
        Some(major_pin_reason(46))
    );
    assert_eq!(h.properties().snapshots.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_tag_not_published_yet_refuses_before_any_pin_or_snapshot() {
    let h = harness();
    h.push(status(&before()))
        .push(switch().exit_code(1).stderr(
            "error: Switching (ostree): Pulling: failed to invoke method OpenImage: \
                 reading manifest 46 in ghcr.io/apex-linux/apex: manifest unknown",
        ))
        .push(status(&before()));
    let error = h.run(MOVE).await.unwrap_err();
    assert_eq!(error.kind(), Some(ErrorKind::NothingToDo), "{error:?}");
    assert_eq!(error.message(), "No Apex 46 image is published yet.");
    h.fake.assert_finished();
    assert_eq!(grub_var(&h, "apex_target"), None);
    assert_eq!(
        h.store.read_major_upgrades().unwrap(),
        MajorUpgrades::default()
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_snapshot_reports_the_pin_it_left_and_installs_nothing() {
    let h = harness();
    let locked = host(booted(), Some(next(true)), Some(rollback()));
    let kept = host(pinned(booted()), Some(next(true)), Some(rollback()));
    h.push(status(&before()))
        .push(switch())
        .push(status(&locked))
        .push(Scripted::new(BootcCommand::PinBooted))
        .push(
            snapshot_var()
                .exit_code(1)
                .stderr("ERROR: cannot snapshot '/var': Read-only file system"),
        )
        .push(status(&kept));
    let error = h.run(MOVE).await.unwrap_err();
    h.fake.assert_finished();
    let message = error.message();
    assert!(
        message.contains("Apex 45 stays pinned (Kept by the move to Apex 46.)"),
        "{message}"
    );
    assert!(message.contains("Read-only file system"), "{message}");
    assert_eq!(grub_var(&h, "apex_target"), None, "nothing is armed");
    let record = h.store.read_major_upgrades().unwrap();
    assert_eq!(record.pins.len(), 1);
    assert!(record.snapshots.is_empty());
    let properties = h.properties();
    assert_eq!(properties.booted.pin_reason, Some(major_pin_reason(46)));
    assert_eq!(properties.staged.download_only, Some(true));
}

#[tokio::test(start_paused = true)]
async fn refusals_come_before_any_change() {
    let refuse = |change: fn(&mut apex_updated::testing::FakeState), host: Host, op: Op| async move {
        let h = Harness::with(
            {
                let mut s = state();
                change(&mut s);
                s
            },
            grubenv_with(&[("apex_ok", "1")]),
        );
        h.push(status(&host));
        let error = h.run(op).await.unwrap_err();
        h.fake.assert_finished();
        assert_eq!(h.fake.calls(), [BootcCommand::Status]);
        assert_eq!(
            h.store.read_major_upgrades().unwrap(),
            MajorUpgrades::default()
        );
        error
    };
    let none = |_: &mut apex_updated::testing::FakeState| {};

    let error = refuse(
        |s| {
            if let Some(m) = &mut s.running_manifest {
                m.fedora = 46;
            }
        },
        before(),
        MOVE,
    )
    .await;
    assert_eq!(error.kind(), Some(ErrorKind::NothingToDo));
    assert_eq!(error.message(), "This computer already runs Apex 46.");

    let skip = Op::MajorUpgrade {
        target_major: 47,
        override_soft_gates: false,
    };
    let error = refuse(none, before(), skip).await;
    assert_eq!(error.kind(), Some(ErrorKind::InvalidArgument));

    let error = refuse(none, host(booted(), Some(staged(true)), None), MOVE).await;
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert!(
        error
            .message()
            .starts_with("A downloaded update is waiting.")
    );

    let error = refuse(|s| s.var_is_subvolume = Ok(false), before(), MOVE).await;
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
    assert_eq!(
        error.message(),
        "/var is not a btrfs subvolume, so it cannot be saved before the move. The major \
         upgrade is refused."
    );

    let error = refuse(|s| s.metered = Some(true), before(), MOVE).await;
    assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
}

#[tokio::test(start_paused = true)]
async fn the_override_skips_only_the_soft_gates() {
    let h = Harness::with(
        {
            let mut s = state();
            s.metered = Some(true);
            s
        },
        grubenv_with(&[("apex_ok", "1")]),
    );
    h.push(status(&before()))
        .push(
            switch()
                .exit_code(1)
                .stderr("error: reading manifest 46 in ghcr.io/apex-linux/apex: manifest unknown"),
        )
        .push(status(&before()));
    let error = h
        .run(Op::MajorUpgrade {
            target_major: 46,
            override_soft_gates: true,
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        Some(ErrorKind::NothingToDo),
        "it reached the switch"
    );
    h.fake.assert_finished();
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_deleted_by_hand_leaves_snapshots_and_the_record() {
    let h = harness();
    let locked = host(booted(), Some(next(true)), Some(rollback()));
    let kept = host(pinned(booted()), Some(next(true)), Some(rollback()));
    let applied = host(pinned(booted()), Some(next(false)), Some(rollback()));
    h.push(status(&before()))
        .push(switch())
        .push(status(&locked))
        .push(Scripted::new(BootcCommand::PinBooted))
        .push(snapshot_var())
        .push(status(&kept))
        .push(Scripted::new(BootcCommand::UpgradeFromDownloaded))
        .push(status(&applied));
    h.run(MOVE).await.expect("major upgrade");
    assert_eq!(h.properties().snapshots.len(), 1);

    h.system
        .update(|state| state.deleted_snapshots.push(SNAPSHOT.to_owned()));
    h.refresh(&applied).await;
    h.fake.assert_finished();
    assert_eq!(h.properties().snapshots, []);
    let record = h.store.read_major_upgrades().unwrap();
    assert!(record.snapshots.is_empty());
    assert_eq!(
        record.pins.len(),
        1,
        "the pin is the deployment's, not the snapshot's"
    );
    assert_eq!(h.properties().booted.pin_reason, Some(major_pin_reason(46)));
}
