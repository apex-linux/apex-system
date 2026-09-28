//! The persisted state (`/var/lib/apex`, contract §4) as the daemon uses it.
//!
//! `apex_update::StateStore` covers every file except one write: `health-failure.json` is
//! normally written by `apex-boot-assess`, but `ReportBoot("red")` must also record it when
//! that write failed (the XML contract: the daemon publishes `HealthFailure`). [`Store`]
//! adds that write, and the startup cleanup of temporary files ([`Store::remove_stale_temp_files`]).
//!
//! A write whose rename (or removal) happened but whose directory flush failed is
//! `StoreError::NotFlushed`: readers already see the change. [`unflushed_is_done`] turns it
//! into a success with a warning, so the daemon never acts as if such a file still held its
//! old content.

use apex_update::store::{
    DENYLIST_FILE, HEALTH_FAILURE_FILE, INTENT_FILE, LAST_CHECK_FILE, write_json_atomic,
};
use apex_update::{HealthFailureRecord, StateDir, StateStore, StoreError};

use crate::errors::with_sources;

/// Every state file (M1 contract §4).
pub const STATE_FILES: [&str; 4] = [
    INTENT_FILE,
    DENYLIST_FILE,
    LAST_CHECK_FILE,
    HEALTH_FAILURE_FILE,
];

/// The daemon's persisted state.
pub trait Store: StateStore + Send + Sync {
    /// Records a health failure (atomic write, like every other state file).
    fn write_health_failure(&self, record: &HealthFailureRecord) -> Result<(), StoreError>;

    /// Removes the temporary files that writers of the state files left when they died
    /// before their rename, and returns how many were removed. Only files of writers that
    /// are gone are touched (`apex_update::store::remove_stale_temp_files`).
    ///
    /// Every write already cleans up after earlier writes of the same file; the daemon also
    /// calls this once at startup, for files that are rarely written (the denylist).
    fn remove_stale_temp_files(&self) -> usize {
        0
    }
}

impl Store for StateDir {
    fn write_health_failure(&self, record: &HealthFailureRecord) -> Result<(), StoreError> {
        write_json_atomic(&self.file(HEALTH_FAILURE_FILE), record)
    }

    fn remove_stale_temp_files(&self) -> usize {
        STATE_FILES
            .iter()
            .map(|name| apex_update::store::remove_stale_temp_files(&self.file(name)))
            .sum()
    }
}

/// `result` of a state-file write or removal, with `StoreError::NotFlushed` (the change is
/// made and visible, only the directory flush failed, so it may not survive a power cut)
/// logged as a warning and counted as done. Every other error is returned as it is.
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

    /// A pid that cannot belong to a running process: above the kernel's `PID_MAX_LIMIT`
    /// (4194304).
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
