use crate::agent::Notification;
use crate::updates::{self, Digested};

pub const ICON: &str = "dialog-warning-symbolic";

/// Placeholders until board 26's wording is final; the notice and the action stay.
pub const FALLBACK_SUMMARY: &str = "Update Did Not Start";

pub const FALLBACK_BODY: &str = "The update failed to start, so Apex went back to the previous \
version. Your files and settings are unchanged.";

pub const HEALTH_SUMMARY: &str = "Update Failed Its Checks";

pub const HEALTH_BODY: &str = "The update started but failed its health checks. You can go back \
to the previous version in Software Updates.";

pub const ACTION_LABEL: &str = "Open Software Updates";

/// `gnome-control-center system software-updates`.
pub const SETTINGS_PANEL: &str = "system";

pub const SETTINGS_PAGE: &str = "software-updates";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// GRUB fell back: the update never started (`LastFallback`).
    Fallback,
    /// The update started and failed its health checks (`HealthFailure`).
    Health,
}

/// A failed update the user has not acknowledged yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: Kind,
    pub digest: String,
}

impl Digested for Failure {
    fn digest(&self) -> &str {
        &self.digest
    }
}

impl Failure {
    /// From Update1's `HealthFailure` and `LastFallback`; a fallback is the graver news.
    pub fn from_properties(health_failure: &str, last_fallback: &str) -> Option<Self> {
        let (kind, digest) = if !last_fallback.is_empty() {
            (Kind::Fallback, last_fallback)
        } else if !health_failure.is_empty() {
            (Kind::Health, health_failure)
        } else {
            return None;
        };
        Some(Self {
            kind,
            digest: digest.to_owned(),
        })
    }

    pub fn notification(&self) -> Notification {
        let (summary, body) = match self.kind {
            Kind::Fallback => (FALLBACK_SUMMARY, FALLBACK_BODY),
            Kind::Health => (HEALTH_SUMMARY, HEALTH_BODY),
        };
        Notification {
            icon: ICON,
            summary: summary.to_owned(),
            body: body.to_owned(),
            actions: vec![
                (updates::DEFAULT_ACTION_KEY, ACTION_LABEL),
                (updates::ACTION_KEY, ACTION_LABEL),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fallback_wins_over_a_health_failure() {
        assert_eq!(Failure::from_properties("", ""), None);
        assert_eq!(
            Failure::from_properties("sha256:h", ""),
            Some(Failure {
                kind: Kind::Health,
                digest: "sha256:h".to_owned()
            })
        );
        assert_eq!(
            Failure::from_properties("sha256:h", "sha256:f"),
            Some(Failure {
                kind: Kind::Fallback,
                digest: "sha256:f".to_owned()
            })
        );
    }

    #[test]
    fn each_kind_has_its_own_words_and_the_same_action() {
        let fallback = Failure::from_properties("", "sha256:f")
            .unwrap()
            .notification();
        let health = Failure::from_properties("sha256:h", "")
            .unwrap()
            .notification();
        assert_eq!(fallback.summary, FALLBACK_SUMMARY);
        assert_eq!(health.summary, HEALTH_SUMMARY);
        assert_eq!(fallback.actions, health.actions);
        assert_eq!(fallback.actions[1], (updates::ACTION_KEY, ACTION_LABEL));
    }
}
