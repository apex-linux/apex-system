//! Polkit actions guarding the Update1 methods (contract §1 and §8).

use std::fmt;

/// A polkit authorization default (`<allow_any>`, `<allow_inactive>`, `<allow_active>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolkitDefault {
    /// `no`: never authorized.
    No,
    /// `yes`: authorized without a prompt.
    Yes,
    /// `auth_admin`: an administrator must authenticate, every time.
    AuthAdmin,
}

impl PolkitDefault {
    /// The value as written in a `.policy` file.
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

/// A polkit action of the update service.
///
/// The contract spells action ids in lowercase (`io.github.sekirokenjii.apex.update.*`), unlike
/// the bus name; polkit itself accepts `[A-Za-z0-9.-]`. Inactive and remote sessions get
/// [`PolkitDefault::No`] for every action ([`PolkitAction::ALLOW_ANY`],
/// [`PolkitAction::ALLOW_INACTIVE`]); uid 0 is always allowed. `Automatic` and `ReportBoot` are
/// not polkit actions: the daemon accepts them from uid 0 only, checked with the bus credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PolkitAction {
    /// `Check`.
    Check,
    /// `Download`.
    Download,
    /// `Apply` (applies an already verified, reviewed digest).
    Apply,
    /// `DiscardStaged`, and `AcknowledgeHealthFailure` (contract §8): dismissing the
    /// warning about a failed update is a routine decision of the same weight as cancelling
    /// one.
    Discard,
    /// `Rollback`.
    Rollback,
    /// `SwitchChannel`.
    SwitchChannel,
    /// Moving to an older version. The contract defines the action, but no Update1 method is
    /// documented as requiring it yet.
    Downgrade,
}

impl PolkitAction {
    /// Every action, in contract order.
    pub const ALL: [Self; 7] = [
        Self::Check,
        Self::Download,
        Self::Apply,
        Self::Discard,
        Self::Rollback,
        Self::SwitchChannel,
        Self::Downgrade,
    ];

    /// The polkit action id.
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

    /// The action whose id is `id`, if any.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }

    /// The action that guards the Update1 method `member` (its D-Bus name), contract §8.
    ///
    /// `None` for the methods polkit does not guard: `Ping` and `GetChanges` are read-only,
    /// and `Automatic` and `ReportBoot` are accepted from uid 0 only (bus credentials). Also
    /// `None` for a name that is not an Update1 method. [`PolkitAction::Downgrade`] guards no
    /// method yet.
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

    /// `<allow_any>` of every action.
    pub const ALLOW_ANY: PolkitDefault = PolkitDefault::No;

    /// `<allow_inactive>` of every action.
    pub const ALLOW_INACTIVE: PolkitDefault = PolkitDefault::No;

    /// `<allow_active>`: the default for an active local session.
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

/// Polkit action id for `Check`.
pub const POLKIT_CHECK: &str = "io.github.sekirokenjii.apex.update.check";
/// Polkit action id for `Download`.
pub const POLKIT_DOWNLOAD: &str = "io.github.sekirokenjii.apex.update.download";
/// Polkit action id for `Apply`.
pub const POLKIT_APPLY: &str = "io.github.sekirokenjii.apex.update.apply";
/// Polkit action id for `DiscardStaged` and `AcknowledgeHealthFailure`.
pub const POLKIT_DISCARD: &str = "io.github.sekirokenjii.apex.update.discard";
/// Polkit action id for `Rollback`.
pub const POLKIT_ROLLBACK: &str = "io.github.sekirokenjii.apex.update.rollback";
/// Polkit action id for `SwitchChannel`.
pub const POLKIT_SWITCH_CHANNEL: &str = "io.github.sekirokenjii.apex.update.switch-channel";
/// Polkit action id for moving to an older version.
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
    fn guarded_methods_match_contract_section_8() {
        let table = [
            ("Ping", None),
            ("Check", Some(PolkitAction::Check)),
            ("Download", Some(PolkitAction::Download)),
            ("Apply", Some(PolkitAction::Apply)),
            ("DiscardStaged", Some(PolkitAction::Discard)),
            ("Rollback", Some(PolkitAction::Rollback)),
            ("GetChanges", None),
            ("SwitchChannel", Some(PolkitAction::SwitchChannel)),
            // Contract §8: guarded by the discard action, not apply.
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
