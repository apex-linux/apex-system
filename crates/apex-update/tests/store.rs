#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use apex_update::store::{
    DENYLIST_FILE, HEALTH_FAILURE_FILE, INTENT_FILE, LAST_CHECK_FILE, MAX_FILE_SIZE, read_json,
    remove_durable, remove_stale_temp_files, write_json_atomic,
};
use apex_update::{
    DenyEntry, Denylist, HealthFailureRecord, Intent, LastCheck, StateDir, StateStore, StoreError,
};

fn state_dir() -> (tempfile::TempDir, StateDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = StateDir::new(tmp.path());
    (tmp, dir)
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn intent() -> Intent {
    Intent {
        digest: "sha256:0123".into(),
        created: 1_790_000_000,
    }
}

#[test]
fn missing_files_read_as_defaults() {
    let (_tmp, dir) = state_dir();
    assert_eq!(dir.read_intent().expect("intent"), None);
    assert!(dir.read_denylist().expect("denylist").is_empty());
    assert_eq!(
        dir.read_last_check().expect("last check"),
        LastCheck::default()
    );
    assert_eq!(dir.read_health_failure().expect("health"), None);
}

#[test]
fn missing_directory_reads_as_defaults_but_cannot_be_written() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = StateDir::new(tmp.path().join("absent"));
    assert_eq!(dir.read_intent().expect("intent"), None);
    let err = dir.write_intent(&intent()).expect_err("no directory");
    assert!(
        matches!(
            err,
            StoreError::Io {
                action: "create",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn intent_round_trips_and_clears() {
    let (tmp, dir) = state_dir();
    dir.write_intent(&intent()).expect("write");
    assert_eq!(dir.read_intent().expect("read"), Some(intent()));
    assert_eq!(
        entries(tmp.path()),
        [INTENT_FILE],
        "no temporary file left behind"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(tmp.path().join(INTENT_FILE)).expect("file"))
            .expect("json");
    assert_eq!(
        json,
        serde_json::json!({ "digest": "sha256:0123", "created": 1_790_000_000 })
    );

    dir.clear_intent().expect("clear");
    assert_eq!(dir.read_intent().expect("read"), None);
    dir.clear_intent().expect("clearing twice is fine");
    assert!(entries(tmp.path()).is_empty());
}

#[test]
fn files_are_private_and_end_with_a_newline() {
    let (tmp, dir) = state_dir();
    dir.write_intent(&intent()).expect("write");
    let path = tmp.path().join(INTENT_FILE);
    let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(fs::read(&path).expect("read").last(), Some(&b'\n'));
}

#[test]
fn overwrite_replaces_the_whole_file() {
    let (tmp, dir) = state_dir();
    let long = Intent {
        digest: format!("sha256:{}", "f".repeat(64)),
        created: 1,
    };
    dir.write_intent(&long).expect("write long");
    dir.write_intent(&intent()).expect("write short");
    assert_eq!(dir.read_intent().expect("read"), Some(intent()));
    assert_eq!(entries(tmp.path()), [INTENT_FILE]);
}

#[test]
fn denylist_round_trips_in_contract_format() {
    let (tmp, dir) = state_dir();
    let mut list = Denylist::new();
    list.insert(DenyEntry {
        digest: "sha256:bb".into(),
        reason: "Required checks failed: session".into(),
        at: 2,
    });
    list.insert(DenyEntry {
        digest: "sha256:aa".into(),
        reason: "GRUB fell back".into(),
        at: 1,
    });
    dir.write_denylist(&list).expect("write");
    assert_eq!(dir.read_denylist().expect("read"), list);
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(tmp.path().join(DENYLIST_FILE)).expect("file"))
            .expect("json");
    assert_eq!(json["digests"][0]["digest"], "sha256:aa");
    assert_eq!(json["digests"][1]["at"], 2);
}

#[test]
fn last_check_round_trips_with_nulls() {
    let (tmp, dir) = state_dir();
    let check = LastCheck {
        at: 1_790_000_000,
        remote_digest: None,
        remote_version: None,
        remote_size: None,
    };
    dir.write_last_check(&check).expect("write");
    assert_eq!(dir.read_last_check().expect("read"), check);
    let text = fs::read_to_string(tmp.path().join(LAST_CHECK_FILE)).expect("file");
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(
        json,
        serde_json::json!({
            "at": 1_790_000_000,
            "remote_digest": null,
            "remote_version": null,
            "remote_size": null
        })
    );
}

#[test]
fn health_failure_is_read_and_cleared() {
    let (tmp, dir) = state_dir();
    fs::write(
        tmp.path().join(HEALTH_FAILURE_FILE),
        br#"{ "digest": "sha256:dd", "failed_checks": ["session-active", "apex-updated"], "at": 5, "boot_id": "x" }"#,
    )
    .expect("write");
    assert_eq!(
        dir.read_health_failure().expect("read"),
        Some(HealthFailureRecord {
            digest: "sha256:dd".into(),
            failed_checks: vec!["session-active".into(), "apex-updated".into()],
            at: 5,
        })
    );
    dir.clear_health_failure().expect("clear");
    assert_eq!(dir.read_health_failure().expect("read"), None);
}

#[test]
fn corrupt_files_are_a_distinct_error() {
    let (tmp, dir) = state_dir();
    for (name, bytes) in [
        (INTENT_FILE, &b""[..]),
        (DENYLIST_FILE, &b"{\"digests\": [ {\"digest\": 3} ]}"[..]),
        (LAST_CHECK_FILE, &b"{\"at\": -1}"[..]),
        (HEALTH_FAILURE_FILE, &b"not json"[..]),
    ] {
        fs::write(tmp.path().join(name), bytes).expect("write");
    }
    assert!(matches!(dir.read_intent(), Err(StoreError::Corrupt { .. })));
    assert!(matches!(
        dir.read_denylist(),
        Err(StoreError::Corrupt { .. })
    ));
    assert!(matches!(
        dir.read_last_check(),
        Err(StoreError::Corrupt { .. })
    ));
    let err = dir.read_health_failure().expect_err("corrupt");
    assert!(
        matches!(&err, StoreError::Corrupt { path, .. } if path.ends_with(HEALTH_FAILURE_FILE))
    );
    assert!(
        err.to_string().ends_with("health-failure.json is corrupt"),
        "{err}"
    );
    assert!(std::error::Error::source(&err).is_some());
}

#[test]
fn oversized_files_are_refused() {
    let (tmp, dir) = state_dir();
    let size = usize::try_from(MAX_FILE_SIZE).expect("fits") + 1;
    fs::write(tmp.path().join(DENYLIST_FILE), vec![b' '; size]).expect("write");
    assert!(matches!(
        dir.read_denylist(),
        Err(StoreError::TooLarge {
            limit: MAX_FILE_SIZE,
            ..
        })
    ));
}

#[test]
fn a_directory_in_place_of_a_file_is_an_io_error() {
    let (tmp, dir) = state_dir();
    fs::create_dir(tmp.path().join(INTENT_FILE)).expect("mkdir");
    assert!(matches!(dir.read_intent(), Err(StoreError::Io { .. })));
    let err = dir
        .write_intent(&intent())
        .expect_err("rename over a directory");
    assert!(
        matches!(
            err,
            StoreError::Io {
                action: "rename",
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!err.is_applied(), "the old state is untouched");
    assert!(
        entries(tmp.path()).iter().all(|n| !n.contains(".tmp-")),
        "the temporary file is removed after a failed write: {:?}",
        entries(tmp.path())
    );
    assert!(matches!(
        dir.clear_intent(),
        Err(StoreError::Io {
            action: "remove",
            ..
        })
    ));
}

#[test]
fn generic_helpers_work_on_any_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("custom.json");
    write_json_atomic(&path, &vec![1, 2, 3]).expect("write");
    assert_eq!(
        read_json::<Vec<u8>>(&path).expect("read"),
        Some(vec![1, 2, 3])
    );
    remove_durable(&path).expect("remove");
    assert_eq!(read_json::<Vec<u8>>(&path).expect("read"), None);
    let err = write_json_atomic(Path::new("/"), &1).expect_err("no file name");
    assert!(
        matches!(
            err,
            StoreError::Io {
                action: "write",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn many_writes_leave_exactly_one_file() {
    let (tmp, dir) = state_dir();
    for n in 0..50 {
        dir.write_last_check(&LastCheck {
            at: n,
            remote_digest: Some(format!("sha256:{n}")),
            remote_version: Some("45.20260924.1".into()),
            remote_size: None,
        })
        .expect("write");
    }
    assert_eq!(dir.read_last_check().expect("read").at, 49);
    assert_eq!(entries(tmp.path()), [LAST_CHECK_FILE]);
}

/// Above Linux's largest `pid_max` (2^22), so no running process has it.
const DEAD_PID: u32 = 4_194_305;

#[test]
fn a_write_removes_temporary_files_left_by_dead_writers() {
    let (tmp, dir) = state_dir();
    let mut live = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn a live writer");
    let live_pid = live.id();
    let stale = [
        format!(".{INTENT_FILE}.tmp-{DEAD_PID}-0"),
        format!(".{INTENT_FILE}.tmp-{DEAD_PID}-7"),
    ];
    let kept = [
        format!(".{INTENT_FILE}.tmp-{live_pid}-0"),
        format!(".{INTENT_FILE}.tmp-{}-3", std::process::id()),
        format!(".{DENYLIST_FILE}.tmp-{DEAD_PID}-0"),
        format!(".{INTENT_FILE}.tmp-x{DEAD_PID}-0"),
        format!("{INTENT_FILE}.tmp-{DEAD_PID}-0"),
    ];
    for name in stale.iter().chain(&kept) {
        fs::write(tmp.path().join(name), b"{").expect("leftover");
    }

    dir.write_intent(&intent()).expect("write");
    assert_eq!(dir.read_intent().expect("read"), Some(intent()));
    let mut expected: Vec<String> = kept.to_vec();
    expected.push(INTENT_FILE.to_owned());
    expected.sort();
    assert_eq!(entries(tmp.path()), expected);

    live.kill().expect("kill");
    live.wait().expect("reap");
    assert_eq!(remove_stale_temp_files(&tmp.path().join(INTENT_FILE)), 1);
    assert!(
        !tmp.path().join(&kept[0]).exists(),
        "its writer is gone now"
    );
    assert_eq!(remove_stale_temp_files(&tmp.path().join(INTENT_FILE)), 0);
}

/// Files can still be created and renamed, but the directory cannot be opened to flush it.
/// `false` when the privileges ignore the mode (root).
fn make_write_only(dir: &Path) -> bool {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o300)).expect("chmod");
    fs::File::open(dir).is_err()
}

#[test]
fn a_failed_directory_flush_after_the_rename_is_not_a_failed_write() {
    let (tmp, dir) = state_dir();
    let path = tmp.path().join(INTENT_FILE);
    let restore = || {
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
    };
    if !make_write_only(tmp.path()) {
        restore();
        eprintln!("skipped: directory permissions are not enforced for this user");
        return;
    }
    let written = write_json_atomic(&path, &intent());
    restore();
    let err = written.expect_err("flush fails");
    assert!(
        matches!(&err, StoreError::NotFlushed { path: p, .. } if *p == path),
        "{err:?}"
    );
    assert!(err.is_applied());
    assert_eq!(dir.read_intent().expect("read"), Some(intent()), "in place");
    assert_eq!(entries(tmp.path()), [INTENT_FILE], "no temporary file");

    make_write_only(tmp.path());
    let removed = remove_durable(&path);
    restore();
    let err = removed.expect_err("flush fails");
    assert!(err.is_applied(), "{err:?}");
    assert!(entries(tmp.path()).is_empty(), "the file is gone");
}
