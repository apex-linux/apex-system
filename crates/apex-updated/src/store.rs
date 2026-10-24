use apex_update::store::{
    DENYLIST_FILE, FALLBACK_FILE, HEALTH_FAILURE_FILE, INTENT_FILE, LAST_CHECK_FILE, SETTINGS_FILE,
    read_json, remove_durable, write_json_atomic,
};
use apex_update::{
    FallbackRecord, HealthFailureRecord, StateDir, StateStore, StoreError, UpdateSettings,
};

use crate::errors::with_sources;

pub const STATE_FILES: [&str; 6] = [
    INTENT_FILE,
    DENYLIST_FILE,
    LAST_CHECK_FILE,
    HEALTH_FAILURE_FILE,
    SETTINGS_FILE,
    FALLBACK_FILE,
];

pub trait Store: StateStore + Send + Sync {
    fn write_health_failure(&self, record: &HealthFailureRecord) -> Result<(), StoreError>;
    fn read_settings(&self) -> Result<UpdateSettings, StoreError>;
    fn write_settings(&self, settings: &UpdateSettings) -> Result<(), StoreError>;
    fn read_fallback(&self) -> Result<Option<FallbackRecord>, StoreError>;
    fn write_fallback(&self, record: &FallbackRecord) -> Result<(), StoreError>;
    fn clear_fallback(&self) -> Result<(), StoreError>;

    /// Every write cleans up after earlier writes of its own file; this, at startup, covers
    /// files rarely written, such as the denylist.
    fn remove_stale_temp_files(&self) -> usize {
        0
    }
}

impl Store for StateDir {
    fn write_health_failure(&self, record: &HealthFailureRecord) -> Result<(), StoreError> {
        write_json_atomic(&self.file(HEALTH_FAILURE_FILE), record)
    }

    fn read_settings(&self) -> Result<UpdateSettings, StoreError> {
        Ok(read_json(&self.file(SETTINGS_FILE))?.unwrap_or_default())
    }

    fn write_settings(&self, settings: &UpdateSettings) -> Result<(), StoreError> {
        write_json_atomic(&self.file(SETTINGS_FILE), settings)
    }

    fn read_fallback(&self) -> Result<Option<FallbackRecord>, StoreError> {
        read_json(&self.file(FALLBACK_FILE))
    }

    fn write_fallback(&self, record: &FallbackRecord) -> Result<(), StoreError> {
        write_json_atomic(&self.file(FALLBACK_FILE), record)
    }

    fn clear_fallback(&self) -> Result<(), StoreError> {
        remove_durable(&self.file(FALLBACK_FILE))
    }

    fn remove_stale_temp_files(&self) -> usize {
        STATE_FILES
            .iter()
            .map(|name| apex_update::store::remove_stale_temp_files(&self.file(name)))
            .sum()
    }
}

/// `NotFlushed` means readers already see the change, so acting as if the old content
/// remained would be wrong.
pub fn unflushed_is_done(result: Result<(), StoreError>, what: &str) -> Result<(), StoreError> {
    match result {
        Err(error) if error.is_applied() => {
            tracing::warn!(
                error = %with_sources(&error),
                "{what}: the change is made, but may not survive a power cut"
            );
            Ok(())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn health_failure_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateDir::new(dir.path());
        assert_eq!(store.read_health_failure().unwrap(), None);
        let record = HealthFailureRecord {
            digest: "sha256:aa".into(),
            failed_checks: vec!["session".into()],
            at: 7,
        };
        store.write_health_failure(&record).unwrap();
        assert_eq!(store.read_health_failure().unwrap(), Some(record));
        store.clear_health_failure().unwrap();
        assert_eq!(store.read_health_failure().unwrap(), None);
    }

    #[test]
    fn settings_and_fallback_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateDir::new(dir.path());
        assert!(
            store.read_settings().unwrap().automatic_downloads,
            "the default"
        );
        store
            .write_settings(&UpdateSettings {
                automatic_downloads: false,
            })
            .unwrap();
        assert!(
            !StateDir::new(dir.path())
                .read_settings()
                .unwrap()
                .automatic_downloads
        );
        assert_eq!(store.read_fallback().unwrap(), None);
        let record = FallbackRecord {
            digest: "sha256:ff".into(),
            at: 9,
        };
        store.write_fallback(&record).unwrap();
        assert_eq!(store.read_fallback().unwrap(), Some(record));
        store.clear_fallback().unwrap();
        assert_eq!(store.read_fallback().unwrap(), None);
    }

    /// Above the kernel's `PID_MAX_LIMIT` (4194304).
    const DEAD_PID: u32 = 4_194_305;

    #[test]
    fn startup_removes_what_dead_writers_left_of_every_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateDir::new(dir.path());
        let dead: Vec<PathBuf> = STATE_FILES
            .iter()
            .map(|name| dir.path().join(format!(".{name}.tmp-{DEAD_PID}-0")))
            .collect();
        let own = dir
            .path()
            .join(format!(".denylist.json.tmp-{}-7", std::process::id()));
        let other = dir.path().join(".unrelated.json.tmp-4194305-0");
        for path in dead.iter().chain([&own, &other]) {
            std::fs::write(path, b"{").unwrap();
        }
        if !std::path::Path::new("/proc/self").exists() {
            // Without /proc nothing is known about the writers, so nothing is removed.
            assert_eq!(store.remove_stale_temp_files(), 0);
            return;
        }
        assert_eq!(store.remove_stale_temp_files(), STATE_FILES.len());
        for path in &dead {
            assert!(!path.exists(), "{}", path.display());
        }
        assert!(own.exists(), "this process may still be writing it");
        assert!(other.exists(), "not a state file");
        assert_eq!(store.remove_stale_temp_files(), 0);
    }

    #[test]
    fn an_unflushed_change_is_done_and_other_errors_stay_errors() {
        let unflushed = StoreError::NotFlushed {
            path: "/var/lib/apex/denylist.json".into(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert!(unflushed_is_done(Err(unflushed), "saving the denylist").is_ok());
        assert!(unflushed_is_done(Ok(()), "saving the denylist").is_ok());
        let failed = StoreError::Io {
            action: "rename",
            path: "/var/lib/apex/denylist.json".into(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert!(matches!(
            unflushed_is_done(Err(failed), "saving the denylist"),
            Err(StoreError::Io { .. })
        ));
    }
}
