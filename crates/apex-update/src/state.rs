//! The derived update state (the D-Bus `State` property) and the staleness rule.

use std::fmt;
use std::str::FromStr;

use crate::gates::{Trigger, gate_version};
use crate::snapshot::SystemSnapshot;

/// The update state, as published in the `State` property of
/// `io.github.sekirokenjii.Apex.Update1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// Nothing newer is known.
    UpToDate,
    /// A newer image was found and is not staged.
    Available,
    /// A download is running. Set by the daemon, never derived.
    Downloading,
    /// An update is downloaded and locked; it will not boot until applied.
    Staged,
    /// The user applied the staged update; it boots at the next restart.
    ReadyForReboot,
    /// A rollback is queued for the next boot.
    RollbackQueued,
    /// Local rpm-ostree changes stop bootc from upgrading.
    Incompatible,
    /// A transient writable overlay is mounted on `/usr`.
    UsrOverlay,
    /// The last staged update could not be finalized at shutdown.
    FinalizeFailed,
    /// A newly applied deployment failed its health checks during a user session.
    HealthFailure,
    /// The last operation failed. Set by the daemon, never derived.
    Error,
    /// Not known yet, for example before the first status read. Set by the daemon.
    Unknown,
}

impl State {
    /// Every state, in declaration order.
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

    /// The value of the D-Bus `State` property.
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

/// A string that is not one of the [`State`] values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown update state {0:?}")]
pub struct UnknownState(pub String);

impl FromStr for State {
    type Err = UnknownState;

    /// Parses a `State` property value, for clients.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| UnknownState(s.to_owned()))
    }
}

/// Derives the state from a snapshot.
///
/// The first matching rule wins:
///
/// 1. `incompatible` — bootc cannot upgrade at all;
/// 2. `usr-overlay` — the running system was modified;
/// 3. `rollback-queued` — the next boot goes back a version;
/// 4. `ready-for-reboot` — a staged deployment is unlocked;
/// 5. `staged` — a staged deployment is locked (download-only);
/// 6. `finalize-failed` — the last update did not install;
/// 7. `health-failure` — the running deployment failed its checks: the unacknowledged
///    failure record names the booted digest ([`health_failure_is_current`]);
/// 8. `available` — the remote digest is neither booted nor staged, is not
///    denylisted, and is not known to be older or equal (an unknown version counts as
///    available, as for a user download; see [`crate::gates::gate_version`]);
/// 9. `up-to-date` — otherwise, including when no check has run yet.
///
/// The bootc flags come first: they block every operation, so a blocked system never
/// looks like it is merely waiting. A stage comes before the two failure states because
/// it is always newer than what they describe. `finalize-failed` is about the stage
/// that failed at the last shutdown, which is gone, and it stays true for the whole boot
/// (`ostree-boot-complete.service` consumes the stamp and stays failed).
/// `health-failure` is about the booted deployment, which the stage would replace.
/// Ranking them first would keep showing an old failure instead of "restart to finish"
/// after the user applied a newer update in the same boot. The failure itself is still
/// published in the `HealthFailure` property. `downloading`, `error` and `unknown`
/// describe the daemon's own activity and are never derived.
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

/// Whether the unacknowledged health failure is about the running deployment (rule 7
/// of [`derive_state`]).
///
/// A record for another digest describes a deployment that is no longer booted (after a
/// rollback, or once a newer update booted): it stays in `health-failure.json` until it
/// is acknowledged, but it no longer determines the state.
pub fn health_failure_is_current(snapshot: &SystemSnapshot) -> bool {
    snapshot
        .health_failure
        .as_deref()
        .is_some_and(|digest| snapshot.host.booted_digest() == Some(digest))
}

/// Whether the last check found an image worth offering (rule 8 of [`derive_state`]).
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

/// The channel counts as stale after this many seconds without a newer build (7 days).
pub const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

/// Whether the channel has not published a newer build for more than 7 days.
///
/// `latest_remote_build_time` is the build time of the newest image known on the channel
/// (the remote image, or the booted one when nothing newer exists), in Unix seconds.
/// Exactly 7 days is not yet stale. A build time in the future (clock skew) is not stale.
pub fn is_stale(latest_remote_build_time: u64, now: u64) -> bool {
    now.saturating_sub(latest_remote_build_time) > STALE_AFTER_SECS
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

    /// Every condition at once; removing them from the top must reveal each state in
    /// the documented precedence order.
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

    /// V2 failed to finalize at shutdown; in the next boot the automatic run staged V3 and
    /// the user applied it. The state must say "restart to finish", not the old failure.
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

    /// A red armed boot the user has not acknowledged, then a newer update applied in
    /// the same boot.
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

        // Rolled back (or a newer update booted): the record is history.
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

    #[test]
    fn staleness_boundary() {
        let built = 1_790_000_000;
        assert!(!is_stale(built, built));
        assert!(!is_stale(built, built + STALE_AFTER_SECS));
        assert!(is_stale(built, built + STALE_AFTER_SECS + 1));
        assert!(
            !is_stale(built, built - 5),
            "future build time is not stale"
        );
        assert!(is_stale(0, u64::MAX));
    }
}
