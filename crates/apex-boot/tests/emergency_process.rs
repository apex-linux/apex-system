//! Runs the built `apex-boot-assess` with the flag the drop-ins pass, as a dry run against
//! fixture files below `--root`: exit status, console text and time, and that the watchdog
//! never touches the system bus (the address points at a socket this test listens on).
//!
//! Only dry runs: a real run reads the host's `/boot/grub2/grubenv` and could schedule a
//! real reboot. The scheduling itself is covered by `emergency_watchdog.rs` with fake
//! programs and by `emergency_units.rs` with the host's real `systemd-run`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]

use std::fs;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use apex_boot::GrubEnv;
use apex_boot::emergency::{EMERGENCY_DROP_IN, Schedule, WATCHDOG_TIME_BUDGET, reboot_message};

const BINARY: &str = env!("CARGO_BIN_EXE_apex-boot-assess");
const TARGET: &str = "sha256:5555555555555555555555555555555555555555555555555555555555555555";

/// The arguments the drop-in passes, taken from the installed text.
fn drop_in_args() -> Vec<String> {
    let line = EMERGENCY_DROP_IN
        .lines()
        .find_map(|l| l.strip_prefix("ExecStartPre=-"))
        .unwrap();
    let mut words = line.split_whitespace();
    assert_eq!(words.next(), Some(apex_boot::ASSESS_BINARY_PATH));
    words.map(str::to_owned).collect()
}

/// A `--root` with `boot/grub2/grubenv` holding `vars` (none: no grubenv at all).
fn root_with(vars: Option<&[(&str, &str)]>) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    if let Some(vars) = vars {
        let mut env = GrubEnv::new();
        for (name, value) in vars {
            env.set(name, value).unwrap();
        }
        let grub = root.path().join("boot/grub2");
        fs::create_dir_all(&grub).unwrap();
        fs::write(grub.join("grubenv"), env.to_bytes()).unwrap();
    }
    root
}

/// Runs the binary as the drop-in does, plus `--dry-run --root ROOT`, with the system bus
/// address pointing at a listener no connection may reach.
fn dry_run(root: &Path) -> (Output, Duration) {
    let bus_name = format!("apex-boot-emergency-test-{}", std::process::id());
    let listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(bus_name.as_bytes()).unwrap())
            .unwrap();
    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();
    let output = Command::new(BINARY)
        .args(drop_in_args())
        .arg("--dry-run")
        .arg("--root")
        .arg(root)
        .env_clear()
        .env("PATH", "/usr/bin:/usr/sbin:/bin")
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:abstract={bus_name}"),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let took = started.elapsed();
    match listener.accept() {
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("the watchdog connected to the bus: {other:?}"),
    }
    (output, took)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn the_drop_in_command_dry_runs_every_case() {
    assert_eq!(drop_in_args(), ["--emergency-watchdog"]);

    // A counted boot of an armed update: the announcement, then the command it would run.
    let root = root_with(Some(&[
        ("apex_target", TARGET),
        ("apex_counter", "1"),
        ("apex_ok", "0"),
    ]));
    let (output, took) = dry_run(root.path());
    let stdout = text(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert_eq!(text(&output.stderr), "", "nothing on stderr");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[..4], reboot_message(1, Schedule::Created));
    assert_eq!(lines.len(), 5, "{stdout}");
    assert!(
        lines[4].starts_with(
            "Apex (dry run): would run: /usr/bin/systemd-run --quiet --no-ask-password \
             --collect --unit=apex-emergency-reboot "
        ),
        "{stdout}"
    );
    assert!(
        lines[4].contains(
            "'--property=ExecCondition=/usr/bin/systemctl --quiet is-active \
             emergency.service rescue.service'"
        ),
        "{stdout}"
    );
    assert!(took < WATCHDOG_TIME_BUDGET, "took {took:?}");

    // The GRUB fixture is unchanged: the watchdog only reads.
    let env = fs::read(root.path().join("boot/grub2/grubenv")).unwrap();
    let vars = apex_boot::BootVars::from_env(&GrubEnv::parse(&env).unwrap()).unwrap();
    assert_eq!(vars.apex_counter, Some(1));

    // Not armed, not counted, exhausted: exit 0 and only the dry run's reason.
    for (vars, reason) in [
        (&[("apex_ok", "0")][..], "no update is on trial"),
        (
            &[
                ("apex_target", TARGET),
                ("apex_counter", "3"),
                ("apex_ok", "0"),
            ][..],
            "GRUB did not count this boot",
        ),
        (
            &[
                ("apex_target", TARGET),
                ("apex_counter", "-1"),
                ("apex_ok", "0"),
            ][..],
            "GRUB already fell back",
        ),
    ] {
        let root = root_with(Some(vars));
        let (output, _) = dry_run(root.path());
        let stdout = text(&output.stdout);
        assert_eq!(output.status.code(), Some(0), "{stdout}");
        assert!(
            stdout.starts_with("Apex (dry run): no reboot: ") && stdout.contains(reason),
            "{stdout}"
        );
        assert_eq!(stdout.lines().count(), 1, "{stdout}");
    }

    // No grubenv (/boot not mounted): one line, exit status 1 (the drop-in ignores it).
    let root = root_with(None);
    let (output, _) = dry_run(root.path());
    let stdout = text(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.starts_with("Apex: cannot read the boot counter, so no automatic reboot: "),
        "{stdout}"
    );

    // In an initramfs: nothing, even with an armed, counted grubenv.
    let root = root_with(Some(&[("apex_target", TARGET), ("apex_counter", "2")]));
    fs::create_dir_all(root.path().join("etc")).unwrap();
    fs::write(root.path().join("etc/initrd-release"), "").unwrap();
    let (output, _) = dry_run(root.path());
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        text(&output.stdout),
        "Apex (dry run): no reboot: this is an initramfs\n"
    );
}
