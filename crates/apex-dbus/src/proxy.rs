use crate::{ApexError, BootVerdict, Change, Channel, Deployment, State};

/// Replies can take hours (downloads, authentication dialogs): set no short call timeout.
/// Polkit-guarded methods allow interactive authorization, which `auth_admin` actions need.
// The macro takes only literals, so a unit test compares them with the crate's constants.
#[zbus::proxy(
    interface = "io.github.sekirokenjii.Apex.Update1",
    default_service = "io.github.sekirokenjii.Apex.Update1",
    default_path = "/io/github/sekirokenjii/Apex/Update1",
    gen_blocking = false
)]
pub trait Update1 {
    /// Liveness probe for the boot health check; no polkit check.
    #[zbus(name = "Ping")]
    fn ping(&self) -> Result<(), ApexError>;

    /// Returns at once within 60 s of the last success, joins a running check, and is
    /// [`ApexError::Busy`] while a download runs or waits.
    #[zbus(name = "Check", allow_interactive_auth)]
    fn check(&self) -> Result<(), ApexError>;

    /// Stages locked; `override_soft_gates` skips only the power and metered gates.
    #[zbus(name = "Download", allow_interactive_auth)]
    fn download(&self, override_soft_gates: bool) -> Result<(), ApexError>;

    /// [`ApexError::DigestMismatch`] unless the stage holds `expected_digest`. Arms the boot
    /// counter; the caller's session reboots.
    #[zbus(name = "Apply", allow_interactive_auth)]
    fn apply(&self, expected_digest: &str) -> Result<(), ApexError>;

    /// Pulls the channel tag again, so it needs the registry and may stage a newer image,
    /// locked. On failure the update stays applied and the boot counter armed.
    #[zbus(name = "DiscardStaged", allow_interactive_auth)]
    fn discard_staged(&self) -> Result<(), ApexError>;

    /// `mark_bad` denylists the booted digest. [`ApexError::PolicyDenied`] when the rollback
    /// deployment is denylisted.
    #[zbus(name = "Rollback", allow_interactive_auth)]
    fn rollback(&self, mark_bad: bool) -> Result<(), ApexError>;

    /// Sorted by `(name, arch)`; empty when nothing is staged.
    #[zbus(name = "GetChanges")]
    fn get_changes(&self) -> Result<Vec<Change>, ApexError>;

    /// Stages the channel's image locked; it takes effect with [`Self::apply`] and a restart.
    #[zbus(name = "SwitchChannel", allow_interactive_auth)]
    fn switch_channel(&self, channel: Channel) -> Result<(), ApexError>;

    /// Guarded by the `discard` polkit action.
    #[zbus(name = "AcknowledgeHealthFailure", allow_interactive_auth)]
    fn acknowledge_health_failure(&self) -> Result<(), ApexError>;

    /// The timer's run; uid 0 only.
    #[zbus(name = "Automatic")]
    fn automatic(&self) -> Result<(), ApexError>;

    /// uid 0 only. `digest` is the booted digest when green, the failed target on fallback.
    #[zbus(name = "ReportBoot")]
    fn report_boot(&self, verdict: BootVerdict, digest: &str) -> Result<(), ApexError>;

    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<State>;

    /// `[0, 1]` while downloading; `-1` when the installed bootc reports no progress.
    #[zbus(property, name = "Progress")]
    fn progress(&self) -> zbus::Result<f64>;

    #[zbus(property, name = "Booted")]
    fn booted(&self) -> zbus::Result<Deployment>;

    #[zbus(property, name = "Staged")]
    fn staged(&self) -> zbus::Result<Deployment>;

    #[zbus(property, name = "Rollback")]
    fn rollback_deployment(&self) -> zbus::Result<Deployment>;

    /// Found by the last check and not staged yet.
    #[zbus(property, name = "Available")]
    fn available(&self) -> zbus::Result<Deployment>;

    #[zbus(property, name = "Channel")]
    fn channel(&self) -> zbus::Result<Channel>;

    /// Unix seconds of the last successful check; 0 if never.
    #[zbus(property, name = "LastCheck")]
    fn last_check(&self) -> zbus::Result<u64>;

    /// Why the available update waits, fit for display; `""` otherwise.
    #[zbus(property, name = "DeferredReason")]
    fn deferred_reason(&self) -> zbus::Result<String>;

    /// Cleared by the next success.
    #[zbus(property, name = "LastError")]
    fn last_error(&self) -> zbus::Result<String>;

    /// No newer build on the channel for more than 7 days.
    #[zbus(property, name = "Stale")]
    fn stale(&self) -> zbus::Result<bool>;

    /// The digest of an armed boot that failed its health checks, or `""`.
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
