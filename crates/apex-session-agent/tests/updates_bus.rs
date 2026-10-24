#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::rc::Rc;
use std::time::Duration;

use apex_session_agent::agent::Agent;
use apex_session_agent::failure;
use apex_session_agent::notifications::DbusNotifier;
use apex_session_agent::paths::Paths;
use apex_session_agent::update1;
use apex_session_agent::updates::{ACTION_KEY, ACTION_LABEL, DEFAULT_ACTION_KEY, SUMMARY};
use common::{Bus, settings_from_the_compiled_schemas, wait_for};
use gio::prelude::*;

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const SETTINGS: &str = "org.gnome.Settings";
const SETTINGS_PATH: &str = "/org/gnome/Settings";
const APPLICATION: &str = "org.freedesktop.Application";

const XML: &str = r#"<node>
  <interface name="org.freedesktop.Notifications">
    <method name="Notify">
      <arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/>
      <arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/>
      <arg type="a{sv}" direction="in"/><arg type="i" direction="in"/>
      <arg type="u" direction="out"/>
    </method>
    <method name="CloseNotification"><arg type="u" direction="in"/></method>
    <signal name="ActionInvoked"><arg type="u"/><arg type="s"/></signal>
    <signal name="NotificationClosed"><arg type="u"/><arg type="u"/></signal>
  </interface>
  <interface name="io.github.apex_linux.Apex.Update1">
    <property name="Staged" type="a{sv}" access="read"/>
    <property name="HealthFailure" type="s" access="read"/>
    <property name="LastFallback" type="s" access="read"/>
  </interface>
  <interface name="org.freedesktop.Application">
    <method name="ActivateAction">
      <arg type="s" direction="in"/><arg type="av" direction="in"/>
      <arg type="a{sv}" direction="in"/>
    </method>
  </interface>
</node>"#;

#[derive(Debug, Default)]
struct Seen {
    /// Summary, body and actions of each `Notify`.
    posted: Vec<(String, String, Vec<String>)>,
    closed: Vec<u32>,
    activated: Vec<glib::Variant>,
}

fn interface(name: &str) -> gio::DBusInterfaceInfo {
    gio::DBusNodeInfo::for_xml(XML)
        .unwrap()
        .lookup_interface(name)
        .unwrap()
}

fn own(connection: &gio::DBusConnection, name: &str) -> gio::OwnerId {
    gio::bus_own_name_on_connection(
        connection,
        name,
        gio::BusNameOwnerFlags::NONE,
        |_, _| {},
        |_, _| {},
    )
}

/// Answers every `Notify` with the next id from 7.
fn serve_notifications(
    connection: &gio::DBusConnection,
    seen: &Rc<RefCell<Seen>>,
) -> gio::RegistrationId {
    let seen = Rc::clone(seen);
    connection
        .register_object(NOTIFICATIONS_PATH, &interface(NOTIFICATIONS))
        .method_call(move |_, _, _, _, method, parameters, invocation| {
            let mut seen = seen.borrow_mut();
            if method == "Notify" {
                seen.posted.push((
                    parameters.child_value(3).get().unwrap(),
                    parameters.child_value(4).get().unwrap(),
                    parameters.child_value(5).get().unwrap(),
                ));
                let id = 6 + u32::try_from(seen.posted.len()).unwrap();
                invocation.return_value(Some(&(id,).to_variant()));
            } else {
                seen.closed.push(parameters.child_value(0).get().unwrap());
                invocation.return_value(None);
            }
        })
        .build()
        .unwrap()
}

fn serve_settings(connection: &gio::DBusConnection, seen: &Rc<RefCell<Seen>>) {
    let seen = Rc::clone(seen);
    connection
        .register_object(SETTINGS_PATH, &interface(APPLICATION))
        .method_call(move |_, _, _, _, _, parameters, invocation| {
            seen.borrow_mut().activated.push(parameters);
            invocation.return_value(None);
        })
        .build()
        .unwrap();
}

fn staged(digest: &str, version: &str) -> glib::Variant {
    let mut dict: HashMap<String, glib::Variant> = HashMap::new();
    if !digest.is_empty() {
        dict.insert("digest".into(), digest.to_variant());
        dict.insert("version".into(), version.to_variant());
        dict.insert("download-only".into(), true.to_variant());
    }
    dict.to_variant()
}

type Update1Properties = Rc<RefCell<HashMap<String, glib::Variant>>>;

fn update1_properties(staged: glib::Variant) -> Update1Properties {
    Rc::new(RefCell::new(HashMap::from([
        ("Staged".to_owned(), staged),
        ("HealthFailure".to_owned(), "".to_variant()),
        ("LastFallback".to_owned(), "".to_variant()),
    ])))
}

/// A fake Update1 that serves its properties and counts the reads.
fn serve_update1(
    connection: &gio::DBusConnection,
    current: &Update1Properties,
    reads: &Rc<Cell<u32>>,
) {
    let current = Rc::clone(current);
    let reads = Rc::clone(reads);
    connection
        .register_object(update1::PATH, &interface(update1::INTERFACE))
        .property(move |_, _, _, _, name| {
            reads.set(reads.get() + 1);
            current.borrow()[name].clone()
        })
        .build()
        .unwrap();
}

fn stage(connection: &gio::DBusConnection, current: &Update1Properties, value: glib::Variant) {
    change(connection, current, "Staged", value);
}

fn change(
    connection: &gio::DBusConnection,
    current: &Update1Properties,
    name: &str,
    value: glib::Variant,
) {
    current.borrow_mut().insert(name.to_owned(), value.clone());
    let changed: HashMap<String, glib::Variant> = HashMap::from([(name.to_owned(), value)]);
    connection
        .emit_signal(
            None,
            update1::PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            Some(&(update1::INTERFACE, changed, Vec::<String>::new()).to_variant()),
        )
        .unwrap();
}

fn settle() {
    let context = glib::MainContext::default();
    for _ in 0..50 {
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Running {
    agent: Rc<Agent>,
    _notifications: apex_session_agent::notifications::Subscription,
    _update1: apex_session_agent::notifications::Subscription,
}

fn start(bus: &Bus, dir: &std::path::Path, paths: &Paths) -> Running {
    let (settings, _) = settings_from_the_compiled_schemas(dir);
    let session = bus.connect();
    let agent = Agent::start(
        settings,
        None,
        paths.clone(),
        Box::new(DbusNotifier::new(session.clone())),
    );
    let notifications = DbusNotifier::new(session).subscribe(&agent);
    let update1 = update1::subscribe(&bus.connect(), &agent);
    Running {
        agent,
        _notifications: notifications,
        _update1: update1,
    }
}

struct Fixture<'a> {
    bus: &'a Bus,
    dir: &'a std::path::Path,
    paths: &'a Paths,
    daemon: &'a gio::DBusConnection,
    current: &'a Update1Properties,
    reads: &'a Rc<Cell<u32>>,
    shell: &'a gio::DBusConnection,
    seen: &'a Rc<RefCell<Seen>>,
}

/// A restarted agent leaves the posted digest alone, posts the next one and withdraws it.
fn restart_and_next_digest(f: &Fixture<'_>, running: Running, c: &str) -> Running {
    drop(running);
    let reads_before = f.reads.get();
    let restarted = start(f.bus, &f.dir.join("second"), f.paths);
    wait_for("the restarted agent's read", || {
        f.reads.get() > reads_before
    });
    settle();
    assert_eq!(
        f.seen.borrow().posted.len(),
        1,
        "the same digest is not posted again"
    );
    assert_eq!(restarted.agent.updates_ready(), None);
    stage(f.daemon, f.current, staged(c, "45.20260926.119"));
    wait_for("Updates Ready for the next digest", || {
        restarted.agent.updates_ready() == Some(8)
    });
    assert_eq!(
        f.seen.borrow().posted[1].1,
        "Apex 45.20260926.119 is downloaded and ready to install."
    );
    assert_eq!(
        fs::read_to_string(f.paths.updates_notified()).unwrap(),
        format!("{c}\n")
    );

    stage(f.daemon, f.current, staged("", ""));
    wait_for("the notification withdrawn with the stage", || {
        f.seen.borrow().closed == [8]
    });
    assert_eq!(restarted.agent.updates_ready(), None);
    restarted
}

/// After the Updates Ready part: notification ids 7 and 8 are taken, and 8 was withdrawn.
fn failure_notices(f: &Fixture<'_>, restarted: Running, b: &str, c: &str) {
    change(f.daemon, f.current, "HealthFailure", c.to_variant());
    wait_for("the health-failure notice", || {
        restarted.agent.update_failed() == Some(9)
    });
    assert_eq!(f.seen.borrow().posted[2].0, failure::HEALTH_SUMMARY);
    assert_eq!(
        fs::read_to_string(f.paths.failure_notified()).unwrap(),
        format!("{c}\n")
    );
    change(f.daemon, f.current, "LastFallback", c.to_variant());
    settle();
    assert_eq!(f.seen.borrow().posted.len(), 3, "one notice per digest");
    f.shell
        .emit_signal(
            None,
            NOTIFICATIONS_PATH,
            NOTIFICATIONS,
            "ActionInvoked",
            Some(&(9u32, ACTION_KEY).to_variant()),
        )
        .unwrap();
    wait_for("Software Updates to open", || {
        f.seen.borrow().activated.len() == 2
    });
    let target = f.seen.borrow().activated[1]
        .child_value(1)
        .child_value(0)
        .as_variant()
        .unwrap();
    assert_eq!(target.child_value(0).get::<String>().unwrap(), "system");
    let page = target.child_value(1).get::<Vec<glib::Variant>>().unwrap();
    assert_eq!(page[0].get::<String>().as_deref(), Some("software-updates"));

    drop(restarted);
    let reads_before = f.reads.get();
    let again = start(f.bus, &f.dir.join("third"), f.paths);
    wait_for("the third agent's read", || f.reads.get() > reads_before);
    settle();
    assert_eq!(f.seen.borrow().posted.len(), 3, "not again after a restart");
    assert_eq!(again.agent.update_failed(), None);

    change(f.daemon, f.current, "HealthFailure", "".to_variant());
    change(f.daemon, f.current, "LastFallback", b.to_variant());
    wait_for("a notice for the next failed update", || {
        again.agent.update_failed() == Some(10)
    });
    assert_eq!(f.seen.borrow().posted[3].0, failure::FALLBACK_SUMMARY);
    change(f.daemon, f.current, "LastFallback", "".to_variant());
    wait_for("the notice withdrawn when acknowledged", || {
        f.seen.borrow().closed == [8, 10]
    });
}

/// One test: the main context and the buses belong to one thread.
#[test]
fn updates_ready_over_a_private_bus() {
    let bus = Bus::start();
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(
        dir.path().join("config"),
        dir.path().join("data"),
        dir.path().join("state"),
    );
    let b = format!("sha256:{}", "b".repeat(64));
    let c = format!("sha256:{}", "c".repeat(64));

    let daemon = bus.connect();
    let current = update1_properties(staged(&b, "45.20260925.118"));
    let reads = Rc::new(Cell::new(0));
    serve_update1(&daemon, &current, &reads);
    let _update1_name = own(&daemon, update1::BUS_NAME);

    let running = start(&bus, &dir.path().join("first"), &paths);
    wait_for("the staged update read", || reads.get() > 0);
    settle();
    assert_eq!(
        running.agent.updates_ready(),
        None,
        "no notification server yet"
    );
    assert!(!paths.updates_notified().exists());

    let shell = bus.connect();
    let seen = Rc::new(RefCell::new(Seen::default()));
    let registration = serve_notifications(&shell, &seen);
    serve_settings(&shell, &seen);
    let notifications_name = own(&shell, NOTIFICATIONS);
    let _settings_name = own(&shell, SETTINGS);
    wait_for("Updates Ready once the server owns its name", || {
        running.agent.updates_ready() == Some(7)
    });
    assert_eq!(
        seen.borrow().posted,
        [(
            SUMMARY.to_owned(),
            "Apex 45.20260925.118 is downloaded and ready to install.".to_owned(),
            [DEFAULT_ACTION_KEY, ACTION_LABEL, ACTION_KEY, ACTION_LABEL]
                .map(str::to_owned)
                .to_vec(),
        )]
    );
    assert_eq!(
        fs::read_to_string(paths.updates_notified()).unwrap(),
        format!("{b}\n")
    );

    shell
        .emit_signal(
            None,
            NOTIFICATIONS_PATH,
            NOTIFICATIONS,
            "ActionInvoked",
            Some(&(7u32, ACTION_KEY).to_variant()),
        )
        .unwrap();
    wait_for("Settings to open", || !seen.borrow().activated.is_empty());
    let activated = seen.borrow().activated[0].clone();
    assert_eq!(
        activated.child_value(0).get::<String>().unwrap(),
        "launch-panel"
    );
    let target = activated
        .child_value(1)
        .child_value(0)
        .as_variant()
        .unwrap();
    assert_eq!(target.type_().as_str(), "(sav)");
    assert_eq!(target.child_value(0).get::<String>().unwrap(), "system");

    let fixture = Fixture {
        bus: &bus,
        dir: dir.path(),
        paths: &paths,
        daemon: &daemon,
        current: &current,
        reads: &reads,
        shell: &shell,
        seen: &seen,
    };
    let restarted = restart_and_next_digest(&fixture, running, &c);
    failure_notices(&fixture, restarted, &b, &c);

    gio::bus_unown_name(notifications_name);
    shell.unregister_object(registration).unwrap();
}
