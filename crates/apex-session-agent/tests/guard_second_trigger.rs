#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

//! DESK-1 across logins: the crash guard trips again on the same deployment after the offer
//! turned the Apex desktop back on, and keeps a trip counter beside the marker.

mod common;

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use apex_session_agent::LAYER_KEY;
use apex_session_agent::agent::Agent;
use apex_session_agent::guard::ACTION_KEY;
use apex_session_agent::paths::Paths;
use common::{Calls, FakeNotifier, settings_from_the_compiled_schemas, wait_for};
use gio::prelude::*;

/// Assumed until DESK-1's script names it: one file, the deployment and its trip count.
const COUNTER: &str = "shell-guard-trips";
const DEPLOYMENT: &str = "4f1c2a9e0d7b3c6a5e8f1b2d4c6a8e0f2b4d6c8a0e2f4b6d8c0a2e4f6b8d0c2a.0";

struct Session {
    settings: gio::Settings,
    interface: gio::Settings,
    paths: Paths,
    calls: Rc<RefCell<Calls>>,
}

impl Session {
    fn counter(&self) -> PathBuf {
        self.paths.marker_dir().join(COUNTER)
    }

    /// What the guard does as the session fails: the layer off, the marker, the count.
    fn trip(&self, count: u32) {
        self.settings.set_boolean(LAYER_KEY, false).unwrap();
        gio::Settings::sync();
        fs::create_dir_all(self.paths.marker_dir()).unwrap();
        fs::write(self.paths.guard_marker(), "").unwrap();
        fs::write(self.counter(), format!("{DEPLOYMENT} {count}\n")).unwrap();
    }

    fn login(&self) -> Rc<Agent> {
        Agent::start(
            self.settings.clone(),
            Some(self.interface.clone()),
            self.paths.clone(),
            Box::new(FakeNotifier {
                calls: Rc::clone(&self.calls),
            }),
        )
    }

    fn turn_on_with_the_offer(&self, agent: &Rc<Agent>, id: u32) {
        agent.action_invoked(id, ACTION_KEY);
        assert!(self.settings.boolean(LAYER_KEY));
        assert!(!self.paths.guard_marker().exists());
        agent.notification_closed(id);
    }

    fn counter_writes_alone_post_and_withdraw_nothing(&self, agent: &Rc<Agent>, count: u32) {
        let before = (
            self.calls.borrow().posted.clone(),
            self.calls.borrow().withdrawn.clone(),
            agent.offered(),
        );
        fs::write(self.counter(), format!("{DEPLOYMENT} {count}\n")).unwrap();
        fs::remove_file(self.counter()).unwrap();
        fs::write(self.counter(), format!("{DEPLOYMENT} {count}\n")).unwrap();
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if !context.iteration(false) {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let after = (
            self.calls.borrow().posted.clone(),
            self.calls.borrow().withdrawn.clone(),
            agent.offered(),
        );
        assert_eq!(after, before);
    }
}

/// One test, because the `GLib` main context and the file monitor belong to one thread.
#[test]
fn a_second_trip_on_the_same_deployment_offers_the_desktop_again() {
    let dir = tempfile::tempdir().unwrap();
    let (settings, interface) = settings_from_the_compiled_schemas(dir.path());
    let session = Session {
        settings,
        interface,
        paths: Paths::new(
            dir.path().join("config"),
            dir.path().join("data"),
            dir.path().join("state"),
        ),
        calls: Rc::new(RefCell::new(Calls::default())),
    };

    session.trip(1);
    let agent = session.login();
    wait_for("the offer after the first trip", || {
        agent.offered() == Some(40)
    });
    let revert = session
        .settings
        .connect_changed(Some(LAYER_KEY), |settings, key| {
            if settings.boolean(key) {
                settings.set_boolean(key, false).unwrap();
            }
        });
    agent.action_invoked(40, ACTION_KEY);
    session.settings.disconnect(revert);
    assert!(!session.settings.boolean(LAYER_KEY));
    assert!(
        session.paths.guard_marker().exists(),
        "a lost write keeps the marker"
    );
    agent.notification_closed(40);
    drop(agent);

    let agent = session.login();
    wait_for("the offer at the next login", || {
        agent.offered() == Some(41)
    });
    session.counter_writes_alone_post_and_withdraw_nothing(&agent, 1);
    session.turn_on_with_the_offer(&agent, 41);
    session.counter_writes_alone_post_and_withdraw_nothing(&agent, 1);
    drop(agent);

    session.trip(2);
    assert_eq!(
        fs::read_to_string(session.counter()).unwrap(),
        format!("{DEPLOYMENT} 2\n")
    );
    let agent = session.login();
    wait_for("the offer after the second trip", || {
        agent.offered() == Some(42)
    });
    session.turn_on_with_the_offer(&agent, 42);
    assert_eq!(
        fs::read_to_string(session.counter()).unwrap(),
        format!("{DEPLOYMENT} 2\n"),
        "the agent leaves the counter to the guard"
    );
    assert_eq!(session.calls.borrow().posted, [40, 41, 42]);
    assert!(session.calls.borrow().withdrawn.is_empty());
}
