//! zbus rejects a call whose body signature differs from the handler's, and the proxy fails to
//! decode another type, so exercising every member against the mock ties the proxy to the XML.

#![allow(
    clippy::expect_used,
    reason = "integration test helpers: a failed expectation is the test failure"
)]

mod common;

use std::collections::BTreeSet;
use std::future::poll_fn;
use std::pin::Pin;

use apex_dbus::{
    ApexError, BUS_NAME, BootVerdict, Channel, Deployment, ErrorKind, INTERFACE, INTROSPECTION_XML,
    OBJECT_PATH, PolkitAction, State, Update1Proxy,
};
use common::{Log, MockDaemon, STAGED_DIGEST};
use zbus::{
    Connection, Guid, connection, export::futures_core::Stream, fdo::IntrospectableProxy,
    proxy::CacheProperties,
};

async fn connect(daemon: MockDaemon) -> (Connection, Connection) {
    let (server_socket, client_socket) =
        std::os::unix::net::UnixStream::pair().expect("socket pair");
    let server = connection::Builder::unix_stream(server_socket)
        .server(Guid::generate())
        .p2p()
        .serve_at(OBJECT_PATH, daemon)
        .build();
    let client = connection::Builder::unix_stream(client_socket)
        .p2p()
        .build();
    tokio::try_join!(server, client).expect("peer-to-peer handshake")
}

async fn proxy(client: &Connection, cache: CacheProperties) -> Update1Proxy<'static> {
    Update1Proxy::builder(client)
        .cache_properties(cache)
        .build()
        .await
        .expect("proxy")
}

fn contract_members(kind: &str) -> BTreeSet<String> {
    common::interface_description(INTROSPECTION_XML)
        .into_keys()
        .filter_map(|key| key.strip_prefix(&format!("{kind} ")).map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn server_types_produce_the_contract_introspection() {
    let (_server, client) = connect(MockDaemon::new(Log::default())).await;
    let introspectable = IntrospectableProxy::builder(&client)
        .destination(BUS_NAME)
        .path(OBJECT_PATH)
        .build()
        .await
        .expect("introspectable proxy");
    let generated = introspectable.introspect().await.expect("introspect");

    let differences = common::diff(
        &common::interface_description(INTROSPECTION_XML),
        &common::interface_description(&generated),
    );
    assert!(
        differences.is_empty(),
        "the interface generated from apex-dbus types differs from the contract XML:\n{}\n\n\
         generated XML:\n{generated}",
        differences.join("\n")
    );
}

#[tokio::test]
async fn every_method_matches_the_contract_on_the_wire() {
    let log = Log::default();
    let (_server, client) = connect(MockDaemon::new(log.clone())).await;
    let proxy = proxy(&client, CacheProperties::No).await;

    proxy.ping().await.expect("Ping");
    proxy.check().await.expect("Check");
    proxy.download(true).await.expect("Download");
    proxy.apply(STAGED_DIGEST).await.expect("Apply");
    proxy.rollback(true).await.expect("Rollback");
    assert_eq!(
        proxy.get_changes().await.expect("GetChanges"),
        common::changes()
    );
    assert_eq!(
        proxy
            .get_changes_for(STAGED_DIGEST)
            .await
            .expect("GetChangesFor"),
        common::changes()
    );
    assert!(matches!(
        proxy.get_changes_for("sha256:00").await,
        Err(ApexError::InvalidArgument(_))
    ));
    proxy
        .acknowledge_health_failure()
        .await
        .expect("AcknowledgeHealthFailure");
    assert!(matches!(
        proxy.automatic().await,
        Err(ApexError::NotAuthorized(_))
    ));
    proxy
        .report_boot(BootVerdict::Green, STAGED_DIGEST)
        .await
        .expect("ReportBoot");
    proxy
        .set_automatic_downloads(true)
        .await
        .expect("SetAutomaticDownloads");
    proxy.major_upgrade(46, false).await.expect("MajorUpgrade");
    assert!(matches!(
        proxy.major_upgrade(47, true).await,
        Err(ApexError::NothingToDo(_))
    ));
    assert!(matches!(
        proxy.set_automatic_downloads(false).await,
        Err(ApexError::NotAuthorized(_))
    ));

    let calls = log.calls();
    let called: BTreeSet<String> = calls.iter().map(|call| call.member.clone()).collect();
    assert_eq!(
        called,
        contract_members("method"),
        "every contract method must be exposed by Update1Proxy and exercised here"
    );

    let interactive: BTreeSet<&str> = calls
        .iter()
        .filter(|call| call.interactive)
        .map(|call| call.member.as_str())
        .collect();
    let expected: BTreeSet<&str> = [
        "Check",
        "Download",
        "Apply",
        "Rollback",
        "AcknowledgeHealthFailure",
        "SetAutomaticDownloads",
        "MajorUpgrade",
    ]
    .into();
    assert_eq!(interactive, expected);
    let guarded: BTreeSet<&str> = calls
        .iter()
        .map(|call| call.member.as_str())
        .filter(|member| PolkitAction::guarding(member).is_some())
        .collect();
    assert_eq!(guarded, expected);
}

#[tokio::test]
async fn every_property_matches_the_contract_on_the_wire() {
    let log = Log::default();
    let (_server, client) = connect(MockDaemon::new(log.clone())).await;
    let proxy = proxy(&client, CacheProperties::No).await;

    assert_eq!(proxy.state().await.expect("State"), State::Staged);
    assert!((proxy.progress().await.expect("Progress") + 1.0).abs() < f64::EPSILON);
    assert_eq!(proxy.booted().await.expect("Booted"), common::booted());
    assert_eq!(proxy.staged().await.expect("Staged"), common::staged());
    let rollback = proxy.rollback_deployment().await.expect("Rollback");
    assert!(rollback.is_empty());
    assert_eq!(
        proxy.available().await.expect("Available").into_option(),
        None
    );
    assert_eq!(proxy.channel().await.expect("Channel"), Channel::Stable);
    assert_eq!(proxy.last_check().await.expect("LastCheck"), 1_790_400_000);
    assert_eq!(
        proxy.deferred_reason().await.expect("DeferredReason"),
        "Metered network"
    );
    assert_eq!(proxy.last_error().await.expect("LastError"), "");
    assert_eq!(proxy.health_failure().await.expect("HealthFailure"), "");
    assert_eq!(
        proxy
            .health_failure_checks()
            .await
            .expect("HealthFailureChecks"),
        ["session"]
    );
    assert_eq!(proxy.kept().await.expect("Kept"), [common::kept()]);
    assert_eq!(
        proxy.snapshots().await.expect("Snapshots"),
        [common::snapshot()]
    );
    assert_eq!(
        proxy.last_fallback().await.expect("LastFallback"),
        STAGED_DIGEST
    );
    assert!(
        !proxy
            .automatic_downloads()
            .await
            .expect("AutomaticDownloads")
    );

    let read: BTreeSet<String> = log.properties().into_iter().collect();
    assert_eq!(
        read,
        contract_members("property"),
        "every contract property must be exposed by Update1Proxy and exercised here"
    );
}

#[tokio::test]
async fn every_error_kind_round_trips() {
    let (_server, client) = connect(MockDaemon::new(Log::default())).await;
    let proxy = proxy(&client, CacheProperties::No).await;

    for kind in ErrorKind::ALL {
        let error = proxy
            .apply(&format!("error:{}", kind.as_str()))
            .await
            .expect_err("scripted error");
        assert_eq!(error.kind(), Some(kind), "{error}");
        assert_eq!(error.message(), format!("{} from the mock", kind.as_str()));
        assert_eq!(
            error.to_string(),
            format!("{}: {} from the mock", kind.error_name(), kind.as_str())
        );
    }

    let error = proxy.apply("error:zbus").await.expect_err("scripted error");
    assert_eq!(error.kind(), Some(ErrorKind::Internal));
    assert!(error.message().contains("socket closed"), "{error}");
    let error = proxy
        .apply("error:unconverted")
        .await
        .expect_err("scripted error");
    assert_eq!(error.kind(), None);
    match error {
        ApexError::ZBus(zbus::Error::MethodError(name, ..)) => {
            assert_eq!(name.as_str(), "org.freedesktop.zbus.Error");
        }
        other => panic!("unexpected {other:?}"),
    }

    match proxy.apply("sha256:0000").await {
        Err(ApexError::DigestMismatch(message)) => assert!(message.contains(STAGED_DIGEST)),
        other => panic!("unexpected {other:?}"),
    }
    match proxy.download(false).await {
        Err(ApexError::PolicyDenied(reason)) => assert_eq!(reason, "On battery below 50 %"),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn invalid_strings_are_the_daemons_to_reject() {
    let (_server, client) = connect(MockDaemon::new(Log::default())).await;
    let raw = zbus::Proxy::new(&client, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .expect("raw proxy");
    let error: ApexError = raw
        .call::<_, _, ()>("ReportBoot", &("beta", STAGED_DIGEST))
        .await
        .expect_err("invalid verdict")
        .into();
    match error {
        ApexError::InvalidArgument(message) => {
            assert_eq!(message, "\"beta\" is not a valid BootVerdict");
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// Avoids a futures-util dependency.
async fn next<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

#[tokio::test]
async fn cached_properties_follow_change_signals() {
    let (_server, client) = connect(MockDaemon::new(Log::default())).await;
    let proxy = proxy(&client, CacheProperties::Yes).await;
    assert_eq!(proxy.cached_state().expect("cache"), Some(State::Staged));
    assert_eq!(
        proxy
            .cached_booted()
            .expect("cache")
            .and_then(Deployment::into_option),
        Some(common::booted())
    );

    let mut changes = proxy.receive_state_changed().await;
    let first = next(&mut changes).await.expect("current value");
    assert_eq!(first.get().await.expect("value"), State::Staged);
    proxy.check().await.expect("Check");
    let update = next(&mut changes).await.expect("change signal");
    assert_eq!(update.get().await.expect("value"), State::Available);
    assert_eq!(proxy.cached_state().expect("cache"), Some(State::Available));
}
