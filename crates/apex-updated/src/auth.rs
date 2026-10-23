use std::collections::HashMap;

use apex_dbus::{ApexError, PolkitAction};
use tokio::sync::OnceCell;
use zbus::Connection;
use zbus::message::{Flags, Header};
use zbus::names::BusName;
use zbus_polkit::policykit1::{AuthorityProxy, CheckAuthorizationFlags, Subject};

use crate::BoxFuture;

pub trait Authorizer: Send + Sync + std::fmt::Debug {
    fn authorize<'a>(
        &'a self,
        header: &'a Header<'_>,
        action: PolkitAction,
    ) -> BoxFuture<'a, Result<(), ApexError>>;

    /// The sender's uid as the bus daemon knows it.
    fn caller_uid<'a>(&'a self, header: &'a Header<'_>) -> BoxFuture<'a, Result<u32, ApexError>>;
}

pub async fn require_root(auth: &dyn Authorizer, header: &Header<'_>) -> Result<(), ApexError> {
    match auth.caller_uid(header).await? {
        0 => Ok(()),
        uid => Err(ApexError::NotAuthorized(format!(
            "Only the system can call this method (caller uid {uid})."
        ))),
    }
}

/// As the D-Bus specification asks, whatever the action's default: a local polkit rule may
/// make a `yes` action need a password, and then the caller gets its dialog.
pub fn allows_interaction(header: &Header<'_>) -> bool {
    header
        .primary()
        .flags()
        .contains(Flags::AllowInteractiveAuth)
}

pub fn describe(action: PolkitAction) -> &'static str {
    match action {
        PolkitAction::Check => "check for updates",
        PolkitAction::Download => "download updates",
        PolkitAction::Apply => "install updates",
        PolkitAction::Discard => "dismiss update warnings",
        PolkitAction::Rollback => "go back to the previous version",
        PolkitAction::Downgrade => "install an older version",
    }
}

pub fn refusal(action: PolkitAction, is_challenge: bool, dismissed: bool) -> ApexError {
    let what = describe(action);
    ApexError::NotAuthorized(if dismissed {
        format!("Authentication to {what} was cancelled.")
    } else if is_challenge {
        format!("Authentication is required to {what}.")
    } else {
        format!("You are not allowed to {what}.")
    })
}

#[derive(Debug)]
pub struct PolkitAuthorizer {
    connection: Connection,
    authority: OnceCell<AuthorityProxy<'static>>,
}

impl PolkitAuthorizer {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            authority: OnceCell::new(),
        }
    }

    async fn authority(&self) -> zbus::Result<&AuthorityProxy<'static>> {
        self.authority
            .get_or_try_init(|| AuthorityProxy::new(&self.connection))
            .await
    }

    async fn check(&self, header: &Header<'_>, action: PolkitAction) -> Result<(), ApexError> {
        if self.uid(header).await? == 0 {
            return Ok(());
        }
        // A bus-name subject, never a PID, which another process could reuse.
        let subject = Subject::new_for_message_header(header).map_err(|e| {
            ApexError::NotAuthorized(format!("The caller cannot be identified: {e}"))
        })?;
        // Naming the empty `BitFlags` would need a direct enumflags2 dependency.
        #[allow(clippy::default_trait_access)]
        let flags = if allows_interaction(header) {
            CheckAuthorizationFlags::AllowUserInteraction.into()
        } else {
            Default::default()
        };
        let details: HashMap<&str, &str> = HashMap::new();
        let unavailable = |e: zbus::Error| {
            tracing::error!(error = %e, action = action.id(), "polkit check failed");
            ApexError::NotAuthorized(format!(
                "Permission to {} could not be checked: {e}",
                describe(action)
            ))
        };
        let result = self
            .authority()
            .await
            .map_err(unavailable)?
            .check_authorization(&subject, action.id(), &details, flags, "")
            .await
            .map_err(unavailable)?;
        if result.is_authorized {
            Ok(())
        } else {
            Err(refusal(
                action,
                result.is_challenge,
                result.details.contains_key("polkit.dismissed"),
            ))
        }
    }

    async fn uid(&self, header: &Header<'_>) -> Result<u32, ApexError> {
        let sender = header.sender().ok_or_else(|| {
            ApexError::NotAuthorized("The caller cannot be identified: no sender.".to_owned())
        })?;
        let dbus = zbus::fdo::DBusProxy::new(&self.connection)
            .await
            .map_err(|e| ApexError::Internal(format!("Cannot reach the bus daemon: {e}")))?;
        dbus.get_connection_unix_user(BusName::Unique(sender.clone()))
            .await
            .map_err(|e| ApexError::NotAuthorized(format!("The caller cannot be identified: {e}")))
    }
}

impl Authorizer for PolkitAuthorizer {
    fn authorize<'a>(
        &'a self,
        header: &'a Header<'_>,
        action: PolkitAction,
    ) -> BoxFuture<'a, Result<(), ApexError>> {
        Box::pin(self.check(header, action))
    }

    fn caller_uid<'a>(&'a self, header: &'a Header<'_>) -> BoxFuture<'a, Result<u32, ApexError>> {
        Box::pin(self.uid(header))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_dbus::ErrorKind;

    fn call(interactive: bool) -> zbus::Message {
        let builder =
            zbus::Message::method_call("/io/github/apex_linux/Apex/Update1", "Apply").unwrap();
        let builder = if interactive {
            builder.with_flags(Flags::AllowInteractiveAuth).unwrap()
        } else {
            builder
        };
        builder.build(&("sha256:d",)).unwrap()
    }

    #[test]
    fn interaction_follows_the_callers_flag() {
        assert!(allows_interaction(&call(true).header()));
        assert!(!allows_interaction(&call(false).header()));
    }

    #[test]
    fn refusals_are_sentences() {
        for action in PolkitAction::ALL {
            for (challenge, dismissed) in [(false, false), (true, false), (true, true)] {
                let error = refusal(action, challenge, dismissed);
                assert_eq!(error.kind(), Some(ErrorKind::NotAuthorized));
                let message = error.message();
                assert!(message.starts_with(char::is_uppercase) && message.ends_with('.'));
            }
        }
        assert_eq!(
            refusal(PolkitAction::Rollback, true, true).message(),
            "Authentication to go back to the previous version was cancelled."
        );
    }
}
