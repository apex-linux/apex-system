#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

mod common;

use std::cell::RefCell;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;

use apex_session_agent::LAYER_KEY;
use apex_session_agent::agent::Agent;
use apex_session_agent::guard::{ACTION_KEY, ACTION_LABEL};
use apex_session_agent::notifications::DbusNotifier;
use apex_session_agent::paths::Paths;
use common::{settings_from_the_compiled_schema, wait_for};
use gio::prelude::*;

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

const SERVER_XML: &str = r#"<node>
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
</node>"#;

struct Bus {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl Bus {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bus.conf");
        fs::write(
            &config,
            format!(
                "<busconfig><listen>unix:dir={}</listen><auth>EXTERNAL</auth>\
                 <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
                 <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
                dir.path().display()
            ),
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon (Fedora: dbus-daemon) must be installed");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().to_owned(),
            _dir: dir,
        }
    }

    fn connect(&self) -> gio::DBusConnection {
        gio::DBusConnection::for_address_sync(
            &self.address,
            gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
            None,
            gio::Cancellable::NONE,
        )
        .unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Default)]
struct Seen {
    actions: Vec<Vec<String>>,
    closed: Vec<u32>,
}

/// A notification server that answers every `Notify` with the next id from 7.
fn serve(connection: &gio::DBusConnection, seen: &Rc<RefCell<Seen>>) -> gio::RegistrationId {
    let node = gio::DBusNodeInfo::for_xml(SERVER_XML).unwrap();
    let interface = node.lookup_interface(NAME).unwrap();
    let seen = Rc::clone(seen);
    connection
        .register_object(PATH, &interface)
        .method_call(move |_, _, _, _, method, parameters, invocation| {
            let mut seen = seen.borrow_mut();
            if method == "Notify" {
                seen.actions.push(parameters.child_value(5).get().unwrap());
                let id = 6 + u32::try_from(seen.actions.len()).unwrap();
                invocation.return_value(Some(&(id,).to_variant()));
            } else {
                seen.closed.push(parameters.child_value(0).get().unwrap());
                invocation.return_value(None);
            }
        })
        .build()
        .unwrap()
}

/// One test: the main context, the bus and the name watch belong to one thread.
#[test]
fn the_offer_over_a_private_session_bus() {
    let bus = Bus::start();
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("config"), dir.path().join("state"));
    let settings = settings_from_the_compiled_schema(dir.path());
    settings.set_boolean(LAYER_KEY, false).unwrap();
    fs::create_dir_all(paths.marker_dir()).unwrap();
    fs::write(paths.guard_marker(), "").unwrap();

    let client = bus.connect();
    let agent = Agent::start(
        settings.clone(),
        paths.clone(),
        Box::new(DbusNotifier::new(client.clone())),
    );
    let subscription = DbusNotifier::new(client).subscribe(&agent);
    let context = glib::MainContext::default();
    for _ in 0..50 {
        context.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(agent.offered(), None, "no notification server yet");

    let server = bus.connect();
    let seen = Rc::new(RefCell::new(Seen::default()));
    let registration = serve(&server, &seen);
    let owner = gio::bus_own_name_on_connection(
        &server,
        NAME,
        gio::BusNameOwnerFlags::NONE,
        |_, _| {},
        |_, _| {},
    );
    wait_for("the offer once the server owns its name", || {
        agent.offered() == Some(7)
    });
    assert_eq!(
        seen.borrow().actions,
        [vec![ACTION_KEY.to_owned(), ACTION_LABEL.to_owned()]]
    );

    server
        .emit_signal(
            None,
            PATH,
            NAME,
            "ActionInvoked",
            Some(&(8u32, ACTION_KEY).to_variant()),
        )
        .unwrap();
    server
        .emit_signal(
            None,
            PATH,
            NAME,
            "ActionInvoked",
            Some(&(7u32, ACTION_KEY).to_variant()),
        )
        .unwrap();
    wait_for("the action to turn the layer on", || {
        !paths.guard_marker().exists()
    });
    assert!(settings.boolean(LAYER_KEY));
    server
        .emit_signal(
            None,
            PATH,
            NAME,
            "NotificationClosed",
            Some(&(7u32, 2u32).to_variant()),
        )
        .unwrap();

    fs::write(paths.guard_marker(), "").unwrap();
    wait_for("an offer for the next trip", || agent.offered() == Some(8));
    gio::bus_unown_name(owner);
    server.unregister_object(registration).unwrap();
    wait_for("the offer forgotten with the server", || {
        agent.offered().is_none()
    });
    drop(subscription);
}
