//! Who may call what (contract §8).
//!
//! - Polkit actions are checked with `CheckAuthorization` on a **system-bus-name** subject
//!   (`Subject::new_for_message_header`), never a PID, so a caller cannot race a PID reuse.
//!   `AllowUserInteraction` is passed exactly when the caller set
//!   `ALLOW_INTERACTIVE_AUTHORIZATION` on the message, as the D-Bus specification asks,
//!   whatever the action's shipped default: for a `yes` action polkit never asks anyway,
//!   and a local rule that makes an action stricter (say `apply` → `AUTH_ADMIN`) gets its
//!   dialog instead of a silent refusal.
//! - uid 0 is always allowed (XML contract). It is recognised with the bus's own
//!   credentials (`GetConnectionUnixUser`), which is also how `Automatic` and `ReportBoot`
//!   are restricted to root.
//!
//! [`Authorizer`] is the boundary; [`PolkitAuthorizer`] is the only production
//! implementation. The allow-all fake lives in `apex_updated::testing` (cargo feature
//! `testing`), which the daemon never compiles.

use std::collections::HashMap;

use apex_dbus::{ApexError, PolkitAction};
use tokio::sync::OnceCell;
use zbus::Connection;
use zbus::message::{Flags, Header};
use zbus::names::BusName;
use zbus_polkit::policykit1::{AuthorityProxy, CheckAuthorizationFlags, Subject};

use crate::BoxFuture;

/// Authorization checks for method calls.
pub trait Authorizer: Send + Sync + std::fmt::Debug {
    /// Succeeds if the sender of `header` may perform `action`; otherwise
    /// [`ApexError::NotAuthorized`] with a message for the user.
    fn authorize<'a>(
        &'a self,
        header: &'a Header<'_>,
        action: PolkitAction,
    ) -> BoxFuture<'a, Result<(), ApexError>>;

    /// The Unix uid of the sender of `header`, from the bus credentials.
    fn caller_uid<'a>(&'a self, header: &'a Header<'_>) -> BoxFuture<'a, Result<u32, ApexError>>;
}

/// Succeeds only for callers running as uid 0 (`Automatic`, `ReportBoot`).
pub async fn require_root(auth: &dyn Authorizer, header: &Header<'_>) -> Result<(), ApexError> {
    match auth.caller_uid(header).await? {
        0 => Ok(()),
        uid => Err(ApexError::NotAuthorized(format!(
            "Only the system can call this method (caller uid {uid})."
        ))),
    }
}

/// Whether a polkit check for the call in `header` may show an authentication dialog:
/// exactly when the caller set `ALLOW_INTERACTIVE_AUTHORIZATION` on the message, for every
/// action (see the module documentation).
pub fn allows_interaction(header: &Header<'_>) -> bool {
    header
        .primary()
        .flags()
        .contains(Flags::AllowInteractiveAuth)
}

/// What the user was trying to do, for refusal messages.
pub fn describe(action: PolkitAction) -> &'static str {
    match action {
        PolkitAction::Check => "check for updates",
        PolkitAction::Download => "download updates",
        PolkitAction::Apply => "install updates",
        PolkitAction::Discard => "cancel updates or dismiss update warnings",
        PolkitAction::Rollback => "go back to the previous version",
        PolkitAction::SwitchChannel => "change the update channel",
        PolkitAction::Downgrade => "install an older version",
    }
}

/// The refusal message for a polkit answer that did not authorize.
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

/// Polkit on the system bus.
#[derive(Debug)]
pub struct PolkitAuthorizer {
    connection: Connection,
    authority: OnceCell<AuthorityProxy<'static>>,
}

impl PolkitAuthorizer {
    /// Uses `connection` (the system bus) to reach polkit and the bus daemon.
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
        let subject = Subject::new_for_message_header(header).map_err(|e| {
            ApexError::NotAuthorized(format!("The caller cannot be identified: {e}"))
        })?;
        // `BitFlags` lives in enumflags2, which zbus_polkit does not re-export; naming it
        // would take a direct dependency for one empty value.
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
            zbus::Message::method_call("/io/github/sekirokenjii/Apex/Update1", "Apply").unwrap();
        let builder = if interactive {
            builder.with_flags(Flags::AllowInteractiveAuth).unwrap()
        } else {
            builder
        };
        builder.build(&("sha256:d",)).unwrap()
    }

    /// The dialog follows the caller's flag, also for actions whose shipped default is
    /// `yes` (a local polkit rule may make them `auth_admin`).
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
            refusal(PolkitAction::SwitchChannel, true, true).message(),
            "Authentication to change the update channel was cancelled."
        );
    }
}
