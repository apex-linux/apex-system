use std::os::unix::fs::PermissionsExt as _;
use std::time::Instant as StdInstant;

use proptest::prelude::*;

use super::*;
use crate::client::Bootc;
use crate::command::ENVIRONMENT;
use crate::error::ErrorKind;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const TEST_GRACE: Duration = Duration::from_millis(200);

fn contract_env() -> Vec<(OsString, OsString)> {
    ENVIRONMENT
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

fn sh(script: &str, positional: &[&str]) -> ProcessSpec {
    let mut args: Vec<OsString> = vec!["-c".into(), script.into(), "sh".into()];
    args.extend(positional.iter().map(OsString::from));
    ProcessSpec {
        program: PathBuf::from("/bin/sh"),
        args,
        env: contract_env(),
        timeout: TEST_TIMEOUT,
        kill_grace: TEST_GRACE,
        progress_fd: false,
        output_limit: OUTPUT_LIMIT,
    }
}

/// The state letter follows the parenthesised command name in `/proc/<pid>/stat`.
fn proc_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.trim_start().chars().next()
}

/// A zombie is dead too: only its new parent has not reaped it yet.
fn assert_dies(pid: i32) {
    let start = StdInstant::now();
    while start.elapsed() < Duration::from_secs(5) {
        match proc_state(pid) {
            None | Some('Z' | 'X') => return,
            Some(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    panic!("process {pid} survived the process-group kill");
}

fn read_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn captures_stdout_stderr_and_exit_status() {
    let out = run_process(
        &sh("echo out; echo err >&2; exit 3", &[]),
        None,
        &CancelToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(out.exit, ExitState::Code(3));
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
    assert!(!out.stdout_truncated && !out.stderr_truncated);

    let out = run_process(&sh("kill -9 $$", &[]), None, &CancelToken::new())
        .await
        .unwrap();
    assert_eq!(out.exit, ExitState::Signal(9));
}

#[tokio::test]
async fn environment_is_exactly_the_contract() {
    let spec = ProcessSpec {
        program: PathBuf::from("/usr/bin/env"),
        args: Vec::new(),
        ..sh("", &[])
    };
    let out = run_process(&spec, None, &CancelToken::new()).await.unwrap();
    let mut lines: Vec<String> = out.stdout_lossy().lines().map(str::to_owned).collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            "CONTAINERS_POLICY_JSON=/usr/share/apex/containers/policy.json",
            "LANG=C.UTF-8",
            "PATH=/usr/bin:/usr/sbin",
        ]
    );
}

#[tokio::test]
async fn child_leads_its_group_in_root_with_null_stdin() {
    let script = r"echo $$; cut -d' ' -f5 /proc/$$/stat; pwd; readlink /proc/$$/fd/0";
    let out = run_process(&sh(script, &[]), None, &CancelToken::new())
        .await
        .unwrap();
    let text = out.stdout_lossy();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert_eq!(lines[0], lines[1], "pid == pgid");
    assert_eq!(lines[2], "/");
    assert_eq!(lines[3], "/dev/null");
}

#[tokio::test]
async fn output_keeps_the_tail_within_the_limit() {
    let script = "i=0; while [ $i -lt 500 ]; do echo line$i; i=$((i+1)); done";
    let spec = ProcessSpec {
        output_limit: 1000,
        ..sh(script, &[])
    };
    let out = run_process(&spec, None, &CancelToken::new()).await.unwrap();
    assert!(out.stdout_truncated);
    assert_eq!(out.stdout.len(), 1000);
    assert!(out.stdout_lossy().ends_with("line498\nline499\n"));
}

#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let spec = ProcessSpec {
        timeout: Duration::from_secs(1),
        ..sh(
            r#"sleep 1000 & echo $! > "$1"; wait"#,
            &[pidfile.to_str().unwrap()],
        )
    };
    let start = StdInstant::now();
    let err = run_process(&spec, None, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProcessError::TimedOut), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(6));
    assert_dies(read_pid(&pidfile));
}

#[tokio::test]
async fn term_ignoring_group_is_killed_after_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    // SIG_IGN survives exec, so both the shell and its `sleep` ignore SIGTERM.
    let spec = ProcessSpec {
        timeout: Duration::from_secs(1),
        ..sh(
            r#"trap '' TERM; sleep 1000 & echo $! > "$1"; wait"#,
            &[pidfile.to_str().unwrap()],
        )
    };
    let start = StdInstant::now();
    let err = run_process(&spec, None, &CancelToken::new())
        .await
        .unwrap_err();
    let elapsed = start.elapsed();
    assert!(matches!(err, ProcessError::TimedOut), "{err:?}");
    assert!(
        elapsed >= Duration::from_secs(1) + TEST_GRACE,
        "{elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(6), "{elapsed:?}");
    assert_dies(read_pid(&pidfile));
}

#[tokio::test]
async fn stragglers_die_even_when_the_leader_exits_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let spec = ProcessSpec {
        timeout: Duration::from_secs(1),
        ..sh(
            r#"(trap '' TERM; exec sleep 1000) & echo $! > "$1"; wait"#,
            &[pidfile.to_str().unwrap()],
        )
    };
    let err = run_process(&spec, None, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProcessError::TimedOut), "{err:?}");
    assert_dies(read_pid(&pidfile));
}

#[tokio::test]
async fn cancel_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    // Write the pid atomically: the cancel may land right after the file appears.
    let spec = sh(
        r#"sleep 1000 & echo $! > "$1.tmp"; mv "$1.tmp" "$1"; wait"#,
        &[pidfile.to_str().unwrap()],
    );
    let cancel = CancelToken::new();
    let canceller = cancel.clone();
    let watched = pidfile.clone();
    tokio::spawn(async move {
        while !watched.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        canceller.cancel();
    });
    let err = run_process(&spec, None, &cancel).await.unwrap_err();
    assert!(matches!(err, ProcessError::Cancelled), "{err:?}");
    assert_dies(read_pid(&pidfile));
}

#[tokio::test]
async fn dropping_a_run_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let spec = sh(
        r#"sleep 1000 & echo $! > "$1"; wait"#,
        &[pidfile.to_str().unwrap()],
    );
    let cancel = CancelToken::new();
    let run = run_process(&spec, None, &cancel);
    let waited = tokio::time::timeout(Duration::from_secs(1), run).await;
    assert!(waited.is_err(), "the run should still have been going");
    assert_dies(read_pid(&pidfile));
}

#[tokio::test]
async fn cancelled_token_does_not_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = run_process(
        &sh(r#"touch "$1""#, &[marker.to_str().unwrap()]),
        None,
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ProcessError::Cancelled));
    assert!(!marker.exists());
}

#[tokio::test]
async fn missing_program_is_a_spawn_error() {
    let spec = ProcessSpec {
        program: PathBuf::from("/nonexistent/bootc"),
        ..sh("", &[])
    };
    let err = run_process(&spec, None, &CancelToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProcessError::Spawn(e) if e.kind() == io::ErrorKind::NotFound),
        "{err:?}"
    );
}

const START: &str = r#"{"type":"Start","version":"0.1.0"}"#;
const BYTES: &str = r#"{"bytes":3,"bytesCached":0,"bytesTotal":4,"description":"Pulling Image: sha256:ab","id":"sha256:ab","steps":0,"stepsCached":0,"stepsTotal":2,"subtasks":[],"task":"pulling","type":"ProgressBytes"}"#;

#[tokio::test]
async fn progress_is_read_from_fd_3() {
    let spec = ProcessSpec {
        progress_fd: true,
        ..sh(
            r#"readlink /proc/$$/fd/3; printf '%s\n' "$1" "junk" "$2" >&3"#,
            &[START, BYTES],
        )
    };
    let (tx, mut rx) = mpsc::channel(8);
    let out = run_process(&spec, Some(tx), &CancelToken::new())
        .await
        .unwrap();
    assert!(
        out.stdout_lossy().starts_with("pipe:"),
        "{}",
        out.stdout_lossy()
    );
    let update = rx.recv().await.unwrap();
    assert_eq!(update.fraction, Some(0.75));
    assert!(
        rx.recv().await.is_none(),
        "exactly one update, then the sender is dropped"
    );
}

#[tokio::test]
async fn a_full_or_closed_sink_never_blocks_bootc() {
    // Far more than a pipe buffer holds, so bootc would block if draining stopped.
    let script = r#"i=0; while [ $i -lt 20000 ]; do printf '%s\n' "$1" >&3; i=$((i+1)); done"#;
    let spec = ProcessSpec {
        progress_fd: true,
        timeout: Duration::from_secs(60),
        ..sh(script, &[BYTES])
    };
    let (tx, rx) = mpsc::channel(1);
    let out = run_process(&spec, Some(tx), &CancelToken::new())
        .await
        .unwrap();
    assert!(out.exit.success());
    drop(rx);
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let out = run_process(&spec, Some(tx), &CancelToken::new())
        .await
        .unwrap();
    assert!(out.exit.success());
}

const FAKE_BOOTC: &str = r#"#!/bin/sh
case "$*" in
  "status --format=json --format-version=1")
    printf '%s' '{"apiVersion":"org.containers.bootc/v1","kind":"BootcHost","spec":{"image":{"image":"ghcr.io/apex-linux/apex:45","transport":"registry"}},"status":{"booted":{"image":{"image":{"image":"ghcr.io/apex-linux/apex:45","transport":"registry"},"version":"45.20260924.117","timestamp":"2026-09-24T03:00:00Z","imageDigest":"sha256:736b359467c9437c1ac915acaae952aad854e07eb4a16a94999a48af08c83c34","architecture":"amd64"},"incompatible":false,"pinned":false,"downloadOnly":false,"ostree":{"checksum":"26836632adf6228d64ef07a26fd3efaf177104efd1f341a2cf7909a3e4e2c72c","deploySerial":0,"stateroot":"default"}},"rollbackQueued":false,"type":"bootcHost"}}'
    ;;
  "upgrade --download-only --progress-fd 3")
    echo "error: unexpected argument '--progress-fd' found" >&2
    exit 2
    ;;
  "upgrade --download-only")
    echo "Image downloaded, but will not be applied on reboot"
    ;;
  "upgrade --check")
    echo "error: Upgrading: pinging container registry ghcr.io: Get \"https://ghcr.io/v2/\": dial tcp: lookup ghcr.io: no such host" >&2
    exit 1
    ;;
  "rollback")
    exec sleep 1000
    ;;
  *)
    echo "error: unexpected argument '$1' found" >&2
    exit 2
    ;;
esac
"#;

fn install_fake_bootc(dir: &Path) -> PathBuf {
    let path = dir.join("bootc");
    std::fs::write(&path, FAKE_BOOTC).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A test forking concurrently can hold our write descriptor until its child execs,
    // making exec fail with ETXTBSY.
    for _ in 0..200 {
        match std::process::Command::new(&path).output() {
            Err(e) if e.raw_os_error() == Some(Errno::ETXTBSY as i32) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => return path,
        }
    }
    panic!("{} stayed busy", path.display());
}

#[tokio::test]
async fn tokio_runner_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let program = install_fake_bootc(dir.path());
    let bootc = Bootc::new(TokioRunner::for_test(
        &program,
        &dir.path().join("lock"),
        None,
    ));
    let cancel = CancelToken::new();

    let host = bootc.status(&cancel).await.unwrap();
    assert!(host.is_bootc_host());
    assert_eq!(
        host.booted().and_then(BootEntry::version),
        Some("45.20260924.117")
    );

    let (tx, _rx) = mpsc::channel(8);
    bootc.download(Some(tx), &cancel).await.unwrap();

    let err = bootc.check(&cancel).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Network, "{err}");

    let err = bootc.apply_downloaded(&cancel).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported, "{err}");
}

use crate::status::BootEntry;

#[tokio::test]
async fn timeout_while_another_process_holds_the_lock_is_busy() {
    use nix::fcntl::{FcntlArg, fcntl};
    use nix::libc;

    let dir = tempfile::tempdir().unwrap();
    let program = install_fake_bootc(dir.path());
    let lock_path = dir.path().join("lock");
    let runner = TokioRunner::for_test(&program, &lock_path, Some(Duration::from_millis(200)));

    let err = runner
        .run(Invocation::new(BootcCommand::Rollback))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Timeout, "{err}");

    let holder = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    let lock = libc::flock {
        l_type: i16::try_from(libc::F_WRLCK).unwrap(),
        l_whence: i16::try_from(libc::SEEK_SET).unwrap(),
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    fcntl(&holder, FcntlArg::F_OFD_SETLK(&lock)).unwrap();
    let err = runner
        .run(Invocation::new(BootcCommand::Rollback))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Busy, "{err}");
}

proptest! {
    #[test]
    fn tail_buffer_keeps_exactly_the_last_bytes(
        chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 0..30),
        limit in 0usize..1000,
    ) {
        let mut tail = TailBuffer::new(limit);
        let mut all = Vec::new();
        for c in &chunks {
            tail.push(c);
            all.extend_from_slice(c);
        }
        let (kept, truncated) = tail.into_parts();
        let start = all.len().saturating_sub(limit);
        prop_assert_eq!(&kept[..], &all[start..]);
        prop_assert_eq!(truncated, all.len() > limit);
    }
}
