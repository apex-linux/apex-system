use zbus::DBusError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorKind {
    Busy,
    NotAuthorized,
    PolicyDenied,
    NothingToDo,
    InvalidArgument,
    DigestMismatch,
    Bootc,
    Network,
    Internal,
}

impl ErrorKind {
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

    // Spelled out to stay `&'static str`; a unit test checks them against ERROR_PREFIX.
    pub const fn error_name(self) -> &'static str {
        match self {
            Self::Busy => "io.github.apex_linux.Apex.Update1.Error.Busy",
            Self::NotAuthorized => "io.github.apex_linux.Apex.Update1.Error.NotAuthorized",
            Self::PolicyDenied => "io.github.apex_linux.Apex.Update1.Error.PolicyDenied",
            Self::NothingToDo => "io.github.apex_linux.Apex.Update1.Error.NothingToDo",
            Self::InvalidArgument => "io.github.apex_linux.Apex.Update1.Error.InvalidArgument",
            Self::DigestMismatch => "io.github.apex_linux.Apex.Update1.Error.DigestMismatch",
            Self::Bootc => "io.github.apex_linux.Apex.Update1.Error.Bootc",
            Self::Network => "io.github.apex_linux.Apex.Update1.Error.Network",
            Self::Internal => "io.github.apex_linux.Apex.Update1.Error.Internal",
        }
    }
}

/// A daemon passes every error through [`ApexError::into_contract`]. `Display` is for logs;
/// show users [`ApexError::message`].
#[derive(Debug, DBusError)]
#[zbus(prefix = "io.github.apex_linux.Apex.Update1.Error")]
pub enum ApexError {
    /// Transport, bus and non-contract errors. A daemon replying with it would send
    /// `org.freedesktop.zbus.Error`.
    #[zbus(error)]
    ZBus(zbus::Error),
    /// The daemon runs one operation at a time.
    Busy(String),
    /// Polkit, or the uid 0 check of `Automatic` and `ReportBoot`, refused the caller.
    NotAuthorized(String),
    /// An update gate refused; the message is fit for display.
    PolicyDenied(String),
    NothingToDo(String),
    InvalidArgument(String),
    /// `Apply`: the staged digest is not the expected one.
    DigestMismatch(String),
    Bootc(String),
    /// The registry could not be reached.
    Network(String),
    Internal(String),
}

impl ApexError {
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

    /// For [`ApexError::ZBus`], the remote error's description if there is one.
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

    #[must_use]
    pub fn into_contract(self) -> Self {
        match self {
            Self::ZBus(error) => Self::Internal(zbus_message(&error)),
            contract => contract,
        }
    }
}

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
            "io.github.apex_linux.Apex.Update1.Error.PolicyDenied: On battery below 50 %"
        );
    }

    #[test]
    fn contract_errors_convert_from_method_error_replies() {
        for kind in ErrorKind::ALL {
            let name = ErrorName::try_from(kind.error_name()).unwrap();
            // zbus makes a received error reply MethodError(name, description, message).
            let message = zbus::message::Message::method_call("/", "Ping")
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
        let busy = ApexError::new(ErrorKind::Busy, "download running").into_contract();
        assert_eq!(busy.kind(), Some(ErrorKind::Busy));
    }
}
