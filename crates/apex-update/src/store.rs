use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::records::{Denylist, HealthFailureRecord, Intent, LastCheck};

/// Created 0700 by systemd-tmpfiles, not here.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/apex";
pub const INTENT_FILE: &str = "intent.json";
pub const DENYLIST_FILE: &str = "denylist.json";
pub const LAST_CHECK_FILE: &str = "last-check.json";
pub const HEALTH_FAILURE_FILE: &str = "health-failure.json";
pub const SETTINGS_FILE: &str = "settings.json";
pub const FALLBACK_FILE: &str = "last-fallback.json";
pub const PINNED_FILE: &str = "pinned.json";
/// The largest file, the denylist, holds about 150 bytes per failed build.
pub const MAX_FILE_SIZE: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot {action} {}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Never read as a default, which would silently forget a denied digest.
    #[error("{} is corrupt", .path.display())]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{} is larger than {limit} bytes", .path.display())]
    TooLarge { path: PathBuf, limit: u64 },
    #[error("cannot encode {}", .path.display())]
    Encode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// Not a failed write: readers already see the change.
    #[error(
        "{} was changed, but its directory could not be flushed, so the change may not survive a power cut",
        .path.display()
    )]
    NotFlushed {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl StoreError {
    /// The caller must not act as if the file still held its old content.
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::NotFlushed { .. })
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> StoreError {
    let path = path.to_path_buf();
    move |source| StoreError::Io {
        action,
        path,
        source,
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error("open", path)(e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error("read", path))?;
    if bytes.len() as u64 > MAX_FILE_SIZE {
        return Err(StoreError::TooLarge {
            path: path.to_path_buf(),
            limit: MAX_FILE_SIZE,
        });
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| StoreError::Corrupt {
            path: path.to_path_buf(),
            source,
        })
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A name can collide only with a file left by a crashed process that had the same pid.
const TEMP_ATTEMPTS: u32 = 16;

struct RemoveOnDrop {
    path: Option<PathBuf>,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // Best effort: the write already failed and that error is what matters.
            let _ = fs::remove_file(path);
        }
    }
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

fn sync_dir_of(path: &Path) -> Result<(), StoreError> {
    File::open(parent_dir(path))
        .and_then(|d| d.sync_all())
        .map_err(|source| StoreError::NotFlushed {
            path: path.to_path_buf(),
            source,
        })
}

fn temp_prefix(name: &str) -> String {
    format!(".{name}.tmp-")
}

const PROC: &str = "/proc";

/// Removes only a dead writer's files: this process's own may be in use by another thread,
/// and a reused pid only delays the cleanup.
pub fn remove_stale_temp_files(path: &Path) -> usize {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return 0;
    };
    let proc = Path::new(PROC);
    if !proc.join("self").exists() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(parent_dir(path)) else {
        return 0;
    };
    let prefix = temp_prefix(name);
    let own_pid = std::process::id();
    let mut removed = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name
            .to_str()
            .and_then(|n| n.strip_prefix(prefix.as_str()))
            .and_then(temp_pid)
        else {
            continue;
        };
        if pid == own_pid || proc.join(pid.to_string()).exists() {
            continue;
        }
        if fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn temp_pid(suffix: &str) -> Option<u32> {
    let (pid, counter) = suffix.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(counter) || !digits(pid) {
        return None;
    }
    pid.parse().ok()
}

fn create_temp(dir: &Path, name: &str) -> Result<(File, PathBuf), StoreError> {
    let pid = std::process::id();
    let mut last_error = None;
    for _ in 0..TEMP_ATTEMPTS {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!("{}{pid}-{n}", temp_prefix(name)));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_error = Some((tmp, e)),
            Err(e) => return Err(io_error("create", &tmp)(e)),
        }
    }
    let (tmp, e) = last_error.unwrap_or_else(|| {
        (
            dir.to_path_buf(),
            io::Error::from(io::ErrorKind::AlreadyExists),
        )
    });
    Err(io_error("create", &tmp)(e))
}

/// A power cut leaves either the old or the new file, never a torn one.
pub fn write_json_atomic<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), StoreError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| StoreError::Encode {
        path: path.to_path_buf(),
        source,
    })?;
    bytes.push(b'\n');

    let dir = parent_dir(path);
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        io_error("write", path)(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a file name",
        ))
    })?;
    remove_stale_temp_files(path);
    let (mut file, tmp) = create_temp(dir, name)?;
    let mut guard = RemoveOnDrop {
        path: Some(tmp.clone()),
    };
    file.write_all(&bytes).map_err(io_error("write", &tmp))?;
    file.sync_all().map_err(io_error("flush", &tmp))?;
    drop(file);
    fs::rename(&tmp, path).map_err(io_error("rename", path))?;
    guard.path = None;
    sync_dir_of(path)
}

pub fn remove_durable(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir_of(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("remove", path)(e)),
    }
}

pub trait StateStore {
    fn read_intent(&self) -> Result<Option<Intent>, StoreError>;
    fn write_intent(&self, intent: &Intent) -> Result<(), StoreError>;
    fn clear_intent(&self) -> Result<(), StoreError>;
    fn read_denylist(&self) -> Result<Denylist, StoreError>;
    fn write_denylist(&self, denylist: &Denylist) -> Result<(), StoreError>;
    fn read_last_check(&self) -> Result<LastCheck, StoreError>;
    fn write_last_check(&self, check: &LastCheck) -> Result<(), StoreError>;
    fn read_health_failure(&self) -> Result<Option<HealthFailureRecord>, StoreError>;
    fn clear_health_failure(&self) -> Result<(), StoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    root: PathBuf,
}

impl Default for StateDir {
    fn default() -> Self {
        Self::new(DEFAULT_STATE_DIR)
    }
}

impl StateDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl StateStore for StateDir {
    fn read_intent(&self) -> Result<Option<Intent>, StoreError> {
        read_json(&self.file(INTENT_FILE))
    }

    fn write_intent(&self, intent: &Intent) -> Result<(), StoreError> {
        write_json_atomic(&self.file(INTENT_FILE), intent)
    }

    fn clear_intent(&self) -> Result<(), StoreError> {
        remove_durable(&self.file(INTENT_FILE))
    }

    fn read_denylist(&self) -> Result<Denylist, StoreError> {
        Ok(read_json(&self.file(DENYLIST_FILE))?.unwrap_or_default())
    }

    fn write_denylist(&self, denylist: &Denylist) -> Result<(), StoreError> {
        write_json_atomic(&self.file(DENYLIST_FILE), denylist)
    }

    fn read_last_check(&self) -> Result<LastCheck, StoreError> {
        Ok(read_json(&self.file(LAST_CHECK_FILE))?.unwrap_or_default())
    }

    fn write_last_check(&self, check: &LastCheck) -> Result<(), StoreError> {
        write_json_atomic(&self.file(LAST_CHECK_FILE), check)
    }

    fn read_health_failure(&self) -> Result<Option<HealthFailureRecord>, StoreError> {
        read_json(&self.file(HEALTH_FAILURE_FILE))
    }

    fn clear_health_failure(&self) -> Result<(), StoreError> {
        remove_durable(&self.file(HEALTH_FAILURE_FILE))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryState {
    pub intent: Option<Intent>,
    pub denylist: Denylist,
    pub last_check: LastCheck,
    pub health_failure: Option<HealthFailureRecord>,
}

#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<MemoryState>,
}

impl MemoryStore {
    pub fn new(state: MemoryState) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    pub fn snapshot(&self) -> MemoryState {
        self.with(|s| s.clone())
    }

    fn with<R>(&self, f: impl FnOnce(&mut MemoryState) -> R) -> R {
        // A panic in another test thread must not hide this store's contents.
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}

impl StateStore for MemoryStore {
    fn read_intent(&self) -> Result<Option<Intent>, StoreError> {
        Ok(self.with(|s| s.intent.clone()))
    }

    fn write_intent(&self, intent: &Intent) -> Result<(), StoreError> {
        self.with(|s| s.intent = Some(intent.clone()));
        Ok(())
    }

    fn clear_intent(&self) -> Result<(), StoreError> {
        self.with(|s| s.intent = None);
        Ok(())
    }

    fn read_denylist(&self) -> Result<Denylist, StoreError> {
        Ok(self.with(|s| s.denylist.clone()))
    }

    fn write_denylist(&self, denylist: &Denylist) -> Result<(), StoreError> {
        self.with(|s| s.denylist = denylist.clone());
        Ok(())
    }

    fn read_last_check(&self) -> Result<LastCheck, StoreError> {
        Ok(self.with(|s| s.last_check.clone()))
    }

    fn write_last_check(&self, check: &LastCheck) -> Result<(), StoreError> {
        self.with(|s| s.last_check = check.clone());
        Ok(())
    }

    fn read_health_failure(&self) -> Result<Option<HealthFailureRecord>, StoreError> {
        Ok(self.with(|s| s.health_failure.clone()))
    }

    fn clear_health_failure(&self) -> Result<(), StoreError> {
        self.with(|s| s.health_failure = None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_of_a_bare_name_is_the_current_directory() {
        assert_eq!(parent_dir(Path::new("intent.json")), Path::new("."));
        assert_eq!(
            parent_dir(Path::new("/var/lib/apex/intent.json")),
            Path::new("/var/lib/apex")
        );
    }

    #[test]
    fn temporary_file_names_are_parsed_strictly() {
        assert_eq!(temp_pid("812-0"), Some(812));
        assert_eq!(temp_pid("812-17"), Some(812));
        for bad in [
            "", "-", "812", "812-", "-0", "8a2-0", "812-0x", "+812-0", "812-0-1",
        ] {
            assert_eq!(temp_pid(bad), None, "{bad:?}");
        }
        assert_eq!(temp_pid("99999999999-0"), None, "pid out of range");
        assert_eq!(temp_prefix("intent.json"), ".intent.json.tmp-");
    }

    #[test]
    fn only_flush_failures_count_as_applied() {
        let io = || io::Error::from(io::ErrorKind::PermissionDenied);
        let not_flushed = StoreError::NotFlushed {
            path: "/var/lib/apex/intent.json".into(),
            source: io(),
        };
        assert!(not_flushed.is_applied());
        assert_eq!(
            not_flushed.to_string(),
            "/var/lib/apex/intent.json was changed, but its directory could not be flushed, \
             so the change may not survive a power cut"
        );
        let failed = StoreError::Io {
            action: "rename",
            path: "/var/lib/apex/intent.json".into(),
            source: io(),
        };
        assert!(!failed.is_applied());
    }

    #[test]
    fn default_state_dir_is_var_lib_apex() {
        let dir = StateDir::default();
        assert_eq!(dir.root(), Path::new("/var/lib/apex"));
        assert_eq!(
            dir.file(INTENT_FILE),
            Path::new("/var/lib/apex/intent.json")
        );
    }

    fn occupy_next_temp_names(dir: &Path, name: &str, n: u64) {
        let pid = std::process::id();
        let next = TEMP_COUNTER.load(Ordering::Relaxed);
        for i in next..next + n {
            fs::write(dir.join(format!(".{name}.tmp-{pid}-{i}")), b"stale").expect("occupy");
        }
    }

    // The only unit test that writes files, so nothing else moves `TEMP_COUNTER`.
    #[test]
    fn temporary_name_collisions_retry_then_give_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state.json");

        occupy_next_temp_names(tmp.path(), "state.json", 3);
        write_json_atomic(&path, &1).expect("fourth name is free");
        assert_eq!(read_json::<u8>(&path).expect("read"), Some(1));

        occupy_next_temp_names(tmp.path(), "state.json", u64::from(TEMP_ATTEMPTS));
        let err = write_json_atomic(&path, &2).expect_err("every name taken");
        assert!(
            matches!(&err, StoreError::Io { action: "create", source, .. }
                if source.kind() == io::ErrorKind::AlreadyExists),
            "{err:?}"
        );
        assert_eq!(
            read_json::<u8>(&path).expect("read"),
            Some(1),
            "old file intact"
        );
    }

    #[test]
    fn memory_store_round_trips() {
        let store = MemoryStore::default();
        assert_eq!(store.read_intent().expect("read"), None);
        let intent = Intent {
            digest: "sha256:a".into(),
            created: 7,
        };
        store.write_intent(&intent).expect("write");
        assert_eq!(store.read_intent().expect("read"), Some(intent));
        store.clear_intent().expect("clear");
        assert_eq!(store.snapshot().intent, None);

        let mut list = Denylist::new();
        list.insert(crate::records::DenyEntry {
            digest: "sha256:b".into(),
            reason: "red".into(),
            at: 1,
        });
        store.write_denylist(&list).expect("write");
        assert!(store.read_denylist().expect("read").contains("sha256:b"));

        let check = LastCheck {
            at: 5,
            remote_digest: Some("sha256:c".into()),
            remote_version: None,
            remote_size: None,
        };
        store.write_last_check(&check).expect("write");
        assert_eq!(store.read_last_check().expect("read"), check);

        let health = HealthFailureRecord {
            digest: "sha256:d".into(),
            failed_checks: vec!["session".into()],
            at: 9,
        };
        let store = MemoryStore::new(MemoryState {
            health_failure: Some(health.clone()),
            ..MemoryState::default()
        });
        assert_eq!(store.read_health_failure().expect("read"), Some(health));
        store.clear_health_failure().expect("clear");
        assert_eq!(store.read_health_failure().expect("read"), None);
    }
}
