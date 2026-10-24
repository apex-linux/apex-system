#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

//! S2d's hold drill as far as CI can run it: the lane kept gnome-control-center at its last
//! good build because an Apex patch stopped applying.

mod common;

use std::sync::Arc;

use apex_bootc::FakeRunner;
use apex_dbus::{Hold, OBJECT_PATH, Update1Proxy};
use apex_updated::service::Update1Service;
use apex_updated::testing::FakeAuthorizer;
use apexctl::service::{DbusService, UpdateService as _};
use apexctl::status::{StatusJson, render_status};
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
