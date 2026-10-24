#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

mod common;

use std::cell::RefCell;
use std::fs;
use std::rc::Rc;

use apex_session_agent::agent::{Agent, Notifier, Posted};
use apex_session_agent::guard::ACTION_KEY;
use apex_session_agent::paths::Paths;
use apex_session_agent::{ACCENT_KEY, LAYER_KEY, RADIUS_KEY};
use common::{read, settings_from_the_compiled_schemas, wait_for};
use gio::prelude::*;

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

/// One test, because the `GLib` main context and the file monitor belong to one thread.
#[test]
fn the_agent_on_a_temporary_session() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(
        dir.path().join("config"),
        dir.path().join("data"),
        dir.path().join("state"),
    );
    let (settings, interface) = settings_from_the_compiled_schemas(dir.path());
    settings.set_boolean(LAYER_KEY, false).unwrap();
    fs::create_dir_all(paths.gtk_dir()).unwrap();
    fs::write(paths.gtk_css(), "headerbar { min-height: 30px; }\n").unwrap();
    fs::create_dir_all(paths.marker_dir()).unwrap();
    fs::write(paths.guard_marker(), "").unwrap();

    let calls = Rc::new(RefCell::new(Calls::default()));
    let notifier = FakeNotifier {
        calls: Rc::clone(&calls),
    };
    let agent = Agent::start(
        settings.clone(),
        Some(interface.clone()),
        paths.clone(),
        Box::new(notifier),
    );

    assert!(read(&paths.apex_css()).contains(":root { --window-radius: 14px; }"));
    assert_eq!(
        read(&paths.gtk_css()),
        "@import url(\"apex.css\");\nheaderbar { min-height: 30px; }\n"
    );

    assert!(read(&paths.ptyxis_palette()).contains("Cursor=#3584e4\n"));

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

    interface.set_string(ACCENT_KEY, "orange").unwrap();
    wait_for("apex.palette to follow the accent", || {
        read(&paths.ptyxis_palette()).contains("Cursor=#ed5b00\n")
    });
    assert!(!read(&paths.ptyxis_palette()).contains("#3584e4"));

    // As dconf losing the write: the value is false again by the time the agent reads it.
    let revert = settings.connect_changed(Some(LAYER_KEY), |settings, key| {
        if settings.boolean(key) {
            settings.set_boolean(key, false).unwrap();
        }
    });
    agent.action_invoked(40, ACTION_KEY);
    settings.disconnect(revert);
    assert!(!settings.boolean(LAYER_KEY));
    assert!(
        paths.guard_marker().exists(),
        "a lost write keeps the marker"
    );
    agent.notification_closed(40);

    agent.notification_server(false);
    agent.notification_server(true);
    wait_for("the offer posted again when the shell returns", || {
        agent.offered() == Some(41)
    });
    agent.action_invoked(41, ACTION_KEY);
    assert!(settings.boolean(LAYER_KEY));
    assert!(!paths.guard_marker().exists());
    agent.notification_closed(41);

    fs::write(paths.guard_marker(), "").unwrap();
    wait_for("an offer for a marker that appears later", || {
        agent.offered() == Some(42)
    });

    agent.notification_closed(42);
    assert_eq!(agent.offered(), None);
    assert!(paths.guard_marker().exists(), "dismissing keeps the marker");
    assert_eq!(calls.borrow().posted, [40, 41, 42]);

    fs::remove_file(paths.guard_marker()).unwrap();
    fs::write(paths.guard_marker(), "").unwrap();
    wait_for("an offer for a new trip", || agent.offered() == Some(43));
    fs::remove_file(paths.guard_marker()).unwrap();
    wait_for("the offer withdrawn with its marker", || {
        calls.borrow().withdrawn == [43]
    });

    let written: Vec<_> = fs::read_dir(dir.path().join("config/gtk-4.0"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(written.len(), 2, "{written:?}");
}
