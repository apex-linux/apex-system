use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolkitDefault {
    No,
    Yes,
    /// Every time, unlike `auth_admin_keep`.
    AuthAdmin,
}

impl PolkitDefault {
    /// As written in a `.policy` file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::Yes => "yes",
            Self::AuthAdmin => "auth_admin",
        }
    }
}

impl fmt::Display for PolkitDefault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Action ids are lowercase, unlike the bus name. `Automatic` and `ReportBoot` have no action:
/// the daemon accepts them from uid 0 only, by the bus credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PolkitAction {
    Check,
    Download,
    Apply,
    /// Also guards `AcknowledgeHealthFailure`: dismissing a failure warning weighs the same.
    Discard,
    Rollback,
    SwitchChannel,
    /// Reserved: no method checks it yet.
    Downgrade,
}

impl PolkitAction {
    pub const ALL: [Self; 7] = [
        Self::Check,
        Self::Download,
        Self::Apply,
        Self::Discard,
        Self::Rollback,
        Self::SwitchChannel,
        Self::Downgrade,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Check => POLKIT_CHECK,
            Self::Download => POLKIT_DOWNLOAD,
            Self::Apply => POLKIT_APPLY,
            Self::Discard => POLKIT_DISCARD,
            Self::Rollback => POLKIT_ROLLBACK,
            Self::SwitchChannel => POLKIT_SWITCH_CHANNEL,
            Self::Downgrade => POLKIT_DOWNGRADE,
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }

    /// `member` is the D-Bus method name.
    pub fn guarding(member: &str) -> Option<Self> {
        match member {
            "Check" => Some(Self::Check),
            "Download" => Some(Self::Download),
            "Apply" => Some(Self::Apply),
            "DiscardStaged" | "AcknowledgeHealthFailure" => Some(Self::Discard),
            "Rollback" => Some(Self::Rollback),
            "SwitchChannel" => Some(Self::SwitchChannel),
            _ => None,
        }
    }

    pub const ALLOW_ANY: PolkitDefault = PolkitDefault::No;

    pub const ALLOW_INACTIVE: PolkitDefault = PolkitDefault::No;

    pub const fn allow_active(self) -> PolkitDefault {
        match self {
            Self::Check | Self::Download | Self::Apply | Self::Discard => PolkitDefault::Yes,
            Self::Rollback | Self::SwitchChannel | Self::Downgrade => PolkitDefault::AuthAdmin,
        }
    }
}

impl fmt::Display for PolkitAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

pub const POLKIT_CHECK: &str = "io.github.sekirokenjii.apex.update.check";
pub const POLKIT_DOWNLOAD: &str = "io.github.sekirokenjii.apex.update.download";
pub const POLKIT_APPLY: &str = "io.github.sekirokenjii.apex.update.apply";
pub const POLKIT_DISCARD: &str = "io.github.sekirokenjii.apex.update.discard";
pub const POLKIT_ROLLBACK: &str = "io.github.sekirokenjii.apex.update.rollback";
pub const POLKIT_SWITCH_CHANNEL: &str = "io.github.sekirokenjii.apex.update.switch-channel";
pub const POLKIT_DOWNGRADE: &str = "io.github.sekirokenjii.apex.update.downgrade";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_share_the_contract_prefix_and_are_lowercase() {
        for action in PolkitAction::ALL {
            let id = action.id();
            let suffix = id
                .strip_prefix("io.github.sekirokenjii.apex.update.")
                .expect("contract prefix");
            assert!(!suffix.is_empty() && !suffix.contains('.'), "{id}");
            assert!(
                id.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'-'),
                "{id}: the contract spells action ids in lowercase"
            );
            assert_eq!(PolkitAction::from_id(id), Some(action));
            assert_eq!(action.to_string(), id);
        }
        assert_eq!(
            PolkitAction::from_id("io.github.sekirokenjii.apex.update"),
            None
        );
    }

    #[test]
    fn defaults_match_contract_table() {
        use PolkitDefault::{AuthAdmin, No, Yes};
        let table = [
            (PolkitAction::Check, Yes),
            (PolkitAction::Download, Yes),
            (PolkitAction::Apply, Yes),
            (PolkitAction::Discard, Yes),
            (PolkitAction::Rollback, AuthAdmin),
            (PolkitAction::SwitchChannel, AuthAdmin),
            (PolkitAction::Downgrade, AuthAdmin),
        ];
        assert_eq!(PolkitAction::ALLOW_ANY, No);
        assert_eq!(PolkitAction::ALLOW_INACTIVE, No);
        for (action, active) in table {
            assert_eq!(action.allow_active(), active, "{action}");
        }
        assert_eq!(AuthAdmin.to_string(), "auth_admin");
    }

    #[test]
    fn guarded_methods_match_the_contract() {
        let table = [
            ("Ping", None),
            ("Check", Some(PolkitAction::Check)),
            ("Download", Some(PolkitAction::Download)),
            ("Apply", Some(PolkitAction::Apply)),
            ("DiscardStaged", Some(PolkitAction::Discard)),
            ("Rollback", Some(PolkitAction::Rollback)),
            ("GetChanges", None),
            ("SwitchChannel", Some(PolkitAction::SwitchChannel)),
            ("AcknowledgeHealthFailure", Some(PolkitAction::Discard)),
            ("Automatic", None),
            ("ReportBoot", None),
        ];
        for (member, action) in table {
            assert_eq!(PolkitAction::guarding(member), action, "{member}");
        }
        assert_eq!(PolkitAction::guarding("apply"), None, "D-Bus names only");
        assert_eq!(
            PolkitAction::guarding("State"),
            None,
            "properties are unguarded"
        );
    }
}
