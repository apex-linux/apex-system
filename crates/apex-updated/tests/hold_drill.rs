#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

//! The hold drills as far as CI can run them: S2d's held gnome-control-center, and S1c's
//! kmod-side pair hold and the `degraded=nvidia` image after its limit.

mod common;

use std::sync::Arc;

use apex_bootc::FakeRunner;
use apex_dbus::{Hold, OBJECT_PATH, Update1Proxy};
use apex_updated::service::Update1Service;
use apex_updated::testing::FakeAuthorizer;
use apexctl::service::{DbusService, UpdateService as _};
use apexctl::status::{StatusJson, hardware_notes, render_hardware_notes, render_status};
use common::*;
use zbus::proxy::CacheProperties;
use zbus::{Guid, connection};

const HELD_FAMILY: &str = "gnome-control-center";

/// apex-lane's line for a family whose newer Fedora build failed with the Apex patches.
const HELD_LOCK: &str = "\
family gnome-control-center held gnome-control-center-50.0-1.fc45-s7 \
since=2026-10-20T04:10:00Z limit=2026-11-03T04:10:00Z \
newer=gnome-control-center-50.1-1.fc45 security=0 \
reason=0003-software-updates-page.patch does not apply
family gnome-shell built gnome-shell-51.0-1.fc45-s1
";

const CLEAN_LOCK: &str = "\
family gnome-control-center built gnome-control-center-50.0-1.fc45-s7
family gnome-shell built gnome-shell-51.0-1.fc45-s1
";

fn held_manifest(version: &str) -> apex_update::Manifest {
    apex_update::Manifest {
        held: vec![HELD_FAMILY.to_owned()],
        degraded: Vec::new(),
        ..manifest(version, Vec::new())
    }
}

fn drill_state() -> apex_updated::testing::FakeState {
    let mut state = fake_state();
    state.running_manifest = Some(apex_update::Manifest {
        held: Vec::new(),
        degraded: Vec::new(),
        ..manifest(BOOTED_VERSION, Vec::new())
    });
    state.running_lane_lock = Some(CLEAN_LOCK.to_owned());
    state
        .manifests
        .insert(STAGED_CSUM.to_owned(), held_manifest(STAGED_VERSION));
    state
        .lane_locks
        .insert(STAGED_CSUM.to_owned(), HELD_LOCK.to_owned());
    state
}

/// The real Update1 object on a peer-to-peer connection, read through apexctl's client.
async fn serve(h: &Harness) -> (zbus::Connection, Update1Proxy<'static>) {
    let service: Update1Service<Arc<FakeRunner>> =
        Update1Service::new(h.daemon.clone(), Arc::new(FakeAuthorizer::allow_all()));
    let (server_socket, client_socket) = std::os::unix::net::UnixStream::pair().unwrap();
    let server = connection::Builder::unix_stream(server_socket)
        .server(Guid::generate())
        .p2p()
        .serve_at(OBJECT_PATH, service)
        .build();
    let client = connection::Builder::unix_stream(client_socket)
        .p2p()
        .build();
    let (server, client) = tokio::try_join!(server, client).unwrap();
    let proxy = Update1Proxy::builder(&client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    (server, proxy)
}

fn the_hold() -> Hold {
    Hold {
        family: HELD_FAMILY.to_owned(),
        newer: "gnome-control-center-50.1-1.fc45".to_owned(),
        since: 1_792_469_400,
        security: false,
        limit: 1_793_679_000,
        side: String::new(),
    }
}

#[tokio::test]
async fn a_held_family_reaches_update1_and_apexctl_status() {
    let h = Harness::with(drill_state(), grubenv_with(&[("apex_ok", "1")]));
    let downloaded = host(booted(), Some(staged(true)), Some(rollback()));
    h.refresh(&downloaded).await;
    let (_server, proxy) = serve(&h).await;

    let staged = proxy.staged().await.unwrap();
    assert_eq!(staged.held, Some(vec![HELD_FAMILY.to_owned()]));
    assert_eq!(staged.holds, Some(vec![the_hold()]));
    assert_eq!(proxy.booted().await.unwrap().holds, Some(Vec::new()));

    let status = DbusService::from_proxy(proxy, false)
        .status()
        .await
        .unwrap();
    insta::assert_snapshot!("downloaded", render_status(&status));
    let json = serde_json::to_value(StatusJson::new(&status)).unwrap();
    assert_eq!(json["staged"]["holds"][0]["family"], HELD_FAMILY);
    assert_eq!(json["staged"]["holds"][0]["security"], false);
    assert_eq!(
        json["holds"],
        serde_json::json!([]),
        "nothing held while running"
    );
    h.fake.assert_finished();

    // The update is applied and booted: the hold is now the running image's.
    h.system.update(|state| {
        state.running_manifest = Some(held_manifest(STAGED_VERSION));
        state.running_lane_lock = Some(HELD_LOCK.to_owned());
    });
    let h = h.reboot(NOW + 600);
    let running = host(
        entry(STAGED, STAGED_VERSION, STAGED_CSUM),
        None,
        Some(booted()),
    );
    h.refresh(&running).await;
    let (_server, proxy) = serve(&h).await;
    assert_eq!(proxy.booted().await.unwrap().holds, Some(vec![the_hold()]));
    let status = DbusService::from_proxy(proxy, false)
        .status()
        .await
        .unwrap();
    let text = render_status(&status);
    assert!(
        text.contains(
            "\nHeld       gnome-control-center, held since 2026-10-20 04:10 UTC, limit \
             2026-11-03 04:10 UTC\n"
        ),
        "{text}"
    );
    insta::assert_snapshot!("booted", text);
    let json = serde_json::to_value(StatusJson::new(&status)).unwrap();
    assert_eq!(json["held"], serde_json::json!([HELD_FAMILY]));
    assert_eq!(
        json["holds"][0]["newer"],
        "gnome-control-center-50.1-1.fc45"
    );
    h.fake.assert_finished();
}

const PAIR_FAMILY: &str = "kernel-cachyos";
const DRIVER: &str = "615.78.08";

/// apex-lane's lock after the kmod failed to build for the next point kernel: the pair is
/// held for 72 h on the kmod side (ADR 0015).
const KMOD_HELD_LOCK: &str = "\
family kernel-cachyos held kernel-cachyos-7.3.0-4.cachyos1.fc45-s1 \
since=2026-12-01T00:00:00Z limit=2026-12-04T00:00:00Z \
newer=kernel-cachyos-0:7.3.1-1.cachyos1.fc45.src security=1 side=kmod \
reason=kmod: nvidia-kmod 615.78.08 does not build on 7.3.1
";

/// After the limit: the owner published the new kernel without its NVIDIA set.
const DEGRADED_LOCK: &str = "\
family kernel-cachyos built kernel-cachyos-7.3.1-1.cachyos1.fc45-s2
";

fn pair_manifest(
    version: &str,
    held: &[&str],
    degraded: &[&str],
    kmod: bool,
) -> apex_update::Manifest {
    let mut packages = vec![package("kernel-cachyos", "7.3.0", "4.cachyos1.fc45")];
    if kmod {
        packages.push(package(
            "kmod-nvidia-7.3.0-4.cachyos1.fc45.x86_64",
            DRIVER,
            "1.fc45",
        ));
    }
    apex_update::Manifest {
        held: held.iter().map(|family| (*family).to_owned()).collect(),
        degraded: degraded
            .iter()
            .map(|component| (*component).to_owned())
            .collect(),
        ..manifest(version, packages)
    }
}

/// The running system as the hardware notes read it: the manifest and the dGPU's sysfs.
fn machine(manifest: &apex_update::Manifest, loaded: Option<&str>) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let write = |path: &str, text: &str| {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(
        "usr/share/apex/manifest.json",
        &serde_json::to_string(manifest).unwrap(),
    );
    let device = "sys/bus/pci/devices/0000:01:00.0";
    write(&format!("{device}/vendor"), "0x10de\n");
    write(&format!("{device}/device"), "0x25a2\n");
    if let Some(version) = loaded {
        let driver = root.path().join("sys/bus/pci/drivers/nvidia");
        std::fs::create_dir_all(&driver).unwrap();
        std::os::unix::fs::symlink(driver, root.path().join(device).join("driver")).unwrap();
        write("sys/module/nvidia/version", &format!("{version}\n"));
    }
    root
}

/// The next image after a kmod failure keeps the old pair: the hold is shown with its side
/// and its 72 h clock, and the driver still matches.
#[tokio::test]
async fn a_kmod_failure_holds_the_pair_and_the_status_views_show_it() {
    let running = pair_manifest(STAGED_VERSION, &[PAIR_FAMILY], &[], true);
    let mut state = fake_state();
    state.running_manifest = Some(running.clone());
    state.running_lane_lock = Some(KMOD_HELD_LOCK.to_owned());
    let h = Harness::with(state, grubenv_with(&[("apex_ok", "1")]));
    h.refresh(&host(
        entry(STAGED, STAGED_VERSION, STAGED_CSUM),
        None,
        Some(booted()),
    ))
    .await;
    let (_server, proxy) = serve(&h).await;

    let booted = proxy.booted().await.unwrap();
    assert_eq!(booted.held, Some(vec![PAIR_FAMILY.to_owned()]));
    assert_eq!(
        booted.holds,
        Some(vec![Hold {
            family: PAIR_FAMILY.to_owned(),
            newer: "kernel-cachyos-0:7.3.1-1.cachyos1.fc45.src".to_owned(),
            since: 1_796_083_200,
            security: true,
            limit: 1_796_083_200 + 72 * 3600,
            side: "kmod".to_owned(),
        }])
    );
    let status = DbusService::from_proxy(proxy, false)
        .status()
        .await
        .unwrap();
    let notes = hardware_notes(machine(&running, Some(DRIVER)).path());
    let text = render_status(&status) + &render_hardware_notes(&notes);
    assert!(
        text.contains(
            "\nHeld       kernel-cachyos (kmod side), held since 2026-12-01 00:00 UTC, limit \
             2026-12-04 00:00 UTC\n"
        ),
        "{text}"
    );
    insta::assert_snapshot!("kmod_held", text);
    let json = serde_json::to_value(StatusJson::new(&status).with_hardware(notes)).unwrap();
    assert_eq!(json["holds"][0]["side"], "kmod");
    assert_eq!(json["hardware"][0]["ok"], true);
    h.fake.assert_finished();
}

/// The limit expired and the owner published the new kernel without its NVIDIA set.
#[tokio::test]
async fn an_expired_limit_gives_a_degraded_nvidia_image_the_views_name() {
    let running = pair_manifest(STAGED_VERSION, &[], &["nvidia"], false);
    let mut state = fake_state();
    state.running_manifest = Some(running.clone());
    state.running_lane_lock = Some(DEGRADED_LOCK.to_owned());
    let h = Harness::with(state, grubenv_with(&[("apex_ok", "1")]));
    h.refresh(&host(
        entry(STAGED, STAGED_VERSION, STAGED_CSUM),
        None,
        Some(booted()),
    ))
    .await;
    let (_server, proxy) = serve(&h).await;

    let booted = proxy.booted().await.unwrap();
    assert_eq!(booted.degraded, Some(vec!["nvidia".to_owned()]));
    assert_eq!(booted.holds, Some(Vec::new()), "the pair moved on");
    let status = DbusService::from_proxy(proxy, false)
        .status()
        .await
        .unwrap();
    let notes = hardware_notes(machine(&running, None).path());
    let text = render_status(&status) + &render_hardware_notes(&notes);
    assert!(
        text.contains("\nDegraded   nvidia: no NVIDIA driver\n"),
        "{text}"
    );
    assert!(
        text.ends_with("\nHardware notes\n  nvidia   no NVIDIA driver (degraded)\n"),
        "{text}"
    );
    insta::assert_snapshot!("degraded_nvidia", text);
    let json = serde_json::to_value(StatusJson::new(&status).with_hardware(notes)).unwrap();
    assert_eq!(json["degraded"], serde_json::json!(["nvidia"]));
    assert_eq!(json["hardware"][0]["detail"], "no NVIDIA driver (degraded)");
    h.fake.assert_finished();
}
