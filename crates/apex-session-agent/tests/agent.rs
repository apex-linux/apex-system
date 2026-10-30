#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

mod common;

use std::cell::RefCell;
use std::fs;
use std::rc::Rc;

use apex_session_agent::agent::Agent;
use apex_session_agent::guard::ACTION_KEY;
use apex_session_agent::icons::ICON_THEME_KEY;
use apex_session_agent::paths::Paths;
use apex_session_agent::{ACCENT_KEY, LAYER_KEY, RADIUS_KEY};
use common::{Calls, FakeNotifier, read, settings_from_the_compiled_schemas, wait_for};
use gio::prelude::*;

/// The palette and the folder theme follow the accent; a theme the user picked stays.
fn accent_changes(paths: &Paths, interface: &gio::Settings) {
    interface.set_string(ACCENT_KEY, "orange").unwrap();
    wait_for("apex.palette to follow the accent", || {
        read(&paths.ptyxis_palette()).contains("Cursor=#ed5b00\n")
    });
    assert!(!read(&paths.ptyxis_palette()).contains("#3584e4"));
    wait_for("the folders to follow the accent", || {
        interface.string(ICON_THEME_KEY) == "Apex-orange"
    });
    interface.set_string(ICON_THEME_KEY, "Papirus").unwrap();
    interface.set_string(ACCENT_KEY, "green").unwrap();
    wait_for("apex.palette to follow green", || {
        read(&paths.ptyxis_palette()).contains("Cursor=#3a944a\n")
    });
    assert_eq!(
        interface.string(ICON_THEME_KEY),
        "Papirus",
        "a theme the user picked stays"
    );
    interface.set_string(ICON_THEME_KEY, "Apex").unwrap();
    wait_for("back on Apex, the folders follow green", || {
        interface.string(ICON_THEME_KEY) == "Apex-green"
    });
    interface.set_string(ICON_THEME_KEY, "Papirus").unwrap();
    interface.reset(ICON_THEME_KEY);
    wait_for(
        "reset to the vendor default, the folders follow green",
        || interface.string(ICON_THEME_KEY) == "Apex-green",
    );
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
    assert_eq!(interface.string(ICON_THEME_KEY), "Apex-blue");

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

    accent_changes(&paths, &interface);

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
