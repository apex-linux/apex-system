use crate::{ApexError, BootVerdict, Change, Channel, Deployment, State};

/// Replies can take hours: set no short call timeout. Contracts: [`crate::INTROSPECTION_XML`].
/// Polkit-guarded methods allow interactive authorization, which `auth_admin` actions need.
#[zbus::proxy(
    // Literals only, so a unit test compares them with the crate's constants.
    interface = "io.github.apex_linux.Apex.Update1",
    default_service = "io.github.apex_linux.Apex.Update1",
    default_path = "/io/github/apex_linux/Apex/Update1",
)]
pub trait Update1 {
    #[zbus(name = "Ping")]
    fn ping(&self) -> Result<(), ApexError>;

    #[zbus(name = "Check", allow_interactive_auth)]
    fn check(&self) -> Result<(), ApexError>;

    #[zbus(name = "Download", allow_interactive_auth)]
    fn download(&self, override_soft_gates: bool) -> Result<(), ApexError>;

    #[zbus(name = "Apply", allow_interactive_auth)]
    fn apply(&self, expected_digest: &str) -> Result<(), ApexError>;

    #[zbus(name = "Rollback", allow_interactive_auth)]
    fn rollback(&self, mark_bad: bool) -> Result<(), ApexError>;

    #[zbus(name = "GetChanges")]
    fn get_changes(&self) -> Result<Vec<Change>, ApexError>;

    #[zbus(name = "AcknowledgeHealthFailure", allow_interactive_auth)]
    fn acknowledge_health_failure(&self) -> Result<(), ApexError>;

    #[zbus(name = "Automatic")]
    fn automatic(&self) -> Result<(), ApexError>;

    #[zbus(name = "ReportBoot")]
    fn report_boot(&self, verdict: BootVerdict, digest: &str) -> Result<(), ApexError>;

    #[zbus(name = "SetAutomaticDownloads", allow_interactive_auth)]
    fn set_automatic_downloads(&self, enabled: bool) -> Result<(), ApexError>;

    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<State>;

    #[zbus(property, name = "Progress")]
    fn progress(&self) -> zbus::Result<f64>;

    #[zbus(property, name = "Booted")]
    fn booted(&self) -> zbus::Result<Deployment>;

    #[zbus(property, name = "Staged")]
    fn staged(&self) -> zbus::Result<Deployment>;

    #[zbus(property, name = "Rollback")]
    fn rollback_deployment(&self) -> zbus::Result<Deployment>;

    #[zbus(property, name = "Available")]
    fn available(&self) -> zbus::Result<Deployment>;

    #[zbus(property, name = "Channel")]
    fn channel(&self) -> zbus::Result<Channel>;

    #[zbus(property, name = "LastCheck")]
    fn last_check(&self) -> zbus::Result<u64>;

    #[zbus(property, name = "DeferredReason")]
    fn deferred_reason(&self) -> zbus::Result<String>;

    #[zbus(property, name = "LastError")]
    fn last_error(&self) -> zbus::Result<String>;

    #[zbus(property, name = "HealthFailure")]
    fn health_failure(&self) -> zbus::Result<String>;

    #[zbus(property, name = "LastFallback")]
    fn last_fallback(&self) -> zbus::Result<String>;

    #[zbus(property, name = "AutomaticDownloads")]
    fn automatic_downloads(&self) -> zbus::Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BUS_NAME, INTERFACE, OBJECT_PATH};
    use zbus::{
        ObjectPath,
        names::{BusName, InterfaceName},
        proxy::Defaults,
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
