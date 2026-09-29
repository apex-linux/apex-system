//! One test function writes every fake script before anything runs: a test thread forking while
//! a script is open for writing would make its exec fail with ETXTBSY.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use apex_boot::GrubEnv;
use apex_boot::emergency::{
    self, Host, Outcome, REBOOT_TIMER, Schedule, Stay, reboot_message, schedule_args,
};

const TARGET: &str = "sha256:4444444444444444444444444444444444444444444444444444444444444444";

/// Appends its argv, one argument per line and then `--end--`, to `<path>.log`.
fn write_fake(dir: &Path, name: &str, stderr: &str, code: Option<i32>) -> PathBuf {
    let path = dir.join(name);
    let log = dir.join(format!("{name}.log"));
    let tail = match code {
        Some(code) => format!("exit {code}"),
        None => "exec sleep 30".to_owned(),
    };
    let script = format!(
        r#"#!/bin/sh
[ "$PATH" = "/usr/bin:/usr/sbin" ] || {{ echo "bad PATH $PATH" >&2; exit 97; }}
[ "$LANG" = "C.UTF-8" ] || {{ echo "bad LANG" >&2; exit 97; }}
[ -z "${{HOME+x}}" ] || {{ echo "environment not cleared" >&2; exit 98; }}
for arg in "$@"; do printf '%s\n' "$arg" >> '{log}'; done
echo --end-- >> '{log}'
printf '%s' '{stderr}' >&2
{tail}
"#,
        log = log.display(),
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(program: &Path) -> Vec<Vec<String>> {
    let log = PathBuf::from(format!("{}.log", program.display()));
    let Ok(text) = fs::read_to_string(&log) else {
        return Vec::new();
    };
    fs::remove_file(&log).unwrap();
    let mut calls = Vec::new();
    let mut current = Vec::new();
    for line in text.lines() {
        if line == "--end--" {
            calls.push(std::mem::take(&mut current));
        } else {
            current.push(line.to_owned());
        }
    }
    calls
}

fn grubenv(vars: &[(&str, &str)]) -> Vec<u8> {
    let mut env = GrubEnv::new();
    for (name, value) in vars {
        env.set(name, value).unwrap();
    }
    env.to_bytes()
}

struct Fakes {
    run_ok: PathBuf,
    run_taken: PathBuf,
    run_hangs: PathBuf,
    ctl_active: PathBuf,
    ctl_inactive: PathBuf,
    head_hangs: PathBuf,
}

fn watch(host: &Host, dry_run: bool) -> (Outcome, String, Duration) {
    let mut console = Vec::new();
    let started = Instant::now();
    let outcome = emergency::run(host, dry_run, &mut console);
    (
        outcome,
        String::from_utf8(console).unwrap(),
        started.elapsed(),
    )
}

#[test]
fn the_watchdog_against_fake_programs() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let fakes = Fakes {
        run_ok: write_fake(d, "systemd-run-ok", "", Some(0)),
        run_taken: write_fake(
            d,
            "systemd-run-taken",
            "Failed to start transient timer unit: Unit apex-emergency-reboot.timer was \
             already loaded or has a fragment file.\n",
            Some(1),
        ),
        run_hangs: write_fake(d, "systemd-run-hangs", "", None),
        ctl_active: write_fake(d, "systemctl-active", "", Some(0)),
        ctl_inactive: write_fake(d, "systemctl-inactive", "", Some(3)),
        head_hangs: write_fake(d, "head-hangs", "", None),
    };
    let base = Host {
        grubenv: d.join("grubenv"),
        initrd_release: d.join("initrd-release"),
        systemd_run: fakes.run_ok.clone(),
        systemctl: fakes.ctl_inactive.clone(),
        read_timeout: Duration::from_secs(10),
        schedule_timeout: Duration::from_secs(10),
        query_timeout: Duration::from_secs(10),
        ..Host::system()
    };
    assert!(base.head.exists(), "{:?}", base.head);

    scheduling(&fakes, &base);
    failing_to_schedule(&fakes, &base);
    leaving_the_shell_alone(&fakes, &base, d);
    unreadable_grubenv(&fakes, &base);
}

fn scheduling(fakes: &Fakes, base: &Host) {
    let env_path = &base.grubenv;
    fs::write(
        env_path,
        grubenv(&[
            ("apex_target", TARGET),
            ("apex_counter", "2"),
            ("apex_ok", "0"),
        ]),
    )
    .unwrap();
    let (outcome, console, _) = watch(base, false);
    assert_eq!(
        outcome,
        Outcome::RebootScheduled {
            remaining: 2,
            schedule: Schedule::Created
        }
    );
    assert_eq!(
        console.lines().collect::<Vec<_>>(),
        reboot_message(2, Schedule::Created)
    );
    assert_eq!(calls(&fakes.run_ok), [schedule_args()]);
    assert!(calls(&fakes.ctl_inactive).is_empty());

    let (outcome, console, _) = watch(base, true);
    assert_eq!(outcome, Outcome::WouldReboot { remaining: 2 });
    assert!(
        console.contains(&format!(
            "Apex (dry run): would run: {} --quiet --no-ask-password --collect \
             --unit=apex-emergency-reboot",
            fakes.run_ok.display()
        )),
        "{console}"
    );
    assert!(
        console.ends_with(" -- /usr/bin/systemctl reboot\n"),
        "{console}"
    );
    assert!(calls(&fakes.run_ok).is_empty());

    let host = Host {
        systemd_run: fakes.run_taken.clone(),
        systemctl: fakes.ctl_active.clone(),
        ..base.clone()
    };
    let (outcome, console, _) = watch(&host, false);
    assert_eq!(
        outcome,
        Outcome::RebootScheduled {
            remaining: 2,
            schedule: Schedule::AlreadyPending
        }
    );
    assert_eq!(
        console.lines().collect::<Vec<_>>(),
        reboot_message(2, Schedule::AlreadyPending)
    );
    assert_eq!(calls(&fakes.run_taken), [schedule_args()]);
    assert_eq!(
        calls(&fakes.ctl_active),
        [["--quiet", "is-active", "--", REBOOT_TIMER]]
    );
}

fn failing_to_schedule(fakes: &Fakes, base: &Host) {
    let host = Host {
        systemd_run: fakes.run_taken.clone(),
        ..base.clone()
    };
    let (outcome, console, _) = watch(&host, false);
    let Outcome::ScheduleFailed {
        remaining: 2,
        error,
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert!(error.starts_with("systemd-run failed ("), "{error}");
    assert!(error.contains("was already loaded"), "{error}");
    assert!(!outcome.succeeded());
    assert!(
        console.contains("Apex: cannot schedule the automatic reboot: systemd-run failed"),
        "{console}"
    );
    assert!(console.contains("restart the machine"), "{console}");
    assert_eq!(calls(&fakes.run_taken).len(), 1);
    assert_eq!(calls(&fakes.ctl_inactive).len(), 1);

    let host = Host {
        systemd_run: fakes.run_hangs.clone(),
        schedule_timeout: Duration::from_millis(300),
        ..base.clone()
    };
    let (outcome, _, took) = watch(&host, false);
    let Outcome::ScheduleFailed { error, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(error.contains("did not finish within"), "{error}");
    assert!(took < Duration::from_secs(8), "took {took:?}");
    assert_eq!(calls(&fakes.run_hangs).len(), 1);
    assert_eq!(calls(&fakes.ctl_inactive).len(), 1);
}

fn leaving_the_shell_alone(fakes: &Fakes, base: &Host, d: &Path) {
    let env_path = &base.grubenv;
    for (vars, reason) in [
        (&[("apex_ok", "0")][..], Stay::NotArmed),
        (
            &[
                ("apex_target", TARGET),
                ("apex_counter", "3"),
                ("apex_ok", "0"),
            ][..],
            Stay::NotCounted,
        ),
        (
            &[
                ("apex_target", TARGET),
                ("apex_counter", "1"),
                ("apex_ok", "1"),
            ][..],
            Stay::Confirmed,
        ),
        (
            &[
                ("apex_target", TARGET),
                ("apex_counter", "-1"),
                ("apex_ok", "0"),
            ][..],
            Stay::FellBack,
        ),
    ] {
        fs::write(env_path, grubenv(vars)).unwrap();
        let (outcome, console, _) = watch(base, false);
        assert_eq!(outcome, Outcome::Stay(reason), "{vars:?}");
        assert_eq!(console, "", "{vars:?}");
        let (_, console, _) = watch(base, true);
        assert_eq!(console, format!("Apex (dry run): no reboot: {reason}\n"));
    }
    assert!(calls(&fakes.run_ok).is_empty());

    fs::write(d.join("initrd-release"), "").unwrap();
    let host = Host {
        head: d.join("missing-head"),
        ..base.clone()
    };
    let (outcome, console, _) = watch(&host, false);
    assert_eq!(outcome, Outcome::Initrd);
    assert_eq!(console, "");
    fs::remove_file(d.join("initrd-release")).unwrap();
    assert!(calls(&fakes.run_ok).is_empty());
}

fn unreadable_grubenv(fakes: &Fakes, base: &Host) {
    let env_path = &base.grubenv;
    fs::remove_file(env_path).unwrap();
    let (outcome, console, _) = watch(base, false);
    let Outcome::Unreadable(error) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(error.starts_with("head failed ("), "{error}");
    assert!(
        console.starts_with("Apex: cannot read the boot counter, so no automatic reboot: head"),
        "{console}"
    );
    assert_eq!(console.lines().count(), 1);
    for (bytes, expected) in [
        (
            b"# GRUB Environment Block\nno equals sign\n".to_vec(),
            "no '='",
        ),
        (
            grubenv(&[("apex_target", TARGET), ("apex_counter", "02")]),
            "not a canonical integer",
        ),
        (
            grubenv(&[("apex_target", "latest"), ("apex_counter", "2")]),
            "not a valid digest",
        ),
        (
            [
                b"# GRUB Environment Block\n".as_slice(),
                &vec![b'#'; emergency::GRUBENV_READ_LIMIT],
            ]
            .concat(),
            "larger than 65536 bytes",
        ),
    ] {
        fs::write(env_path, &bytes).unwrap();
        let (outcome, _, _) = watch(base, false);
        let Outcome::Unreadable(error) = &outcome else {
            panic!("{outcome:?}");
        };
        assert!(error.contains(expected), "{error}");
    }
    assert!(calls(&fakes.run_ok).is_empty());

    let host = Host {
        head: fakes.head_hangs.clone(),
        read_timeout: Duration::from_millis(300),
        ..base.clone()
    };
    let (outcome, _, took) = watch(&host, false);
    let Outcome::Unreadable(error) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(error.contains("did not finish within"), "{error}");
    assert!(took < Duration::from_secs(8), "took {took:?}");
    let head_calls = calls(&fakes.head_hangs);
    assert_eq!(
        head_calls,
        [["-c", "65537", "--", env_path.to_str().unwrap()]]
    );
    assert!(calls(&fakes.run_ok).is_empty());
}
