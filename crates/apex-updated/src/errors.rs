use apex_bootc::{BootcError, CommandKind};
use apex_dbus::{ApexError, ErrorKind};
use apex_update::{ApplyError, Deferral, DiscardError, DownloadError, StoreError};

pub fn from_bootc(error: &BootcError) -> ApexError {
    let message = error.to_string();
    let kind = match error {
        BootcError::Busy { .. } => ErrorKind::Busy,
        BootcError::Network { .. } => ErrorKind::Network,
        BootcError::Timeout { command, .. } if talks_to_registry(*command) => ErrorKind::Network,
        BootcError::Spawn { .. } | BootcError::InvalidStatus(_) | BootcError::Cancelled { .. } => {
            ErrorKind::Internal
        }
        _ => ErrorKind::Bootc,
    };
    ApexError::new(kind, capitalize(&message))
}

fn talks_to_registry(command: CommandKind) -> bool {
    matches!(
        command,
        CommandKind::UpgradeCheck
            | CommandKind::UpgradeDownloadOnly
            | CommandKind::SwitchDownloadOnly
    )
}

pub fn from_store(error: &StoreError) -> ApexError {
    ApexError::Internal(format!(
        "The update records in /var/lib/apex could not be used: {}",
        with_sources(error)
    ))
}

pub fn from_deferral(deferral: &Deferral) -> ApexError {
    ApexError::PolicyDenied(deferral.reason.clone())
}

pub fn from_download(error: &DownloadError) -> ApexError {
    match error {
        DownloadError::NothingToDownload => ApexError::NothingToDo(error.to_string()),
        DownloadError::Gate(deferral) => from_deferral(deferral),
    }
}

pub fn from_apply(error: &ApplyError) -> ApexError {
    match error {
        ApplyError::NothingStaged | ApplyError::AlreadyApplied => {
            ApexError::NothingToDo(error.to_string())
        }
        ApplyError::DigestMismatch { expected, staged } => ApexError::DigestMismatch(format!(
            "{error} (reviewed {expected}, downloaded {staged})"
        )),
        // A downgrade needs the `downgrade` polkit action, which no method uses yet.
        ApplyError::Downgrade { .. } => ApexError::PolicyDenied(format!(
            "{error} Installing an older version is not supported."
        )),
        ApplyError::Gate(deferral) => from_deferral(deferral),
    }
}

pub fn from_discard(error: &DiscardError) -> ApexError {
    match error {
        DiscardError::NothingStaged | DiscardError::NotApplied => {
            ApexError::NothingToDo(error.to_string())
        }
        DiscardError::Gate(deferral) => from_deferral(deferral),
    }
}

pub fn internal(context: &str, error: &dyn std::error::Error) -> ApexError {
    ApexError::Internal(format!("{context}: {}", with_sources(error)))
}

/// Whether `LastError` keeps it: a refusal goes only to the caller, but a failure of the work
/// itself may have been the timer's, which has no one to tell.
pub fn is_recorded(error: &ApexError) -> bool {
    matches!(
        error.kind(),
        Some(ErrorKind::Bootc | ErrorKind::Network | ErrorKind::Internal) | None
    )
}

/// Skips a source whose text is already there: many errors print their source themselves.
pub fn with_sources(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_bootc::ExitState;
    use apex_update::Gate;
    use std::time::Duration;

    #[test]
    fn bootc_kinds() {
        let cases = [
            (
                BootcError::Busy {
                    detail: "lock".into(),
                },
                ErrorKind::Busy,
            ),
            (
                BootcError::Network {
                    detail: "dns".into(),
                },
                ErrorKind::Network,
            ),
            (
                BootcError::Timeout {
                    command: CommandKind::UpgradeDownloadOnly,
                    timeout: Duration::from_hours(2),
                },
                ErrorKind::Network,
            ),
            (
                BootcError::Timeout {
                    command: CommandKind::Rollback,
                    timeout: Duration::from_mins(10),
                },
                ErrorKind::Bootc,
            ),
            (
                BootcError::SignatureRejected {
                    detail: "no signature".into(),
                },
                ErrorKind::Bootc,
            ),
            (
                BootcError::Cancelled {
                    command: CommandKind::Status,
                },
                ErrorKind::Internal,
            ),
            (
                BootcError::Other {
                    command: CommandKind::Rollback,
                    exit: ExitState::Code(1),
                    detail: "x".into(),
                },
                ErrorKind::Bootc,
            ),
        ];
        for (error, kind) in cases {
            let mapped = from_bootc(&error);
            assert_eq!(mapped.kind(), Some(kind), "{error}");
            assert!(
                !mapped.message().starts_with(char::is_lowercase),
                "{}",
                mapped.message()
            );
        }
    }

    #[test]
    fn engine_refusals() {
        let gate = Deferral::new(Gate::Metered, "Metered.");
        assert_eq!(
            from_download(&DownloadError::Gate(gate.clone())).kind(),
            Some(ErrorKind::PolicyDenied)
        );
        assert_eq!(from_deferral(&gate).message(), "Metered.");
        assert_eq!(
            from_download(&DownloadError::NothingToDownload).kind(),
            Some(ErrorKind::NothingToDo)
        );
        let mismatch = ApplyError::DigestMismatch {
            expected: "sha256:a".into(),
            staged: "sha256:b".into(),
        };
        assert_eq!(
            from_apply(&mismatch).kind(),
            Some(ErrorKind::DigestMismatch)
        );
        assert_eq!(
            from_apply(&ApplyError::AlreadyApplied).kind(),
            Some(ErrorKind::NothingToDo)
        );
        assert_eq!(
            from_discard(&DiscardError::NotApplied).kind(),
            Some(ErrorKind::NothingToDo)
        );
    }

    #[test]
    fn sources_are_not_repeated() {
        let error = BootcError::Spawn {
            program: "/usr/bin/bootc".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let text = with_sources(&error);
        assert_eq!(text.matches("entity not found").count(), 1, "{text}");
        let store = StoreError::Io {
            action: "open",
            path: "/var/lib/apex/intent.json".into(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert_eq!(
            with_sources(&store),
            "cannot open /var/lib/apex/intent.json: permission denied"
        );
    }

    #[test]
    fn recorded_kinds() {
        assert!(is_recorded(&ApexError::Network("x".into())));
        assert!(is_recorded(&ApexError::Internal("x".into())));
        assert!(!is_recorded(&ApexError::PolicyDenied("x".into())));
        assert!(!is_recorded(&ApexError::Busy("x".into())));
    }
}
