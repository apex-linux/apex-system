//! The persisted daemon state in `/var/lib/apex` (contract §4): atomic JSON files.
//!
//! This is the only module in the crate that does I/O, and it uses only `std::fs`.
//!
//! - A missing file reads as its default (no intent, an empty denylist, never checked,
//!   no health failure).
//! - A file that exists but is not valid JSON of the right shape is
//!   [`StoreError::Corrupt`], so the daemon can report it instead of silently forgetting
//!   a denied digest.
//! - Writes are atomic: a temporary file in the same directory is written, flushed with
//!   `fsync`, renamed over the target, and the directory is flushed with `fsync`, so a
//!   power cut leaves either the old or the new file, never a torn one.
//! - A failure after the rename (flushing the directory) is [`StoreError::NotFlushed`],
//!   not a failed write: the new content is already what readers see
//!   ([`StoreError::is_applied`]).
//! - Temporary files left by a writer that died before its rename are removed before the
//!   next write of the same file ([`remove_stale_temp_files`]).
//!
//! The directory itself (0700, root) is created by systemd-tmpfiles, not here.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::records::{Denylist, HealthFailureRecord, Intent, LastCheck};

/// The state directory on an installed system.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/apex";
/// The user's pending apply.
pub const INTENT_FILE: &str = "intent.json";
/// Digests that failed their health checks.
pub const DENYLIST_FILE: &str = "denylist.json";
/// The last remote check.
pub const LAST_CHECK_FILE: &str = "last-check.json";
/// Written by `apex-boot-assess` after a red, armed boot during a user session.
pub const HEALTH_FAILURE_FILE: &str = "health-failure.json";
/// Files larger than this are refused rather than read into memory. The largest file,
/// the denylist, holds roughly 150 bytes per failed build.
pub const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// Why a state file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A filesystem operation failed.
    #[error("cannot {action} {}", .path.display())]
    Io {
        /// What was being done, e.g. `open` or `rename`.
        action: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file exists but is not valid JSON of the expected shape.
    #[error("{} is corrupt", .path.display())]
    Corrupt {
        /// The corrupt file.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: serde_json::Error,
    },
    /// The file is larger than [`MAX_FILE_SIZE`].
    #[error("{} is larger than {limit} bytes", .path.display())]
    TooLarge {
        /// The oversized file.
        path: PathBuf,
        /// The limit.
        limit: u64,
    },
    /// The value could not be encoded as JSON.
    #[error("cannot encode {}", .path.display())]
    Encode {
        /// The file that was to be written.
        path: PathBuf,
        /// The encoding error.
        #[source]
        source: serde_json::Error,
    },
    /// The file was written (or removed), but flushing its directory failed afterwards,
    /// so the change may not survive a power cut. Readers already see it: this is not a
    /// failed write ([`StoreError::is_applied`]).
    #[error(
        "{} was changed, but its directory could not be flushed, so the change may not survive a power cut",
        .path.display()
    )]
    NotFlushed {
        /// The file that was written or removed.
        path: PathBuf,
        /// The error from flushing the directory.
        #[source]
        source: io::Error,
    },
}

impl StoreError {
    /// Whether the change was made despite the error, so the caller must not act as if
    /// the file still held its old content. Only [`StoreError::NotFlushed`].
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

/// Reads and parses a JSON file. A missing file is `Ok(None)`.
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

/// Distinguishes temporary files of one process; with the pid it makes names unique.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many temporary names to try before giving up (a name can collide only with a
/// file left by a crashed process that had the same pid).
const TEMP_ATTEMPTS: u32 = 16;

/// Removes the temporary file unless the rename consumed it.
struct TempFile {
    path: Option<PathBuf>,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // Best effort: the write already failed and that error is what matters.
            let _ = fs::remove_file(path);
        }
    }
}

/// The directory holding `path`, `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// Flushes the directory holding `path` after `path` was renamed into it or removed.
fn sync_dir_of(path: &Path) -> Result<(), StoreError> {
    File::open(parent_dir(path))
        .and_then(|d| d.sync_all())
        .map_err(|source| StoreError::NotFlushed {
            path: path.to_path_buf(),
            source,
        })
}

/// The prefix of the temporary files of `name`: `.<name>.tmp-`, followed by
/// `<pid>-<counter>`.
fn temp_prefix(name: &str) -> String {
    format!(".{name}.tmp-")
}

/// Where the kernel lists running processes.
const PROC: &str = "/proc";

/// Removes the temporary files that interrupted [`write_json_atomic`] calls for `path`
/// left next to it, `.<name>.tmp-<pid>-<n>`, and returns how many were removed.
///
/// Only files whose writer is gone are removed: files with this process's pid are kept
/// (another thread may be writing), and so are files whose pid still appears in `/proc`.
/// A reused pid only delays the cleanup. Without a usable `/proc`, nothing is removed.
/// Best effort: entries that cannot be read or removed are skipped.
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

/// The pid in the `<pid>-<counter>` suffix of a temporary file name.
fn temp_pid(suffix: &str) -> Option<u32> {
    let (pid, counter) = suffix.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(counter) || !digits(pid) {
        return None;
    }
    pid.parse().ok()
}

/// Creates a new, empty temporary file (mode 0600) next to the target.
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

/// Writes `value` as pretty JSON to `path` atomically: temporary file in the same
/// directory (mode 0600), write, `fsync`, rename, `fsync` of the directory.
///
/// Temporary files of `path` left by dead writers are removed first
/// ([`remove_stale_temp_files`]). If only the final directory `fsync` fails, the error
/// is [`StoreError::NotFlushed`]: `path` already holds `value`.
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
    let mut guard = TempFile {
        path: Some(tmp.clone()),
    };
    file.write_all(&bytes).map_err(io_error("write", &tmp))?;
    file.sync_all().map_err(io_error("flush", &tmp))?;
    drop(file);
    fs::rename(&tmp, path).map_err(io_error("rename", path))?;
    guard.path = None;
    sync_dir_of(path)
}

/// Removes `path` durably (the directory is flushed). A missing file is not an error.
/// If only the directory `fsync` fails, the error is [`StoreError::NotFlushed`]: the file
/// is already gone.
pub fn remove_durable(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir_of(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("remove", path)(e)),
    }
}

/// Access to the persisted state; the daemon's I/O boundary for it.
///
/// [`StateDir`] is the real implementation and [`MemoryStore`] a fake for tests.
pub trait StateStore {
    /// The pending apply, if any.
    fn read_intent(&self) -> Result<Option<Intent>, StoreError>;
    /// Records the user's apply.
    fn write_intent(&self, intent: &Intent) -> Result<(), StoreError>;
    /// Forgets the pending apply.
    fn clear_intent(&self) -> Result<(), StoreError>;
    /// The denylist; empty when there is none.
    fn read_denylist(&self) -> Result<Denylist, StoreError>;
    /// Replaces the denylist.
    fn write_denylist(&self, denylist: &Denylist) -> Result<(), StoreError>;
    /// The last check; [`LastCheck::default`] when none ran.
    fn read_last_check(&self) -> Result<LastCheck, StoreError>;
    /// Records a check.
    fn write_last_check(&self, check: &LastCheck) -> Result<(), StoreError>;
    /// The unacknowledged health failure, if any.
    fn read_health_failure(&self) -> Result<Option<HealthFailureRecord>, StoreError>;
    /// Forgets the health failure (`AcknowledgeHealthFailure`, or a green boot).
    fn clear_health_failure(&self) -> Result<(), StoreError>;
}

/// The state files in one directory, normally [`DEFAULT_STATE_DIR`].
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
    /// The state files in `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of one state file in the directory.
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

/// Everything a [`MemoryStore`] holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryState {
    /// `intent.json`.
    pub intent: Option<Intent>,
    /// `denylist.json`.
    pub denylist: Denylist,
    /// `last-check.json`.
    pub last_check: LastCheck,
    /// `health-failure.json`.
    pub health_failure: Option<HealthFailureRecord>,
}

/// An in-memory [`StateStore`] for tests of code that uses the store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<MemoryState>,
}

impl MemoryStore {
    /// A store holding `state`.
    pub fn new(state: MemoryState) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    /// A copy of the current contents.
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

    /// Pre-creates the next `n` temporary names this process would try.
    fn occupy_next_temp_names(dir: &Path, name: &str, n: u64) {
        let pid = std::process::id();
        let next = TEMP_COUNTER.load(Ordering::Relaxed);
        for i in next..next + n {
            fs::write(dir.join(format!(".{name}.tmp-{pid}-{i}")), b"stale").expect("occupy");
        }
    }

    /// The only unit test that writes files, so nothing else moves `TEMP_COUNTER`.
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
