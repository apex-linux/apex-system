use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use apex_boot::checks::names;
use apex_boot::{Check, FnCheck, SESSION_MIN_ACTIVE};

use crate::ports::{Clock, SessionInfo, Sessions};

pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// With a last [`apex_boot::LOGIND_TIMEOUT`] still inside [`apex_boot::CHECK_TIMEOUT`]. The
/// check polls because logging in closes the greeter the wait found, and the new user
/// session needs [`SESSION_MIN_ACTIVE`] of its own.
pub(crate) const SESSION_CHECK_BUDGET: Duration = Duration::from_secs(24);

const DESKTOP_CLASSES: [&str; 2] = ["greeter", "user"];

/// A crashing gnome-shell is replaced by a new session, so a crash loop never gets old
/// enough; sessions without a seat (SSH) prove nothing about the desktop.
pub(crate) fn proves_desktop(session: &SessionInfo, min_active: Duration) -> bool {
    DESKTOP_CLASSES.contains(&session.class.as_str())
        && session.state == "active"
        && !session.seat.is_empty()
        && session.age.is_some_and(|age| age >= min_active)
}

pub(crate) fn desktop_session(
    sessions: &[SessionInfo],
    min_active: Duration,
) -> Option<&SessionInfo> {
    sessions.iter().find(|s| proves_desktop(s, min_active))
}

/// Fails safe: a session whose class could not be read may be somebody's. Closing sessions
/// are logged out already but linger with systemd's default `KillUserProcesses=no`.
pub(crate) fn user_logged_in(sessions: &[SessionInfo]) -> bool {
    sessions
        .iter()
        .any(|s| s.class.is_empty() || (s.class.starts_with("user") && s.state != "closing"))
}

pub(crate) fn describe(sessions: &[SessionInfo]) -> String {
    if sessions.is_empty() {
        return "no sessions".to_owned();
    }
    let mut out = String::new();
    for (i, s) in sessions.iter().enumerate() {
        if i > 0 {
            out.push_str("; ");
        }
        let seat = if s.seat.is_empty() {
            "no seat"
        } else {
            &s.seat
        };
        if s.class.is_empty() {
            let _ = write!(out, "{} (properties unreadable) on {seat}", s.id);
        } else {
            let _ = write!(out, "{} {} {} on {seat}", s.id, s.class, s.state);
        }
        if let Some(age) = s.age {
            let _ = write!(out, " for {} s", age.as_secs());
        }
    }
    out
}

pub(crate) fn poll_for_desktop(
    sessions: &dyn Sessions,
    clock: &dyn Clock,
    deadline: Instant,
) -> Result<SessionInfo, String> {
    loop {
        let listed = sessions.list();
        if let Ok(list) = &listed
            && let Some(found) = desktop_session(list, SESSION_MIN_ACTIVE)
        {
            return Ok(found.clone());
        }
        let now = clock.now();
        if now >= deadline {
            return Err(match listed {
                Ok(list) => format!(
                    "no greeter or user session on a seat has been active for {} s ({})",
                    SESSION_MIN_ACTIVE.as_secs(),
                    describe(&list)
                ),
                Err(err) => format!("cannot list logind sessions: {err}"),
            });
        }
        clock.sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

pub(crate) fn check(sessions: Arc<dyn Sessions>, clock: Arc<dyn Clock>) -> Box<dyn Check> {
    Box::new(FnCheck::new(names::SESSION, move || {
        let deadline = clock.now() + SESSION_CHECK_BUDGET;
        poll_for_desktop(&*sessions, &*clock, deadline).map(|found| {
            tracing::debug!(session = %found.id, class = %found.class, "session check passed");
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{FakeClock, FakeSessions, session};
    use proptest::prelude::*;

    #[test]
    fn desktop_needs_class_state_seat_and_age() {
        let ok = session("c1", "greeter", "active", "seat0", 20);
        assert!(proves_desktop(&ok, SESSION_MIN_ACTIVE));
        assert!(proves_desktop(
            &session("2", "user", "active", "seat0", 300),
            SESSION_MIN_ACTIVE
        ));
        for bad in [
            session("c1", "greeter", "active", "seat0", 19),
            session("c1", "greeter", "online", "seat0", 60),
            session("c1", "greeter", "closing", "seat0", 60),
            session("3", "user", "active", "", 60),
            session("4", "lock-screen", "active", "seat0", 60),
            session("5", "background", "active", "seat0", 60),
            session("6", "user-early", "active", "seat0", 60),
            SessionInfo {
                age: None,
                ..ok.clone()
            },
        ] {
            assert!(!proves_desktop(&bad, SESSION_MIN_ACTIVE), "{bad:?}");
        }
    }

    #[test]
    fn logged_in_means_any_user_class_not_closing() {
        assert!(!user_logged_in(&[]));
        assert!(!user_logged_in(&[session(
            "c1", "greeter", "active", "seat0", 60
        )]));
        assert!(!user_logged_in(&[session(
            "2", "user", "closing", "seat0", 60
        )]));
        assert!(!user_logged_in(&[session(
            "7", "manager", "active", "", 60
        )]));
        for s in [
            session("2", "user", "active", "seat0", 0),
            session("2", "user", "online", "seat0", 60),
            session("3", "user", "active", "", 60),
            session("4", "user-early", "active", "", 60),
            session("5", "user-incomplete", "online", "seat0", 60),
            SessionInfo {
                age: None,
                ..session("6", "", "", "seat0", 0)
            },
        ] {
            assert!(user_logged_in(std::slice::from_ref(&s)), "{s:?}");
        }
    }

    #[test]
    fn describe_is_one_line() {
        let line = describe(&[
            session("c1", "greeter", "active", "seat0", 12),
            session("3", "user", "online", "", 70),
        ]);
        assert_eq!(
            line,
            "c1 greeter active on seat0 for 12 s; 3 user online on no seat for 70 s"
        );
        assert_eq!(describe(&[]), "no sessions");
        let unreadable = SessionInfo {
            age: None,
            ..session("7", "", "", "", 0)
        };
        assert_eq!(
            describe(&[unreadable]),
            "7 (properties unreadable) on no seat"
        );
    }

    #[test]
    fn polling_waits_for_the_session_to_be_old_enough() {
        let clock = Arc::new(FakeClock::new());
        let sessions = FakeSessions::new(Arc::clone(&clock), |t| {
            Ok(if t >= Duration::from_secs(30) {
                vec![session(
                    "c1",
                    "greeter",
                    "active",
                    "seat0",
                    t.as_secs() - 30,
                )]
            } else {
                vec![]
            })
        });
        let deadline = clock.now() + Duration::from_secs(120);
        let found = poll_for_desktop(&sessions, &*clock, deadline).unwrap();
        assert_eq!(found.id, "c1");
        assert_eq!(clock.elapsed(), Duration::from_secs(50));
    }

    #[test]
    fn polling_gives_up_at_the_deadline_with_a_reason() {
        let clock = Arc::new(FakeClock::new());
        let sessions = FakeSessions::new(Arc::clone(&clock), |_| {
            Ok(vec![session("c1", "greeter", "active", "seat0", 5)])
        });
        let deadline = clock.now() + Duration::from_millis(2500);
        let err = poll_for_desktop(&sessions, &*clock, deadline).unwrap_err();
        assert_eq!(
            err,
            "no greeter or user session on a seat has been active for 20 s \
             (c1 greeter active on seat0 for 5 s)"
        );
        assert_eq!(clock.elapsed(), Duration::from_millis(2500));
        assert_eq!(sessions.calls(), 4);

        let failing = FakeSessions::new(Arc::clone(&clock), |_| Err("no bus".to_owned()));
        let err = poll_for_desktop(&failing, &*clock, clock.now()).unwrap_err();
        assert_eq!(err, "cannot list logind sessions: no bus");
    }

    #[test]
    fn logind_errors_are_retried() {
        let clock = Arc::new(FakeClock::new());
        let sessions = FakeSessions::new(Arc::clone(&clock), |t| {
            if t < Duration::from_secs(3) {
                Err("logind restarting".to_owned())
            } else {
                Ok(vec![session("c1", "greeter", "active", "seat0", 40)])
            }
        });
        let found = poll_for_desktop(&sessions, &*clock, clock.now() + SESSION_CHECK_BUDGET);
        assert!(found.is_ok(), "{found:?}");
    }

    #[test]
    fn the_check_survives_the_handover_from_greeter_to_user() {
        let clock = Arc::new(FakeClock::new());
        let sessions = Arc::new(FakeSessions::new(Arc::clone(&clock), |t| {
            Ok(vec![session("3", "user", "active", "seat0", t.as_secs())])
        }));
        let result = check(sessions, clock.clone()).run();
        assert_eq!(result, Ok(()));
        assert_eq!(clock.elapsed(), SESSION_MIN_ACTIVE);
    }

    fn arb_session() -> impl Strategy<Value = SessionInfo> {
        (
            "[a-z0-9]{1,3}",
            prop::sample::select(vec![
                "greeter",
                "user",
                "user-early",
                "user-light",
                "lock-screen",
                "background",
                "manager",
            ]),
            prop::sample::select(vec!["online", "active", "closing"]),
            prop::sample::select(vec!["", "seat0", "seat1"]),
            proptest::option::of(0u64..100),
        )
            .prop_map(|(id, class, state, seat, age)| SessionInfo {
                id,
                class: class.to_owned(),
                state: state.to_owned(),
                seat: seat.to_owned(),
                age: age.map(Duration::from_secs),
            })
    }

    proptest! {
        #[test]
        fn a_found_session_always_qualifies(sessions in proptest::collection::vec(arb_session(), 0..6)) {
            match desktop_session(&sessions, SESSION_MIN_ACTIVE) {
                Some(found) => {
                    prop_assert!(DESKTOP_CLASSES.contains(&found.class.as_str()));
                    prop_assert_eq!(found.state.as_str(), "active");
                    prop_assert!(!found.seat.is_empty());
                    prop_assert!(found.age.unwrap() >= SESSION_MIN_ACTIVE);
                }
                None => prop_assert!(sessions.iter().all(|s| !proves_desktop(s, SESSION_MIN_ACTIVE))),
            }
        }

        #[test]
        fn a_user_desktop_session_is_always_a_login(sessions in proptest::collection::vec(arb_session(), 0..6)) {
            if sessions.iter().any(|s| s.class == "user" && proves_desktop(s, SESSION_MIN_ACTIVE)) {
                prop_assert!(user_logged_in(&sessions));
            }
            if sessions.iter().all(|s| !s.class.starts_with("user")) {
                prop_assert!(!user_logged_in(&sessions));
            }
        }
    }
}
