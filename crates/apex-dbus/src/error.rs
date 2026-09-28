//! Error replies of the Update1 interface.

use zbus::DBusError;

/// The kind of an [`ApexError`], without its message.
///
/// The D-Bus error name is `<ERROR_PREFIX>.<kind>` (see [`ErrorKind::error_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorKind {
    /// Another operation is running; the daemon runs one operation at a time.
    Busy,
    /// Polkit, or the uid-0 check of `Automatic` and `ReportBoot`, refused the caller.
    NotAuthorized,
    /// An update gate refused the operation; the message is a reason fit for display.
    PolicyDenied,
    /// There is nothing to do (for example no update is available or nothing is staged).
    NothingToDo,
    /// An argument is outside what the contract allows.
    InvalidArgument,
    /// `Apply`: the staged digest is not the expected one.
    DigestMismatch,
    /// bootc failed.
    Bootc,
    /// The registry could not be reached.
    Network,
    /// Anything else that went wrong inside the daemon.
    Internal,
}

impl ErrorKind {
    /// Every kind, in contract order.
    pub const ALL: [Self; 9] = [
        Self::Busy,
        Self::NotAuthorized,
        Self::PolicyDenied,
        Self::NothingToDo,
        Self::InvalidArgument,
        Self::DigestMismatch,
        Self::Bootc,
        Self::Network,
        Self::Internal,
    ];

    /// The `<Kind>` part of the error name, e.g. `"PolicyDenied"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "Busy",
            Self::NotAuthorized => "NotAuthorized",
            Self::PolicyDenied => "PolicyDenied",
            Self::NothingToDo => "NothingToDo",
            Self::InvalidArgument => "InvalidArgument",
            Self::DigestMismatch => "DigestMismatch",
            Self::Bootc => "Bootc",
            Self::Network => "Network",
            Self::Internal => "Internal",
        }
    }

    /// The full D-Bus error name, e.g. `"io.github.sekirokenjii.Apex.Update1.Error.Busy"`.
    // Spelled out (rather than formatted from ERROR_PREFIX) so the names stay `&'static str`;
    // a unit test checks every one against ERROR_PREFIX and the DBusError derive.
    pub const fn error_name(self) -> &'static str {
        match self {
            Self::Busy => "io.github.sekirokenjii.Apex.Update1.Error.Busy",
            Self::NotAuthorized => "io.github.sekirokenjii.Apex.Update1.Error.NotAuthorized",
            Self::PolicyDenied => "io.github.sekirokenjii.Apex.Update1.Error.PolicyDenied",
            Self::NothingToDo => "io.github.sekirokenjii.Apex.Update1.Error.NothingToDo",
            Self::InvalidArgument => "io.github.sekirokenjii.Apex.Update1.Error.InvalidArgument",
            Self::DigestMismatch => "io.github.sekirokenjii.Apex.Update1.Error.DigestMismatch",
            Self::Bootc => "io.github.sekirokenjii.Apex.Update1.Error.Bootc",
            Self::Network => "io.github.sekirokenjii.Apex.Update1.Error.Network",
            Self::Internal => "io.github.sekirokenjii.Apex.Update1.Error.Internal",
        }
    }
}

/// An error of the Update1 interface.
///
/// The nine contract variants map one-to-one to the D-Bus errors
/// `io.github.sekirokenjii.Apex.Update1.Error.<Kind>`, each carrying a human-readable message
/// (for [`ApexError::PolicyDenied`] the message is the gate reason, fit for display).
///
/// - **Daemon:** use it as the error type of the `#[zbus::interface]` methods; zbus replies
///   with the variant's error name and the message as the only body field. Pass errors through
///   [`ApexError::into_contract`] first so a stray [`ApexError::ZBus`] goes out as `Internal`.
/// - **Clients:** every [`Update1Proxy`](crate::Update1Proxy) method returns this type. A
///   contract error reply becomes its variant; anything else (no bus, service not activatable,
///   bus policy denial, a malformed reply) is [`ApexError::ZBus`].
///
/// `Display` prints `<error name>: <message>` for logs; show [`ApexError::message`] to users.
#[derive(Debug, DBusError)]
#[zbus(prefix = "io.github.sekirokenjii.Apex.Update1.Error")]
pub enum ApexError {
    /// A transport, bus or non-contract error seen by a client. Not part of the contract: if a
    /// daemon replied with it, the name would be `org.freedesktop.zbus.Error`.
    #[zbus(error)]
    ZBus(zbus::Error),
    /// Another operation is running; the daemon runs one operation at a time.
    Busy(String),
    /// Polkit, or the uid-0 check of `Automatic` and `ReportBoot`, refused the caller.
    NotAuthorized(String),
    /// An update gate refused the operation; the message is a reason fit for display.
    PolicyDenied(String),
    /// There is nothing to do (for example no update is available or nothing is staged).
    NothingToDo(String),
    /// An argument is outside what the contract allows.
    InvalidArgument(String),
    /// `Apply`: the staged digest is not the expected one.
    DigestMismatch(String),
    /// bootc failed.
    Bootc(String),
    /// The registry could not be reached.
    Network(String),
    /// Anything else that went wrong inside the daemon.
    Internal(String),
}

impl ApexError {
    /// Builds the contract error of `kind` with `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        let message = message.into();
        match kind {
            ErrorKind::Busy => Self::Busy(message),
            ErrorKind::NotAuthorized => Self::NotAuthorized(message),
            ErrorKind::PolicyDenied => Self::PolicyDenied(message),
            ErrorKind::NothingToDo => Self::NothingToDo(message),
            ErrorKind::InvalidArgument => Self::InvalidArgument(message),
            ErrorKind::DigestMismatch => Self::DigestMismatch(message),
            ErrorKind::Bootc => Self::Bootc(message),
            ErrorKind::Network => Self::Network(message),
            ErrorKind::Internal => Self::Internal(message),
        }
    }

    /// The contract kind, or `None` for [`ApexError::ZBus`].
    pub fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::ZBus(_) => return None,
            Self::Busy(_) => ErrorKind::Busy,
            Self::NotAuthorized(_) => ErrorKind::NotAuthorized,
            Self::PolicyDenied(_) => ErrorKind::PolicyDenied,
            Self::NothingToDo(_) => ErrorKind::NothingToDo,
            Self::InvalidArgument(_) => ErrorKind::InvalidArgument,
            Self::DigestMismatch(_) => ErrorKind::DigestMismatch,
            Self::Bootc(_) => ErrorKind::Bootc,
            Self::Network(_) => ErrorKind::Network,
            Self::Internal(_) => ErrorKind::Internal,
        })
    }

    /// The human-readable message. For [`ApexError::ZBus`] this is the remote error's
    /// description when there is one, otherwise the zbus error text.
    pub fn message(&self) -> String {
        match self {
            Self::ZBus(error) => zbus_message(error),
            Self::Busy(message)
            | Self::NotAuthorized(message)
            | Self::PolicyDenied(message)
            | Self::NothingToDo(message)
            | Self::InvalidArgument(message)
            | Self::DigestMismatch(message)
            | Self::Bootc(message)
            | Self::Network(message)
            | Self::Internal(message) => message.clone(),
        }
    }

    /// Returns `self` unless it is [`ApexError::ZBus`], which becomes
    /// [`ApexError::Internal`] with the zbus error's text.
    ///
    /// The daemon calls this on every error it returns, so replies only ever carry contract
    /// error names.
    #[must_use]
    pub fn into_contract(self) -> Self {
        match self {
            Self::ZBus(error) => Self::Internal(zbus_message(&error)),
            contract => contract,
        }
    }
}

/// The remote description of a method error, or the zbus error text.
fn zbus_message(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(_, Some(description), _) => description.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ERROR_PREFIX;
    use zbus::names::ErrorName;

    #[test]
    fn error_names_follow_the_prefix_and_the_derive() {
        for kind in ErrorKind::ALL {
            let expected = format!("{ERROR_PREFIX}.{}", kind.as_str());
            assert_eq!(kind.error_name(), expected);
            let error = ApexError::new(kind, "m");
            assert_eq!(DBusError::name(&error).as_str(), expected);
            assert_eq!(error.kind(), Some(kind));
            ErrorName::try_from(kind.error_name()).expect("valid D-Bus error name");
        }
    }

    #[test]
    fn message_is_the_description() {
        let error = ApexError::new(ErrorKind::PolicyDenied, "On battery below 50 %");
        assert_eq!(error.message(), "On battery below 50 %");
        assert_eq!(
            DBusError::description(&error),
            Some("On battery below 50 %")
        );
        assert_eq!(
            error.to_string(),
            "io.github.sekirokenjii.Apex.Update1.Error.PolicyDenied: On battery below 50 %"
        );
    }

    #[test]
    fn contract_errors_convert_from_method_error_replies() {
        for kind in ErrorKind::ALL {
            let name = ErrorName::try_from(kind.error_name()).unwrap();
            // zbus turns a received error reply into MethodError(name, description, message);
            // the message itself does not matter for the conversion.
            let message = zbus::message::Message::method_call("/", "Ping")
                .unwrap()
                .build(&())
                .unwrap();
            let error = zbus::Error::MethodError(name.into(), Some("why".to_owned()), message);
            let converted = ApexError::from(error);
            assert_eq!(converted.kind(), Some(kind));
            assert_eq!(converted.message(), "why");
        }
    }

    #[test]
    fn foreign_errors_stay_zbus_and_become_internal_on_the_wire() {
        let converted = ApexError::from(zbus::Error::Failure("socket closed".to_owned()));
        assert!(matches!(converted, ApexError::ZBus(_)));
        assert_eq!(converted.kind(), None);
        let wire = converted.into_contract();
        assert_eq!(wire.kind(), Some(ErrorKind::Internal));
        assert!(wire.message().contains("socket closed"));
        // Contract variants pass through unchanged.
        let busy = ApexError::new(ErrorKind::Busy, "download running").into_contract();
        assert_eq!(busy.kind(), Some(ErrorKind::Busy));
    }
}
