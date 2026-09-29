use serde::{Deserialize, Serialize};

use crate::digest::Digest;

pub const HEALTH_FAILURE_PATH: &str = "/var/lib/apex/health-failure.json";

/// Readers ignore unknown fields, so fields can be added without a schema bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthFailure {
    pub digest: Digest,
    pub failed_checks: Vec<String>,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

impl HealthFailure {
    pub fn new(digest: Digest, failed_checks: Vec<String>, at: u64) -> Self {
        Self {
            digest,
            failed_checks,
            at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape_matches_the_contract() {
        let record = HealthFailure::new(
            Digest::parse(
                "sha256:4444444444444444444444444444444444444444444444444444444444444444",
            )
            .unwrap(),
            vec!["session".to_owned(), "failed-units".to_owned()],
            1_790_000_000,
        );
        let json = serde_json::to_value(&record).unwrap();
        insta::assert_json_snapshot!(json);
        let back: HealthFailure = serde_json::from_value(json).unwrap();
        assert_eq!(back, record);
    }

    #[test]
    fn readers_ignore_unknown_fields_and_validate_the_digest() {
        let with_extra = r#"{"digest":"sha256:4444444444444444444444444444444444444444444444444444444444444444","failed_checks":[],"at":1,"extra":true}"#;
        assert!(serde_json::from_str::<HealthFailure>(with_extra).is_ok());
        let bad_digest = r#"{"digest":"sha256:44","failed_checks":[],"at":1}"#;
        assert!(serde_json::from_str::<HealthFailure>(bad_digest).is_err());
    }
}
