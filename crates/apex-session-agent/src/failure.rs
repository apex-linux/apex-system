use crate::agent::Notification;
use crate::updates::{self, Digested};

pub const ICON: &str = "dialog-warning-symbolic";

/// Board 26; Settings' Software Updates page shows the rows and the buttons.
pub const FALLBACK_SUMMARY: &str = "Apex Went Back to the Previous Version";

pub const FALLBACK_BODY: &str = "The update did not start properly three times in a row, so Apex \
started the version you had before. That build will not be offered again; a newer one will be.";

pub const HEALTH_SUMMARY: &str = "Update Failed Its Startup Check";

const HEALTH_BODY_REST: &str = "is running, but a required check did not pass. Going back to the \
version you had before is the safe choice. Nothing restarts until you say so.";

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
    /// The booted version, which the health-failure text names.
    pub running: Option<String>,
}

impl Digested for Failure {
    fn digest(&self) -> &str {
        &self.digest
    }
}

impl Failure {
    /// From Update1's `HealthFailure` and `LastFallback`; a fallback is the graver news.
    pub fn from_properties(
        health_failure: &str,
        last_fallback: &str,
        running: Option<&str>,
    ) -> Option<Self> {
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
            running: running.map(str::to_owned),
        })
    }

    pub fn notification(&self) -> Notification {
        let (summary, body) = match (self.kind, &self.running) {
            (Kind::Fallback, _) => (FALLBACK_SUMMARY, FALLBACK_BODY.to_owned()),
            (Kind::Health, Some(version)) => {
                (HEALTH_SUMMARY, format!("Apex {version} {HEALTH_BODY_REST}"))
            }
            (Kind::Health, None) => (
                HEALTH_SUMMARY,
                format!("The new version {HEALTH_BODY_REST}"),
            ),
        };
        Notification {
            app: updates::APP_NAME,
            icon: ICON,
            summary: summary.to_owned(),
            body,
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
        assert_eq!(Failure::from_properties("", "", None), None);
        assert_eq!(
            Failure::from_properties("sha256:h", "", Some("45.1")),
            Some(Failure {
                kind: Kind::Health,
                digest: "sha256:h".to_owned(),
                running: Some("45.1".to_owned()),
            })
        );
        assert_eq!(
            Failure::from_properties("sha256:h", "sha256:f", None).map(|f| f.kind),
            Some(Kind::Fallback)
        );
    }

    #[test]
    fn each_kind_has_its_own_words_and_the_same_action() {
        let fallback = Failure::from_properties("", "sha256:f", Some("45.20260925.118"))
            .unwrap()
            .notification();
        let health = Failure::from_properties("sha256:h", "", Some("45.20260925.118"))
            .unwrap()
            .notification();
        assert_eq!(fallback.summary, FALLBACK_SUMMARY);
        assert_eq!(fallback.body, FALLBACK_BODY);
        assert_eq!(health.summary, HEALTH_SUMMARY);
        assert!(
            health
                .body
                .starts_with("Apex 45.20260925.118 is running, but a required check"),
            "{}",
            health.body
        );
        assert!(!health.body.contains("sha256"), "versions, not digests");
        let unknown = Failure::from_properties("sha256:h", "", None).unwrap();
        assert!(
            unknown
                .notification()
                .body
                .starts_with("The new version is running")
        );
        assert_eq!(fallback.app, "Apex Updates");
        assert_eq!(fallback.actions, health.actions);
        assert_eq!(fallback.actions[1], (updates::ACTION_KEY, ACTION_LABEL));
    }
}
