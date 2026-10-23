use std::fmt;
use std::str::FromStr;

use crate::gates::{Trigger, gate_version};
use crate::snapshot::SystemSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    UpToDate,
    Available,
    Downloading,
    Staged,
    ReadyForReboot,
    RollbackQueued,
    Incompatible,
    UsrOverlay,
    FinalizeFailed,
    HealthFailure,
    Error,
    Unknown,
}

impl State {
    pub const ALL: [State; 12] = [
        State::UpToDate,
        State::Available,
        State::Downloading,
        State::Staged,
        State::ReadyForReboot,
        State::RollbackQueued,
        State::Incompatible,
        State::UsrOverlay,
        State::FinalizeFailed,
        State::HealthFailure,
        State::Error,
        State::Unknown,
    ];

    /// The D-Bus `State` property value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpToDate => "up-to-date",
            Self::Available => "available",
            Self::Downloading => "downloading",
            Self::Staged => "staged",
            Self::ReadyForReboot => "ready-for-reboot",
            Self::RollbackQueued => "rollback-queued",
            Self::Incompatible => "incompatible",
            Self::UsrOverlay => "usr-overlay",
            Self::FinalizeFailed => "finalize-failed",
            Self::HealthFailure => "health-failure",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown update state {0:?}")]
pub struct UnknownState(pub String);

impl FromStr for State {
    type Err = UnknownState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownState(s.to_owned()))
    }
}

/// A stage outranks the failure states, which describe something older that it replaces.
/// `Downloading`, `Error` and `Unknown` are the daemon's own and never derived.
pub fn derive_state(snapshot: &SystemSnapshot) -> State {
    let host = &snapshot.host;
    if host.incompatible {
        State::Incompatible
    } else if host.usr_overlay {
        State::UsrOverlay
    } else if host.rollback_queued {
        State::RollbackQueued
    } else if let Some(staged) = &host.staged {
        if staged.download_only {
            State::Staged
        } else {
            State::ReadyForReboot
        }
    } else if host.finalize_failed {
        State::FinalizeFailed
    } else if health_failure_is_current(snapshot) {
        State::HealthFailure
    } else if is_available(snapshot) {
        State::Available
    } else {
        State::UpToDate
    }
}

/// A record for another digest stays until acknowledged, but no longer sets the state.
pub fn health_failure_is_current(snapshot: &SystemSnapshot) -> bool {
    snapshot
        .health_failure
        .as_deref()
        .is_some_and(|digest| snapshot.host.booted_digest() == Some(digest))
}

pub fn is_available(snapshot: &SystemSnapshot) -> bool {
    let Some(remote) = &snapshot.remote else {
        return false;
    };
    let host = &snapshot.host;
    let digest = Some(remote.digest.as_str());
    digest != host.booted_digest()
        && digest != host.staged_digest()
        && !snapshot.denylist.contains(&remote.digest)
        && gate_version(
            host.booted_version(),
            remote.version.as_ref(),
            Trigger::User,
        )
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{BOOTED, REMOTE, STAGED, deny, deployment, healthy, ver};

    #[test]
    fn as_str_round_trips_and_matches_the_dbus_contract() {
        let names: Vec<_> = State::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            names,
            [
                "up-to-date",
                "available",
                "downloading",
                "staged",
                "ready-for-reboot",
                "rollback-queued",
                "incompatible",
                "usr-overlay",
                "finalize-failed",
                "health-failure",
                "error",
                "unknown",
            ]
        );
        for state in State::ALL {
            assert_eq!(state.as_str().parse::<State>(), Ok(state));
            assert_eq!(state.to_string(), state.as_str());
        }
        assert_eq!(format!("[{:<8}]", State::Staged), "[staged  ]");
        assert_eq!(
            "Up-To-Date".parse::<State>(),
            Err(UnknownState("Up-To-Date".into()))
        );
    }

    #[test]
    fn precedence_order() {
        let mut s = healthy();
        s.host.incompatible = true;
        s.host.usr_overlay = true;
        s.host.rollback_queued = true;
        s.host.finalize_failed = true;
        s.health_failure = Some(BOOTED.into());
        s.host.staged = Some(deployment(STAGED, "45.20260922.1", false));

        let mut seen = vec![derive_state(&s)];
        s.host.incompatible = false;
        seen.push(derive_state(&s));
        s.host.usr_overlay = false;
        seen.push(derive_state(&s));
        s.host.rollback_queued = false;
        seen.push(derive_state(&s));
        s.host.staged.as_mut().expect("staged").download_only = true;
        seen.push(derive_state(&s));
        s.host.staged = None;
        seen.push(derive_state(&s));
        s.host.finalize_failed = false;
        seen.push(derive_state(&s));
        s.health_failure = None;
        seen.push(derive_state(&s));
        s.remote = None;
        seen.push(derive_state(&s));

        assert_eq!(
            seen,
            [
                State::Incompatible,
                State::UsrOverlay,
                State::RollbackQueued,
                State::ReadyForReboot,
                State::Staged,
                State::FinalizeFailed,
                State::HealthFailure,
                State::Available,
                State::UpToDate,
            ]
        );
    }

    #[test]
    fn a_newer_stage_hides_a_finalize_failure_of_this_boot() {
        let mut s = healthy();
        s.host.finalize_failed = true;
        assert_eq!(derive_state(&s), State::FinalizeFailed);
        s.host.staged = Some(deployment(REMOTE, "45.20260924.1", true));
        assert_eq!(derive_state(&s), State::Staged);
        s.host.staged.as_mut().expect("staged").download_only = false;
        assert_eq!(derive_state(&s), State::ReadyForReboot);
    }

    #[test]
    fn a_newer_stage_hides_a_health_failure() {
        let mut s = healthy();
        s.health_failure = Some(BOOTED.into());
        assert_eq!(derive_state(&s), State::HealthFailure);
        s.host.staged = Some(deployment(REMOTE, "45.20260924.1", false));
        assert_eq!(derive_state(&s), State::ReadyForReboot);
    }

    #[test]
    fn a_health_failure_counts_only_while_its_digest_is_booted() {
        let mut s = healthy();
        s.health_failure = Some(BOOTED.into());
        assert!(health_failure_is_current(&s));

        s.health_failure = Some("sha256:failed".into());
        assert!(!health_failure_is_current(&s));
        assert_eq!(derive_state(&s), State::Available);

        s.host.booted = None;
        s.health_failure = Some(BOOTED.into());
        assert!(!health_failure_is_current(&s), "nothing booted to match");
        assert!(!health_failure_is_current(&healthy()), "no record");
    }

    #[test]
    fn never_derives_daemon_states() {
        let derived = [
            derive_state(&healthy()),
            derive_state(&SystemSnapshot::default()),
        ];
        for state in derived {
            assert!(!matches!(
                state,
                State::Downloading | State::Error | State::Unknown
            ));
        }
        assert_eq!(derive_state(&SystemSnapshot::default()), State::UpToDate);
    }

    #[test]
    fn available_needs_a_new_allowed_newer_digest() {
        assert!(is_available(&healthy()));

        let mut s = healthy();
        s.remote.as_mut().expect("remote").digest = BOOTED.into();
        assert_eq!(derive_state(&s), State::UpToDate, "remote is booted");

        let mut s = healthy();
        s.host.staged = Some(deployment(REMOTE, "45.20260924.1", true));
        assert!(!is_available(&s), "remote is staged");
        assert_eq!(derive_state(&s), State::Staged);

        let mut s = healthy();
        s.denylist = [deny(REMOTE)].into_iter().collect();
        assert_eq!(derive_state(&s), State::UpToDate, "denylisted");

        let mut s = healthy();
        s.remote.as_mut().expect("remote").version = Some(ver("45.20260920.1"));
        assert_eq!(derive_state(&s), State::UpToDate, "equal version");

        let mut s = healthy();
        s.remote.as_mut().expect("remote").version = Some(ver("45.20260901.1"));
        assert_eq!(derive_state(&s), State::UpToDate, "older version");

        let mut s = healthy();
        s.remote.as_mut().expect("remote").version = None;
        assert_eq!(
            derive_state(&s),
            State::Available,
            "unknown version is offered"
        );

        let mut s = healthy();
        s.host.booted = None;
        assert_eq!(
            derive_state(&s),
            State::Available,
            "nothing booted to compare"
        );
    }
}
