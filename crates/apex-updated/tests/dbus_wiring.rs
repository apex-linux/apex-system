#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration tests: a failed expectation is the test failure"
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use apex_bootc::{APEX_TESTING, BootcCommand, FakeRunner, Scripted};
use apex_dbus::{
    ApexError, BUS_NAME, BootVerdict, Channel, Deployment, ErrorKind, INTERFACE, INTROSPECTION_XML,
    OBJECT_PATH, PolkitAction, State, Update1Proxy,
};
use apex_updated::queue::Op;
use apex_updated::service::{Update1Service, emit_property_changes};
use apex_updated::store::Store as _;
use apex_updated::testing::{FakeAuthorizer, SharedGrubWriter};
use common::*;
use zbus::OwnedVariant;
use zbus::export::futures_core::Stream;
use zbus::fdo::{IntrospectableProxy, PropertiesChangedStream, PropertiesProxy};
use zbus::proxy::CacheProperties;
use zbus::{Connection, Guid, connection};

/// Returns once the emitter's first signal has arrived, so later streams see only changes.
async fn connect(h: &Harness, auth: FakeAuthorizer) -> (Connection, Connection) {
    let (server, client, first) = connect_announced(h, auth).await;
    assert_eq!(first.len(), 15, "{first:?}");
    (server, client)
}

async fn connect_announced(
    h: &Harness,
    auth: FakeAuthorizer,
) -> (Connection, Connection, BTreeMap<String, OwnedVariant>) {
    let service: Update1Service<Arc<FakeRunner>> =
        Update1Service::new(h.daemon.clone(), Arc::new(auth));
    let (server_socket, client_socket) =
        std::os::unix::net::UnixStream::pair().expect("socket pair");
    let server = connection::Builder::unix_stream(server_socket)
        .server(Guid::generate())
        .p2p()
        .serve_at(OBJECT_PATH, service)
        .build();
    let client = connection::Builder::unix_stream(client_socket)
        .p2p()
        .build();
    let (server, client) = tokio::try_join!(server, client).expect("peer-to-peer handshake");
    let mut stream = properties_proxy(&client)
        .await
        .receive_properties_changed()
        .await
        .unwrap();
    tokio::spawn(emit_property_changes(
        server.clone(),
        h.daemon.engine().subscribe(),
    ));
    let signal = next(&mut stream).await;
    let args = signal.args().expect("PropertiesChanged arguments");
    let first = args
        .changed_properties
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.try_to_owned().expect("owned")))
        .collect();
    (server, client, first)
}

async fn properties_proxy(client: &Connection) -> PropertiesProxy<'static> {
    PropertiesProxy::builder(client)
        .destination(BUS_NAME)
        .path(OBJECT_PATH)
        .build()
        .await
        .unwrap()
}

async fn proxy(client: &Connection) -> Update1Proxy<'static> {
    Update1Proxy::builder(client)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .expect("proxy")
}

const EMITS_CHANGED_SIGNAL_DEFAULT: &str = "true";

fn args<'r, 'x: 'r>(args: impl IntoIterator<Item = &'r zbus_xml::Arg<'x>>) -> String {
    args.into_iter()
        .map(|arg| {
            let direction = match arg.direction() {
                Some(zbus_xml::ArgDirection::Out) => "out",
                _ => "in",
            };
            format!(
                "{direction} {} {}",
                arg.name().unwrap_or("?"),
                arg.ty().to_string()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `"method Apply" => "in expected_digest s"`, `"property State" => "s read changes=true"`.
fn members(xml: &str) -> BTreeMap<String, String> {
    let node = zbus_xml::Node::try_from(xml).expect("introspection XML parses");
    let interface = node
        .interfaces()
        .iter()
        .find(|i| i.name().as_str() == INTERFACE)
        .expect("the interface is present");
    let mut out = BTreeMap::new();
    for method in interface.methods() {
        out.insert(format!("method {}", method.name()), args(method.args()));
    }
    for signal in interface.signals() {
        out.insert(format!("signal {}", signal.name()), args(signal.args()));
    }
    for property in interface.properties() {
        let access = match property.access() {
            zbus_xml::PropertyAccess::Read => "read",
            zbus_xml::PropertyAccess::Write => "write",
            zbus_xml::PropertyAccess::ReadWrite => "readwrite",
        };
        let emits = property
            .annotations()
            .iter()
            .find(|a| a.name() == "org.freedesktop.DBus.Property.EmitsChangedSignal")
            .map_or(EMITS_CHANGED_SIGNAL_DEFAULT, zbus_xml::Annotation::value);
        out.insert(
            format!("property {}", property.name()),
            format!("{} {access} changes={emits}", property.ty().to_string()),
        );
    }
    out
}

#[tokio::test]
async fn introspection_matches_the_contract() {
    let h = Harness::new();
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let introspectable = IntrospectableProxy::builder(&client)
        .destination(BUS_NAME)
        .path(OBJECT_PATH)
        .build()
        .await
        .unwrap();
    let served = introspectable.introspect().await.expect("introspect");
    assert_eq!(members(&served), members(INTROSPECTION_XML));
}

#[tokio::test]
async fn properties_and_methods_over_the_wire() {
    let h = Harness::new();
    h.last_check(STAGED, STAGED_VERSION, 3600);
    h.refresh(&host(booted(), Some(staged(true)), Some(rollback())))
        .await;
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let p = proxy(&client).await;

    p.ping().await.expect("ping");
    assert_eq!(p.state().await.unwrap(), State::Staged);
    assert_eq!(p.channel().await.unwrap(), Channel::Stable);
    let staged_dict = p.staged().await.unwrap();
    assert_eq!(staged_dict.digest.as_deref(), Some(STAGED));
    assert_eq!(staged_dict.download_only, Some(true));
    assert_eq!(p.booted().await.unwrap().digest.as_deref(), Some(BOOTED));
    assert_eq!(
        p.rollback_deployment().await.unwrap().digest.as_deref(),
        Some(ROLLBACK)
    );
    assert!(
        p.available().await.unwrap().is_empty(),
        "staged is not available"
    );
    assert_eq!(p.last_check().await.unwrap(), NOW - 3600);
    assert!(p.progress().await.unwrap() < 0.0);
    assert_eq!(p.deferred_reason().await.unwrap(), "");
    assert_eq!(p.last_error().await.unwrap(), "");
    assert_eq!(p.health_failure().await.unwrap(), "");

    let changes = p.get_changes().await.expect("changes");
    assert!(changes.contains(&(
        "kernel".to_owned(),
        "x86_64".to_owned(),
        "upgraded".to_owned(),
        "6.17.1-300.fc45".to_owned(),
        "6.17.2-300.fc45".to_owned()
    )));
    let keys: Vec<(&str, &str)> = changes
        .iter()
        .map(|(name, arch, ..)| (name.as_str(), arch.as_str()))
        .collect();
    assert!(keys.is_sorted(), "sorted by (name, arch): {keys:?}");

    // The state is fresh, so this is refused without queueing and no bootc status runs.
    match p.apply(REMOTE).await {
        Err(ApexError::DigestMismatch(message)) => assert!(message.contains(STAGED)),
        other => panic!("expected DigestMismatch, got {other:?}"),
    }
    assert!(matches!(
        p.apply("latest; rm -rf /").await,
        Err(ApexError::InvalidArgument(_))
    ));

    p.report_boot(BootVerdict::Green, BOOTED)
        .await
        .expect("green from root");
    assert!(matches!(
        p.report_boot(BootVerdict::Fallback, "").await,
        Err(ApexError::InvalidArgument(_))
    ));
    h.fake.assert_finished();
}

#[tokio::test]
async fn invalid_strings_are_invalid_arguments() {
    let h = Harness::new();
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let verdict = client
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(INTERFACE),
            "ReportBoot",
            &("purple", ""),
        )
        .await;
    let error = ApexError::from(verdict.expect_err("refused"));
    assert_eq!(error.kind(), Some(ErrorKind::InvalidArgument));
    assert!(
        error.message().contains("is not a valid"),
        "{}",
        error.message()
    );
    let error = proxy(&client)
        .await
        .get_changes_for("latest")
        .await
        .expect_err("refused before any state is read");
    assert_eq!(error.kind(), Some(ErrorKind::InvalidArgument), "{error}");
    h.fake.assert_finished();
}

#[tokio::test]
async fn callers_are_authorized() {
    let h = Harness::new();
    h.refresh(&host(booted(), None, None)).await;
    let (_server, client) = connect(&h, FakeAuthorizer::deny_user(1000)).await;
    let p = proxy(&client).await;
    match p.check().await {
        Err(ApexError::NotAuthorized(message)) => {
            assert_eq!(message, "You are not allowed to check for updates.");
        }
        other => panic!("expected NotAuthorized, got {other:?}"),
    }
    assert!(matches!(
        p.rollback(true).await,
        Err(ApexError::NotAuthorized(_))
    ));
    assert!(matches!(
        p.automatic().await,
        Err(ApexError::NotAuthorized(_))
    ));
    assert!(matches!(
        p.report_boot(BootVerdict::Green, BOOTED).await,
        Err(ApexError::NotAuthorized(_))
    ));
    p.ping().await.expect("ping");
    p.get_changes().await.expect("changes");
    h.fake.assert_finished();

    let h = Harness::new();
    let (_server, client) = connect(&h, FakeAuthorizer::allow_user(1000)).await;
    let p = proxy(&client).await;
    assert!(matches!(
        p.automatic().await,
        Err(ApexError::NotAuthorized(_))
    ));
    h.fake.assert_finished();
}

#[tokio::test]
async fn property_changes_are_signalled() {
    let h = Harness::new();
    // Fresh, so reading the properties queues no refresh.
    h.refresh(&host(booted(), None, None)).await;
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let p = Update1Proxy::builder(&client).build().await.expect("proxy");
    let mut changes = p.receive_state_changed().await;
    let first = next(&mut changes).await;
    assert_eq!(first.get().await.unwrap(), State::UpToDate);

    h.push(status(&host(booted(), Some(staged(true)), None)));
    h.daemon.submit(Op::Refresh).await.expect("refresh");
    let update = next(&mut changes).await;
    assert_eq!(update.get().await.unwrap(), State::Staged);
    assert_eq!(p.cached_state().unwrap(), Some(State::Staged));
    h.fake.assert_finished();
}

async fn next<S: Stream + Unpin>(stream: &mut S) -> S::Item {
    tokio::time::timeout(
        Duration::from_secs(10),
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut *stream).poll_next(cx)),
    )
    .await
    .expect("a signal in time")
    .expect("stream open")
}

#[tokio::test]
async fn automatic_runs_for_root() {
    let h = Harness::new();
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let p = proxy(&client).await;
    let current = host(booted(), None, None);
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::UpgradeCheck).stdout("No changes in: x\n"))
        .push(status(&current));
    p.automatic().await.expect("automatic");
    assert_eq!(p.state().await.unwrap(), State::UpToDate);
    assert_eq!(p.last_check().await.unwrap(), NOW);
    h.fake.assert_finished();
}

#[tokio::test]
async fn each_method_asks_polkit_for_its_contract_action() {
    let h = Harness::new();
    h.refresh(&host(booted(), None, None)).await;
    let auth = FakeAuthorizer::deny_user(1000);
    let (_server, client) = connect(&h, auth.clone()).await;
    let p = proxy(&client).await;

    let refused = [
        ("Check", p.check().await),
        ("Download", p.download(false).await),
        ("Apply", p.apply(STAGED).await),
        ("Rollback", p.rollback(false).await),
        (
            "AcknowledgeHealthFailure",
            p.acknowledge_health_failure().await,
        ),
        (
            "SetAutomaticDownloads",
            p.set_automatic_downloads(false).await,
        ),
    ];
    for (member, result) in &refused {
        assert!(
            matches!(result, Err(ApexError::NotAuthorized(_))),
            "{member}: {result:?}"
        );
    }
    p.ping().await.expect("ping");
    p.get_changes().await.expect("changes");
    assert!(matches!(
        p.automatic().await,
        Err(ApexError::NotAuthorized(_))
    ));
    assert!(matches!(
        p.report_boot(BootVerdict::Green, BOOTED).await,
        Err(ApexError::NotAuthorized(_))
    ));

    let expected: Vec<(String, PolkitAction)> = refused
        .iter()
        .map(|(member, _)| {
            let action = PolkitAction::guarding(member).expect("a guarded method");
            ((*member).to_owned(), action)
        })
        .collect();
    assert_eq!(auth.requests(), expected);
    assert!(
        auth.requests()
            .contains(&("AcknowledgeHealthFailure".to_owned(), PolkitAction::Discard))
    );
    assert!(
        p.automatic_downloads().await.unwrap(),
        "a refused setter changes nothing"
    );
    h.fake.assert_finished();
}

type Seen = BTreeMap<String, Vec<OwnedVariant>>;

async fn watch_until(
    stream: &mut PropertiesChangedStream,
    seen: &mut Seen,
    done: impl Fn(&Seen) -> bool,
) {
    let read = async {
        while !done(seen) {
            let signal = next(stream).await;
            let args = signal.args().expect("PropertiesChanged arguments");
            assert_eq!(args.interface_name.as_str(), INTERFACE);
            assert!(
                args.invalidated_properties.is_empty(),
                "values are sent, never only invalidated"
            );
            for (name, value) in &args.changed_properties {
                seen.entry((*name).to_owned())
                    .or_default()
                    .push(value.try_to_owned().expect("owned value"));
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .is_err()
    {
        panic!("the expected signals did not arrive; seen: {seen:?}");
    }
}

fn texts(seen: &Seen, name: &str) -> Vec<String> {
    seen.get(name)
        .into_iter()
        .flatten()
        .map(|value| String::try_from(value.try_clone().expect("clone")).expect("a string"))
        .collect()
}

fn digests(seen: &Seen, name: &str) -> Vec<Option<String>> {
    seen.get(name)
        .into_iter()
        .flatten()
        .map(|value| {
            Deployment::try_from(value.try_clone().expect("clone"))
                .expect("a deployment")
                .digest
        })
        .collect()
}

fn progress(seen: &Seen) -> Vec<f64> {
    seen.get("Progress")
        .into_iter()
        .flatten()
        .map(|value| f64::try_from(value.try_clone().expect("clone")).expect("a double"))
        .collect()
}

fn testing_booted() -> apex_bootc::BootEntry {
    let mut entry = booted();
    if let Some(image) = &mut entry.image {
        image.image.image = APEX_TESTING.to_string();
    }
    entry
}

/// `HealthFailure` with its checks, `AutomaticDownloads` and `LastFallback`, after a
/// download staged `staged`.
async fn warning_and_setting_signals(
    h: &Harness,
    p: &Update1Proxy<'static>,
    stream: &mut PropertiesChangedStream,
    seen: &mut Seen,
    staged: apex_bootc::BootEntry,
) {
    h.store
        .write_health_failure(&apex_update::HealthFailureRecord {
            digest: BOOTED.into(),
            failed_checks: vec!["session".into()],
            at: NOW,
        })
        .unwrap();
    p.report_boot(BootVerdict::Red, BOOTED).await.expect("red");
    watch_until(stream, seen, |s| {
        texts(s, "HealthFailure").last().map(String::as_str) == Some(BOOTED)
            && s.contains_key("HealthFailureChecks")
    })
    .await;
    assert_eq!(
        p.health_failure_checks().await.expect("checks"),
        ["session"]
    );
    p.acknowledge_health_failure().await.expect("acknowledge");
    watch_until(stream, seen, |s| {
        texts(s, "HealthFailure").last().map(String::as_str) == Some("")
    })
    .await;
    assert_eq!(texts(seen, "HealthFailure"), [BOOTED, ""]);
    assert!(p.health_failure_checks().await.expect("checks").is_empty());
    assert_eq!(
        texts(seen, "State").last().map(String::as_str),
        Some("staged")
    );

    p.set_automatic_downloads(false)
        .await
        .expect("automatic downloads off");
    watch_until(stream, seen, |s| s.contains_key("AutomaticDownloads")).await;
    let staged_now = host(testing_booted(), Some(staged), Some(rollback()));
    h.push(status(&staged_now)).push(status(&staged_now));
    p.report_boot(BootVerdict::Fallback, ROLLBACK)
        .await
        .expect("fallback");
    watch_until(stream, seen, |s| {
        texts(s, "LastFallback").last().map(String::as_str) == Some(ROLLBACK)
    })
    .await;
}

/// A deployment the daemon pinned, now fourth in line.
async fn kept_signal(h: &Harness, stream: &mut PropertiesChangedStream, seen: &mut Seen) {
    const KEPT: &str = "sha256:7777777777777777777777777777777777777777777777777777777777777777";
    const KEPT_CSUM: &str = "6666666666666666666666666666666666666666666666666666666666666666";
    h.store
        .write_pins(&apex_update::Pins {
            pins: vec![apex_update::PinRecord {
                checksum: KEPT_CSUM.into(),
                serial: 0,
                digest: KEPT.into(),
                at: NOW,
            }],
        })
        .unwrap();
    let mut current = host(testing_booted(), None, Some(rollback()));
    current.status.other_deployments = vec![apex_bootc::BootEntry {
        pinned: true,
        ..entry(KEPT, "45.20260901.1", KEPT_CSUM)
    }];
    h.refresh(&current).await;
    watch_until(stream, seen, |s| s.contains_key("Kept")).await;
    let zbus::Variant::Array(array) = &**seen["Kept"].last().expect("a value") else {
        panic!("Kept is an array");
    };
    let kept: Vec<Deployment> = array
        .iter()
        .map(|dict| Deployment::try_from(dict).expect("a deployment dict"))
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].digest.as_deref(), Some(KEPT));
    assert!(kept[0].pin_reason.is_some());
}

#[tokio::test]
async fn every_property_signals_its_changes() {
    const REMOTE_VERSION: &str = "45.20260924.9";
    let mut state = fake_state();
    // Sets DeferredReason.
    state.metered = Some(true);
    let h = Harness::with(state, SharedGrubWriter::new());
    h.last_check(REMOTE, REMOTE_VERSION, 120);
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let properties = properties_proxy(&client).await;
    let mut stream = properties.receive_properties_changed().await.unwrap();
    let p = proxy(&client).await;
    let mut seen = Seen::new();

    let before = host(testing_booted(), None, Some(rollback()));
    h.refresh(&before).await;
    watch_until(&mut stream, &mut seen, |s| {
        [
            "State",
            "Booted",
            "Rollback",
            "Available",
            "Channel",
            "LastCheck",
            "DeferredReason",
        ]
        .iter()
        .all(|name| s.contains_key(*name))
    })
    .await;
    assert_eq!(texts(&seen, "State"), ["available"]);
    assert_eq!(texts(&seen, "Channel"), ["testing"]);
    assert_eq!(digests(&seen, "Available"), [Some(REMOTE.to_owned())]);

    let staged_remote = apex_bootc::BootEntry {
        download_only: true,
        ..entry(REMOTE, REMOTE_VERSION, STAGED_CSUM)
    };
    h.push(status(&before))
        .push(
            Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: true })
                .progress_bytes(50, 100)
                .delay(Duration::from_secs(1)),
        )
        .push(status(&host(
            testing_booted(),
            Some(staged_remote.clone()),
            Some(rollback()),
        )));
    p.download(true).await.expect("download");
    watch_until(&mut stream, &mut seen, |s| {
        digests(s, "Staged").last() == Some(&Some(REMOTE.to_owned()))
            && progress(s).last().is_some_and(|p| *p < 0.0)
    })
    .await;
    assert!(
        progress(&seen).iter().any(|p| (p - 0.45).abs() < 1e-9),
        "{:?}",
        progress(&seen)
    );
    assert!(texts(&seen, "State").contains(&"downloading".to_owned()));
    assert_eq!(digests(&seen, "Available").last(), Some(&None));

    h.push(
        Scripted::new(BootcCommand::UpgradeCheck)
            .exit_code(1)
            .stderr("error: Fetching: dial tcp: lookup ghcr.io: no such host\n"),
    );
    assert!(matches!(p.check().await, Err(ApexError::Network(_))));
    watch_until(&mut stream, &mut seen, |s| s.contains_key("LastError")).await;
    assert!(texts(&seen, "LastError")[0].contains("no such host"));

    warning_and_setting_signals(&h, &p, &mut stream, &mut seen, staged_remote).await;
    kept_signal(&h, &mut stream, &mut seen).await;

    let signalled: BTreeSet<String> = seen.keys().cloned().collect();
    let contract: BTreeSet<String> = members(INTROSPECTION_XML)
        .into_keys()
        .filter_map(|key| key.strip_prefix("property ").map(str::to_owned))
        .collect();
    assert_eq!(contract.len(), 15);
    assert_eq!(signalled, contract);
    h.fake.assert_finished();
}

#[tokio::test]
async fn reads_right_after_activation_wait_for_the_first_refresh() {
    let h = Harness::new();
    h.push(
        status(&host(booted(), Some(staged(true)), Some(rollback())))
            .delay(Duration::from_millis(500)),
    );
    // As main does: the startup refresh is queued, not awaited.
    drop(h.daemon.queue().submit(Op::Refresh));
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let p = proxy(&client).await;
    assert_eq!(p.state().await.unwrap(), State::Staged);
    assert_eq!(p.staged().await.unwrap().digest.as_deref(), Some(STAGED));
    assert_eq!(p.booted().await.unwrap().digest.as_deref(), Some(BOOTED));
    assert!(!p.get_changes().await.unwrap().is_empty());
    h.fake.assert_finished();

    // A caching proxy fills its cache with GetAll at its first read.
    let h = Harness::new();
    h.push(status(&host(booted(), Some(staged(true)), None)).delay(Duration::from_millis(500)));
    drop(h.daemon.queue().submit(Op::Refresh));
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let cached = Update1Proxy::builder(&client).build().await.expect("proxy");
    assert_eq!(cached.state().await.unwrap(), State::Staged);
    assert_eq!(cached.cached_state().unwrap(), Some(State::Staged));
    assert_eq!(
        cached.cached_staged().unwrap().and_then(|s| s.digest),
        Some(STAGED.to_owned())
    );
    h.fake.assert_finished();
}

#[tokio::test]
async fn a_new_instance_signals_every_property_once() {
    let h = Harness::new();
    h.refresh(&host(booted(), None, None)).await;
    let (_server, _client, first) = connect_announced(&h, FakeAuthorizer::allow_all()).await;
    let contract: BTreeSet<String> = members(INTROSPECTION_XML)
        .into_keys()
        .filter_map(|key| key.strip_prefix("property ").map(str::to_owned))
        .collect();
    assert_eq!(first.keys().cloned().collect::<BTreeSet<_>>(), contract);
    let text = |name: &str| String::try_from(first[name].try_clone().unwrap()).unwrap();
    assert_eq!(text("LastError"), "");
    assert_eq!(text("HealthFailure"), "");
    assert_eq!(text("State"), "up-to-date");
    let staged = Deployment::try_from(first["Staged"].try_clone().unwrap()).unwrap();
    assert!(staged.is_empty());
    h.fake.assert_finished();
}

#[tokio::test]
async fn control_characters_never_reach_the_wire() {
    let h = Harness::new();
    h.refresh(&host(booted(), None, None)).await;
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let p = proxy(&client).await;
    h.push(
        Scripted::new(BootcCommand::UpgradeCheck)
            .exit_code(1)
            .stderr("error: Fetching: manifest unknown: x\0\r\u{9b}2J\u{7}y\n"),
    );
    let message = match p.check().await {
        Err(ApexError::Bootc(message) | ApexError::Network(message)) => message,
        other => panic!("expected a contract error, got {other:?}"),
    };
    assert!(message.contains("manifest unknown"), "{message:?}");
    assert!(!message.chars().any(char::is_control), "{message:?}");
    let last_error = p.last_error().await.expect("LastError decodes");
    assert!(last_error.contains("manifest unknown"), "{last_error:?}");
    assert!(!last_error.chars().any(char::is_control), "{last_error:?}");
    h.fake.assert_finished();
}

#[tokio::test]
async fn a_burst_of_bogus_applies_neither_queues_nor_starves_the_automatic_run() {
    let h = Harness::new();
    let current = host(booted(), Some(staged(true)), None);
    h.refresh(&current).await;
    let (_server, client) = connect(&h, FakeAuthorizer::allow_all()).await;
    let p = proxy(&client).await;
    for i in 0..50u8 {
        let digest = format!("sha256:{i:064x}");
        assert!(matches!(
            p.apply(&digest).await,
            Err(ApexError::DigestMismatch(_))
        ));
    }
    assert_eq!(h.fake.calls().len(), 1, "only the refresh ran");
    h.push(status(&current))
        .push(Scripted::new(BootcCommand::UpgradeCheck).stdout("No changes in: x\n"))
        .push(status(&current));
    p.automatic().await.expect("automatic");
    h.fake.assert_finished();
}
