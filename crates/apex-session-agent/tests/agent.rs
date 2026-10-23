#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use apex_session_agent::agent::{Agent, Notifier, Posted};
use apex_session_agent::guard::ACTION_KEY;
use apex_session_agent::paths::Paths;
use apex_session_agent::{LAYER_KEY, RADIUS_KEY, SCHEMA_ID};
use gio::prelude::*;

/// A copy of apex-desktop's `desktop/schemas/` file, which this repository does not hold.
const SCHEMA_XML: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/io.github.apex_linux.Apex.Desktop.gschema.xml"
);

#[derive(Debug, Default)]
struct Calls {
    posted: Vec<u32>,
    withdrawn: Vec<u32>,
}

/// Answers on the main loop, like the D-Bus reply would.
struct FakeNotifier {
    calls: Rc<RefCell<Calls>>,
}

impl Notifier for FakeNotifier {
    fn post(&self, posted: Posted) {
        let calls = Rc::clone(&self.calls);
        glib::idle_add_local_once(move || {
            let id = {
                let mut calls = calls.borrow_mut();
                let id = 40 + u32::try_from(calls.posted.len()).unwrap();
                calls.posted.push(id);
                id
            };
            posted(Some(id));
        });
    }

    fn withdraw(&self, id: u32) {
        self.calls.borrow_mut().withdrawn.push(id);
    }
}

fn settings_from_the_compiled_schema(dir: &Path) -> gio::Settings {
    let schemas = dir.join("schemas");
    fs::create_dir_all(&schemas).unwrap();
    fs::copy(
        SCHEMA_XML,
        schemas.join("io.github.apex_linux.Apex.Desktop.gschema.xml"),
    )
    .unwrap();
    let status = Command::new("glib-compile-schemas")
        .arg("--strict")
        .arg(&schemas)
        .status()
        .expect("glib-compile-schemas (Fedora: glib2) must be installed");
    assert!(status.success());
    let source = gio::SettingsSchemaSource::from_directory(&schemas, None, false).unwrap();
    let schema = source.lookup(SCHEMA_ID, false).expect("the Apex schema");
    let backend = gio::memory_settings_backend_new();
    gio::Settings::new_full(&schema, Some(&backend), None)
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        if !context.iteration(false) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// One test, because the `GLib` main context and the file monitor belong to one thread.
#[test]
fn the_agent_on_a_temporary_session() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        config_home: dir.path().join("config"),
        state_home: dir.path().join("state"),
    };
    let settings = settings_from_the_compiled_schema(dir.path());
    settings.set_boolean(LAYER_KEY, false).unwrap();
    fs::create_dir_all(paths.gtk_dir()).unwrap();
    fs::write(paths.gtk_css(), "headerbar { min-height: 30px; }\n").unwrap();
    fs::create_dir_all(paths.state_dir()).unwrap();
    fs::write(paths.guard_marker(), "").unwrap();

    let calls = Rc::new(RefCell::new(Calls::default()));
    let notifier = FakeNotifier {
        calls: Rc::clone(&calls),
    };
    let agent = Agent::start(settings.clone(), paths.clone(), Box::new(notifier));

    assert!(read(&paths.apex_css()).contains(":root { --window-radius: 14px; }"));
    assert_eq!(
        read(&paths.gtk_css()),
        "@import url(\"apex.css\");\nheaderbar { min-height: 30px; }\n"
    );

    wait_for("the offer at start", || agent.offered() == Some(40));
    assert_eq!(calls.borrow().posted, [40]);

    settings.set_uint(RADIUS_KEY, 6).unwrap();
    wait_for("apex.css to follow the setting", || {
        read(&paths.apex_css()).contains("--window-radius: 6px; }")
    });
    assert_eq!(
        read(&paths.gtk_css()).matches("apex.css").count(),
        1,
        "the import is not added twice"
    );

    agent.action_invoked(40, ACTION_KEY);
    assert!(settings.boolean(LAYER_KEY));
    assert!(!paths.guard_marker().exists());
    agent.notification_closed(40);

    fs::write(paths.guard_marker(), "").unwrap();
    wait_for("an offer for a marker that appears later", || {
        agent.offered() == Some(41)
    });

    agent.notification_closed(41);
    assert_eq!(agent.offered(), None);
    assert!(paths.guard_marker().exists(), "dismissing keeps the marker");
    assert_eq!(calls.borrow().posted, [40, 41]);

    fs::remove_file(paths.guard_marker()).unwrap();
    fs::write(paths.guard_marker(), "").unwrap();
    wait_for("an offer for a new trip", || agent.offered() == Some(42));
    fs::remove_file(paths.guard_marker()).unwrap();
    wait_for("the offer withdrawn with its marker", || {
        calls.borrow().withdrawn == [42]
    });

    let written: Vec<_> = fs::read_dir(dir.path().join("config/gtk-4.0"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(written.len(), 2, "{written:?}");
}
