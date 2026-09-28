//! The client proxy for `io.github.sekirokenjii.Apex.Update1`.
//!
//! The attribute literals below must equal [`crate::BUS_NAME`], [`crate::OBJECT_PATH`] and
//! [`crate::INTERFACE`] (the macro only takes literals); a unit test checks that, and
//! `tests/proxy_wire.rs` checks every member against the contract XML on a real connection.

use crate::{ApexError, BootVerdict, Change, Channel, Deployment, State};

/// Client side of the Update1 interface; the macro generates [`Update1Proxy`] from it.
///
/// Methods that polkit guards carry the `ALLOW_INTERACTIVE_AUTHORIZATION` message flag, so the
/// daemon may let polkit ask the session's authentication agent (the `auth_admin` actions
/// need it). The contract does not bound how long a method takes to reply (bootc's download
/// timeout is two hours, and an authentication dialog waits for the user), so do not put a
/// short method-call timeout on the connection.
#[zbus::proxy(
    interface = "io.github.sekirokenjii.Apex.Update1",
    default_service = "io.github.sekirokenjii.Apex.Update1",
    default_path = "/io/github/sekirokenjii/Apex/Update1",
    gen_blocking = false
)]
pub trait Update1 {
    /// Liveness probe used by the boot health check. No polkit check.
    #[zbus(name = "Ping")]
    fn ping(&self) -> Result<(), ApexError>;

    /// Queries the registry for a newer image on the current channel. Calls within 60 s of
    /// the last successful check return immediately; a call while another check runs or
    /// waits gets that check's result, and one while a download runs or waits fails with
    /// [`ApexError::Busy`]. Polkit: `check`.
    #[zbus(name = "Check", allow_interactive_auth)]
    fn check(&self) -> Result<(), ApexError>;

    /// Downloads and stages the available update, locked (download-only). The gates are
    /// evaluated and [`ApexError::PolicyDenied`] carries the reason. `override_soft_gates`
    /// overrides the power and metered gates, never the space, denylist or bootc-flag gates.
    /// Polkit: `download`.
    #[zbus(name = "Download", allow_interactive_auth)]
    fn download(&self, override_soft_gates: bool) -> Result<(), ApexError>;

    /// Unlocks the staged deployment so the next boot uses it, and arms the boot counter.
    /// Fails with [`ApexError::DigestMismatch`] unless the staged digest equals
    /// `expected_digest`. The daemon never reboots; the caller's session requests the reboot.
    /// A stage made by [`Self::switch_channel`] is installed even when it holds the running
    /// build (then nothing is armed) or an older one. Polkit: `apply`.
    #[zbus(name = "Apply", allow_interactive_auth)]
    fn apply(&self, expected_digest: &str) -> Result<(), ApexError>;

    /// Re-locks a staged update that was applied but not yet booted, and disarms the boot
    /// counter. Never calls `bootc rollback`. Polkit: `discard`.
    ///
    /// bootc cannot re-lock a stage in place: the daemon runs `bootc upgrade` in
    /// download-only mode, which pulls the channel's tag again. So the call needs the
    /// registry ([`ApexError::Network`] offline), and when the tag has moved on it downloads
    /// the newer image and stages *that* one, still locked, in place of the applied one: after
    /// success `Staged` may name another digest than before, and nothing boots next either
    /// way. When the call succeeds the counter is disarmed and the intent cleared. When it
    /// fails and the applied update would still boot next (offline, or the tag points back
    /// at the running image, which leaves bootc nothing to lock), both stay, so the restart
    /// that installs it is still protected; `State` stays `ready-for-reboot`.
    #[zbus(name = "DiscardStaged", allow_interactive_auth)]
    fn discard_staged(&self) -> Result<(), ApexError>;

    /// Queues the rollback deployment for the next boot. With `mark_bad`, the booted digest
    /// is added to the denylist. Fails with [`ApexError::PolicyDenied`] when the rollback
    /// deployment is on the denylist (after a fallback it is the failed update). Polkit:
    /// `rollback` (`auth_admin`).
    #[zbus(name = "Rollback", allow_interactive_auth)]
    fn rollback(&self, mark_bad: bool) -> Result<(), ApexError>;

    /// Package changes between the booted and the staged image: `(name, arch, kind,
    /// from_evr, to_evr)` entries (see [`Change`]), sorted by `(name, arch)`; empty when
    /// nothing is staged. Fails while the state of the system is not known.
    #[zbus(name = "GetChanges")]
    fn get_changes(&self) -> Result<Vec<Change>, ApexError>;

    /// Follows another channel of the same Fedora major: downloads its image and stages it
    /// locked; the switch takes effect with [`Self::apply`] and a restart. Polkit:
    /// `switch-channel` (`auth_admin`).
    #[zbus(name = "SwitchChannel", allow_interactive_auth)]
    fn switch_channel(&self, channel: Channel) -> Result<(), ApexError>;

    /// Clears the `HealthFailure` property after the user has seen it. Polkit: `discard`
    /// (contract §8).
    #[zbus(name = "AcknowledgeHealthFailure", allow_interactive_auth)]
    fn acknowledge_health_failure(&self) -> Result<(), ApexError>;

    /// Automatic run for the systemd timer: check, then download if every gate passes and no
    /// user intent exists. Accepted from uid 0 only.
    #[zbus(name = "Automatic")]
    fn automatic(&self) -> Result<(), ApexError>;

    /// Reports the assessment of the current boot (`apex-boot-assess`, uid 0 only). `digest`
    /// is the booted digest for [`BootVerdict::Green`] and the failed target for
    /// [`BootVerdict::Fallback`].
    #[zbus(name = "ReportBoot")]
    fn report_boot(&self, verdict: BootVerdict, digest: &str) -> Result<(), ApexError>;

    /// The derived state.
    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<State>;

    /// Download progress in `[0, 1]` while the state is [`State::Downloading`]; `-1` when
    /// unknown (the installed bootc has no progress support).
    #[zbus(property, name = "Progress")]
    fn progress(&self) -> zbus::Result<f64>;

    /// The booted deployment.
    #[zbus(property, name = "Booted")]
    fn booted(&self) -> zbus::Result<Deployment>;

    /// The staged deployment; empty when nothing is staged.
    #[zbus(property, name = "Staged")]
    fn staged(&self) -> zbus::Result<Deployment>;

    /// The rollback deployment; empty when there is none.
    #[zbus(property, name = "Rollback")]
    fn rollback_deployment(&self) -> zbus::Result<Deployment>;

    /// The newer image found by the last check and not yet staged; empty otherwise.
    #[zbus(property, name = "Available")]
    fn available(&self) -> zbus::Result<Deployment>;

    /// The channel the booted deployment follows.
    #[zbus(property, name = "Channel")]
    fn channel(&self) -> zbus::Result<Channel>;

    /// Unix time (seconds) of the last successful check; 0 if never.
    #[zbus(property, name = "LastCheck")]
    fn last_check(&self) -> zbus::Result<u64>;

    /// Human-readable reason the last automatic download was deferred, or `""`.
    #[zbus(property, name = "DeferredReason")]
    fn deferred_reason(&self) -> zbus::Result<String>;

    /// Message of the last failed operation, or `""`; cleared by the next success.
    #[zbus(property, name = "LastError")]
    fn last_error(&self) -> zbus::Result<String>;

    /// `true` while the channel has not published a newer build for more than 7 days.
    #[zbus(property, name = "Stale")]
    fn stale(&self) -> zbus::Result<bool>;

    /// The digest of a newly applied deployment that failed its required health checks while
    /// a user session was active, or `""`.
    #[zbus(property, name = "HealthFailure")]
    fn health_failure(&self) -> zbus::Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BUS_NAME, INTERFACE, OBJECT_PATH};
    use zbus::{
        names::{BusName, InterfaceName},
        proxy::Defaults,
        zvariant::ObjectPath,
    };

    #[test]
    fn proxy_defaults_are_the_contract_names() {
        let interface = Update1Proxy::INTERFACE.as_ref().map(InterfaceName::as_str);
        let destination = Update1Proxy::DESTINATION.as_ref().map(BusName::as_str);
        let path = Update1Proxy::PATH.as_ref().map(ObjectPath::as_str);
        assert_eq!(interface, Some(INTERFACE));
        assert_eq!(destination, Some(BUS_NAME));
        assert_eq!(path, Some(OBJECT_PATH));
    }
}
