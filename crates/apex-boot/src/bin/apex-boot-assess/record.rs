//! `health-failure.json` on disk. The write is bounded by [`RECORD_TIMEOUT`]: on a stalled
//! `/var` it is abandoned on its thread, so the report and the reboot after it still happen.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use apex_boot::{HealthFailure, RECORD_TIMEOUT};

use crate::ports::FailureStore;

#[derive(Debug, Clone)]
pub(crate) struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl FailureStore for FileStore {
    fn record(&self, failure: &HealthFailure) -> Result<(), String> {
        let mut json = serde_json::to_vec_pretty(failure)
            .map_err(|err| format!("cannot encode the health failure: {err}"))?;
        json.push(b'\n');
        let path = self.path.clone();
        run_bounded(RECORD_TIMEOUT, move || write_atomic(&path, &json))
            .and_then(|written| written.map_err(|err| err.to_string()))
            .map_err(|err| format!("cannot write {}: {err}", self.path.display()))
    }
}

/// A thread that does not finish in time is left to finish, or hang, on its own.
pub(crate) fn run_bounded<T: Send + 'static>(
    timeout: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (done, result) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("apex-record".to_owned())
        .spawn(move || {
            let _ = done.send(work());
        })
        .map_err(|err| format!("cannot start a thread: {err}"))?;
    result
        .recv_timeout(timeout)
        .map_err(|_| format!("did not finish within {} s", timeout.as_secs()))
}

/// A crash or power loss leaves the old record or the new one, never a torn file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path has no file name",
        ));
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = dir.join(temp_name);
    // A leftover from an earlier run with the same pid would make `create_new` fail.
    match fs::remove_file(&temp) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    // The rename is durable only once the directory entry is.
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_boot::Digest;
    use std::os::unix::fs::PermissionsExt as _;

    fn failure(at: u64) -> HealthFailure {
        HealthFailure::new(
            Digest::parse(&format!("sha256:{}", "ab".repeat(32))).unwrap(),
            vec!["session".to_owned(), "failed-units".to_owned()],
            at,
        )
    }

    #[test]
    fn writes_the_contract_json_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("var/lib/apex/health-failure.json");
        FileStore::new(path.clone())
            .record(&failure(1_790_000_000))
            .unwrap();

        let back: HealthFailure = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back, failure(1_790_000_000));
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["at", "digest", "failed_checks"]);

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn replaces_an_older_record_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health-failure.json");
        let store = FileStore::new(path.clone());
        store.record(&failure(1)).unwrap();
        store.record(&failure(2)).unwrap();
        let back: HealthFailure = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.at, 2);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["health-failure.json"]);
    }

    #[test]
    fn a_stalled_write_is_abandoned_at_the_timeout() {
        let started = std::time::Instant::now();
        let result = run_bounded(Duration::from_millis(50), || {
            thread::sleep(Duration::from_secs(5));
            1
        });
        assert_eq!(result, Err("did not finish within 0 s".to_owned()));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(run_bounded(Duration::from_secs(5), || 7), Ok(7));
    }

    #[test]
    fn a_failed_write_is_reported_and_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health-failure.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "x").unwrap();
        let err = FileStore::new(path.clone())
            .record(&failure(1))
            .unwrap_err();
        assert!(
            err.starts_with(&format!("cannot write {}", path.display())),
            "{err}"
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["health-failure.json"]);
    }
}
