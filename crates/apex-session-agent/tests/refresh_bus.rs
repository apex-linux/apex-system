#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use apex_session_agent::REFRESH_KEY;
use apex_session_agent::display::{
    MUTTER_INTERFACE, MUTTER_NAME, MUTTER_PATH, RefreshRate, UPOWER_INTERFACE, UPOWER_NAME,
    UPOWER_PATH,
};
use apex_session_agent::refresh::METHOD_TEMPORARY;
use common::{Bus, settings_from_the_compiled_schemas, wait_for};
use gio::prelude::*;

const XML: &str = r#"<node>
  <interface name="org.gnome.Mutter.DisplayConfig">
    <method name="GetCurrentState">
      <arg type="u" direction="out"/>
      <arg type="a((ssss)a(siiddada{sv})a{sv})" direction="out"/>
      <arg type="a(iiduba(ssss)a{sv})" direction="out"/>
      <arg type="a{sv}" direction="out"/>
    </method>
    <method name="ApplyMonitorsConfig">
      <arg type="u" direction="in"/><arg type="u" direction="in"/>
      <arg type="a(iiduba(ssa{sv}))" direction="in"/><arg type="a{sv}" direction="in"/>
    </method>
    <signal name="MonitorsChanged"/>
  </interface>
  <interface name="org.freedesktop.UPower">
    <property name="OnBattery" type="b" access="read"/>
  </interface>
</node>"#;

type Spec = (String, String, String, String);
type Props = HashMap<String, glib::Variant>;
type Applied = (
    u32,
    u32,
    Vec<(i32, i32, f64, u32, bool, Vec<(String, String, Props)>)>,
    Props,
);

struct FakeMonitor {
    connector: &'static str,
    builtin: bool,
    modes: Vec<(&'static str, i32, i32, f64)>,
    current: String,
    x: i32,
}

struct FakeMutter {
    serial: u32,
    monitors: Vec<FakeMonitor>,
    reads: u32,
    applied: Vec<Applied>,
}

fn spec(connector: &str) -> Spec {
    (
        connector.to_owned(),
        "BOE".to_owned(),
        "0x0a1b".to_owned(),
        "0".to_owned(),
    )
}

fn flag(key: &str, value: bool) -> Props {
    HashMap::from([(key.to_owned(), value.to_variant())])
}

impl FakeMutter {
    fn state(&self) -> glib::Variant {
        let monitors: Vec<_> = self
            .monitors
            .iter()
            .map(|m| {
                let modes: Vec<_> = m
                    .modes
                    .iter()
                    .map(|&(id, w, h, r)| {
                        (
                            id.to_owned(),
                            w,
                            h,
                            r,
                            1.5f64,
                            vec![1.0f64, 1.5],
                            flag("is-current", id == m.current),
                        )
                    })
                    .collect();
                (spec(m.connector), modes, flag("is-builtin", m.builtin))
            })
            .collect();
        let logical: Vec<_> = self
            .monitors
            .iter()
            .map(|m| {
                (
                    m.x,
                    0i32,
                    if m.builtin { 1.5f64 } else { 1.0 },
                    0u32,
                    m.builtin,
                    vec![spec(m.connector)],
                    Props::new(),
                )
            })
            .collect();
        let properties: Props = HashMap::from([("layout-mode".to_owned(), 1u32.to_variant())]);
        (self.serial, monitors, logical, properties).to_variant()
    }
}

fn panel(current: &str) -> FakeMonitor {
    FakeMonitor {
        connector: "eDP-1",
        builtin: true,
        modes: vec![
            ("2560x1600@90.002", 2560, 1600, 90.002),
            ("2560x1600@60.000", 2560, 1600, 60.0),
            ("1920x1200@60.000", 1920, 1200, 60.0),
        ],
        current: current.to_owned(),
        x: 0,
    }
}

fn external(x: i32) -> FakeMonitor {
    FakeMonitor {
        connector: "HDMI-1",
        builtin: false,
        modes: vec![
            ("1920x1080@144.000", 1920, 1080, 144.0),
            ("1920x1080@60.000", 1920, 1080, 60.0),
        ],
        current: "1920x1080@144.000".to_owned(),
        x,
    }
}

fn interface(name: &str) -> gio::DBusInterfaceInfo {
    gio::DBusNodeInfo::for_xml(XML)
        .unwrap()
        .lookup_interface(name)
        .unwrap()
}

/// A mutter that applies what it is sent and signals `MonitorsChanged`, like the real one.
fn serve_mutter(connection: &gio::DBusConnection, mutter: &Rc<RefCell<FakeMutter>>) {
    let mutter = Rc::clone(mutter);
    connection
        .register_object(MUTTER_PATH, &interface(MUTTER_INTERFACE))
        .method_call(move |connection, _, _, _, method, parameters, invocation| {
            if method == "GetCurrentState" {
                let mut mutter = mutter.borrow_mut();
                mutter.reads += 1;
                invocation.return_value(Some(&mutter.state()));
                return;
            }
            let applied = parameters.get::<Applied>().unwrap();
            {
                let mut mutter = mutter.borrow_mut();
                for (_, _, _, _, _, monitors) in &applied.2 {
                    for (connector, mode, _) in monitors {
                        let monitor = mutter
                            .monitors
                            .iter_mut()
                            .find(|m| m.connector == connector)
                            .unwrap();
                        monitor.current.clone_from(mode);
                    }
                }
                mutter.serial += 1;
                mutter.applied.push(applied);
            }
            invocation.return_value(None);
            connection
                .emit_signal(None, MUTTER_PATH, MUTTER_INTERFACE, "MonitorsChanged", None)
                .unwrap();
        })
        .build()
        .unwrap();
}

fn serve_upower(connection: &gio::DBusConnection, on_battery: &Rc<Cell<bool>>) {
    let on_battery = Rc::clone(on_battery);
    connection
        .register_object(UPOWER_PATH, &interface(UPOWER_INTERFACE))
        .property(move |_, _, _, _, _| on_battery.get().to_variant())
        .build()
        .unwrap();
}

fn set_on_battery(connection: &gio::DBusConnection, cell: &Cell<bool>, value: bool) {
    cell.set(value);
    let changed: Props = HashMap::from([("OnBattery".to_owned(), value.to_variant())]);
    connection
        .emit_signal(
            None,
            UPOWER_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            Some(&(UPOWER_INTERFACE, changed, Vec::<String>::new()).to_variant()),
        )
        .unwrap();
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

fn settle() {
    let context = glib::MainContext::default();
    for _ in 0..50 {
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn panel_mode(mutter: &RefCell<FakeMutter>) -> String {
    mutter.borrow().monitors[0].current.clone()
}

/// One test: the main context and the bus belong to one thread.
#[test]
fn the_panel_rate_follows_the_power_source() {
    let bus = Bus::start();
    let dir = tempfile::tempdir().unwrap();
    let (settings, _) = settings_from_the_compiled_schemas(dir.path());
    assert!(settings.boolean(REFRESH_KEY), "on by default");

    let server = bus.connect();
    let mutter = Rc::new(RefCell::new(FakeMutter {
        serial: 1,
        monitors: vec![panel("2560x1600@90.002")],
        reads: 0,
        applied: Vec::new(),
    }));
    let on_battery = Rc::new(Cell::new(false));
    serve_mutter(&server, &mutter);
    serve_upower(&server, &on_battery);
    let _mutter_name = own(&server, MUTTER_NAME);
    let _upower_name = own(&server, UPOWER_NAME);

    let refresh = RefreshRate::start(
        settings.clone(),
        REFRESH_KEY,
        bus.connect(),
        Some(&bus.connect()),
    );
    wait_for("the first reads", || mutter.borrow().reads >= 2);
    settle();
    assert!(
        mutter.borrow().applied.is_empty(),
        "on AC at the highest rate already"
    );

    set_on_battery(&server, &on_battery, true);
    wait_for("battery to lower the rate", || {
        panel_mode(&mutter) == "2560x1600@60.000"
    });
    {
        let mutter = mutter.borrow();
        let (serial, method, logical, properties) = &mutter.applied[0];
        assert_eq!((*serial, *method), (1, METHOD_TEMPORARY));
        let (x, y, scale, transform, primary, monitors) = &logical[0];
        assert_eq!((*x, *y, *transform, *primary), (0, 0, 0, true));
        assert!((scale - 1.5).abs() < 1e-9, "the scale is kept");
        assert_eq!(monitors[0].0, "eDP-1");
        assert_eq!(properties["layout-mode"].get::<u32>(), Some(1));
    }

    set_on_battery(&server, &on_battery, false);
    wait_for("AC to restore the rate", || {
        panel_mode(&mutter) == "2560x1600@90.002"
    });

    set_on_battery(&server, &on_battery, true);
    wait_for("battery again", || {
        panel_mode(&mutter) == "2560x1600@60.000"
    });
    settings.set_boolean(REFRESH_KEY, false).unwrap();
    wait_for("the key turned off to restore the rate", || {
        panel_mode(&mutter) == "2560x1600@90.002"
    });
    settings.set_boolean(REFRESH_KEY, true).unwrap();
    wait_for("the key turned on to lower it", || {
        panel_mode(&mutter) == "2560x1600@60.000"
    });
    let applied = mutter.borrow().applied.len();
    assert_eq!(applied, 5);

    drop(refresh);
    let reads = mutter.borrow().reads;
    let restarted = RefreshRate::start(
        settings.clone(),
        REFRESH_KEY,
        bus.connect(),
        Some(&bus.connect()),
    );
    wait_for("the restarted reads", || mutter.borrow().reads >= reads + 2);
    settle();
    assert_eq!(
        mutter.borrow().applied.len(),
        applied,
        "no write when the mode is already right"
    );
    drop(restarted);

    {
        let mut mutter = mutter.borrow_mut();
        mutter.monitors = vec![external(0)];
        mutter.applied.clear();
    }
    set_on_battery(&server, &on_battery, false);
    let reads = mutter.borrow().reads;
    let external_only =
        RefreshRate::start(settings, REFRESH_KEY, bus.connect(), Some(&bus.connect()));
    wait_for("the external-only reads", || {
        mutter.borrow().reads >= reads + 2
    });
    set_on_battery(&server, &on_battery, true);
    wait_for("the battery read", || mutter.borrow().reads >= reads + 3);
    settle();
    assert!(
        mutter.borrow().applied.is_empty(),
        "an external monitor alone is left alone"
    );
    assert_eq!(mutter.borrow().monitors[0].current, "1920x1080@144.000");
    drop(external_only);
}
