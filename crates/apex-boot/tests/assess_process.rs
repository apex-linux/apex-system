//! Runs the built binary with `bootc` missing, so the booted digest comes from the fake daemon;
//! the unit checks ask this host's real systemd, so the qualification hook fixes the decision.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]
#![allow(
    clippy::unused_self,
    reason = "zbus interface methods take &self; the fakes answer constants"
)]

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use apex_boot::GrubEnv;
use apex_boot::checks::names;
use apex_boot::snippet::SNIPPET;
use apex_dbus::Deployment;
use serde_json::Value;
use zbus::zvariant::OwnedObjectPath;

const BINARY: &str = env!("CARGO_BIN_EXE_apex-boot-assess");
const BOOTED: &str = "sha256:9999999999999999999999999999999999999999999999999999999999999999";
const OTHER: &str = "sha256:1212121212121212121212121212121212121212121212121212121212121212";

/// The fakes answer at once, so a slow run is a hang.
const RUN_LIMIT: Duration = Duration::from_secs(90);

fn unique_name(what: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "apex-boot-assess-test-{what}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn run(args: &[&std::ffi::OsStr], env: &[(&str, &str)]) -> (Output, Duration) {
    let started = Instant::now();
    let mut child = Command::new(BINARY)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/usr/sbin:/bin")
        .env("RUST_LOG", "info")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if started.elapsed() > RUN_LIMIT {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "apex-boot-assess hung; stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    (output, started.elapsed())
}

fn grubenv_with(vars: &[(&str, &str)]) -> Vec<u8> {
    let mut env = GrubEnv::new();
    for (name, value) in vars {
        env.set(name, value).unwrap();
    }
    env.to_bytes()
}

#[test]
fn an_unusable_grubenv_fails_before_any_dbus_call() {
    let dir = tempfile::tempdir().unwrap();
    let bus_name = unique_name("nobus");
    let listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(bus_name.as_bytes()).unwrap())
            .unwrap();
    listener.set_nonblocking(true).unwrap();
    let bus = format!("unix:abstract={bus_name}");

    let corrupt = dir.path().join("corrupt");
    fs::write(&corrupt, b"# GRUB Environment Block\nno equals sign\n").unwrap();
    let missing = dir.path().join("missing");
    let bad_target = dir.path().join("bad-target");
    fs::write(&bad_target, grubenv_with(&[("apex_target", "latest")])).unwrap();
    let bad_counter = dir.path().join("bad-counter");
    fs::write(
        &bad_counter,
        grubenv_with(&[("apex_target", BOOTED), ("apex_counter", "03")]),
    )
    .unwrap();

    for grubenv in [&corrupt, &missing, &bad_target, &bad_counter] {
        let before = fs::read(grubenv).ok();
        let (output, took) = run(
            &[
                "--dry-run".as_ref(),
                "--grubenv".as_ref(),
                grubenv.as_os_str(),
                "--root".as_ref(),
                dir.path().as_os_str(),
            ],
            &[
                ("DBUS_SYSTEM_BUS_ADDRESS", &bus),
                ("APEX_BOOTC", "/nonexistent/bootc"),
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{grubenv:?}: {stderr}");
        assert!(
            stderr.contains("cannot assess this boot; grubenv was left unchanged"),
            "{stderr}"
        );
        assert!(output.stdout.is_empty(), "dry-run printed a decision");
        assert!(took < Duration::from_secs(20), "took {took:?}");
        assert_eq!(fs::read(grubenv).ok(), before, "{grubenv:?} changed");
        match listener.accept() {
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("the binary connected to the bus: {other:?}"),
        }
    }
}

struct PrivateBus {
    daemon: Child,
    address: String,
    _config: tempfile::TempDir,
}

impl PrivateBus {
    fn start() -> Option<Self> {
        let program = ["/usr/bin/dbus-daemon", "/bin/dbus-daemon"]
            .into_iter()
            .find(|p| Path::new(p).exists())?;
        let config = tempfile::tempdir().unwrap();
        let file = config.path().join("bus.conf");
        fs::write(
            &file,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:abstract={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                unique_name("bus")
            ),
        )
        .unwrap();
        let mut daemon = Command::new(program)
            .arg(format!("--config-file={}", file.display()))
            .arg("--nofork")
            .arg("--print-address=1")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // The address is printed once the bus accepts connections.
        let mut line = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line.trim().to_owned();
        assert!(address.starts_with("unix:"), "{address:?}");
        Some(Self {
            daemon,
            address,
            _config: config,
        })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

const SESSION_PATH: &str = "/org/freedesktop/login1/session/c1";

struct FakeManager;

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl FakeManager {
    fn list_sessions(&self) -> Vec<(String, u32, String, String, OwnedObjectPath)> {
        vec![(
            "c1".to_owned(),
            60,
            "gdm".to_owned(),
            "seat0".to_owned(),
            OwnedObjectPath::try_from(SESSION_PATH).unwrap(),
        )]
    }
}

/// The greeter, created 1 µs after boot, so it is as old as this machine's uptime.
struct FakeGreeter;

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl FakeGreeter {
    #[zbus(property)]
    fn class(&self) -> String {
        "greeter".to_owned()
    }

    #[zbus(property)]
    fn state(&self) -> String {
        "active".to_owned()
    }

    #[zbus(property)]
    fn timestamp_monotonic(&self) -> u64 {
        1
    }
}

struct FakeUpdated {
    ping_delay: Duration,
}

#[zbus::interface(name = "io.github.apex_linux.Apex.Update1")]
impl FakeUpdated {
    async fn ping(&self) {
        tokio::time::sleep(self.ping_delay).await;
    }

    #[zbus(property)]
    fn booted(&self) -> Deployment {
        Deployment {
            digest: Some(BOOTED.to_owned()),
            ..Deployment::default()
        }
    }
}

fn fake_root(force_red: Option<&str>) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let grub = root.path().join("boot/grub2");
    fs::create_dir_all(&grub).unwrap();
    fs::write(
        grub.join("grub.cfg"),
        format!(
            "set timeout=1\n### BEGIN 08_apex-bootcount.cfg ###\n{SNIPPET}### END 08_apex-bootcount.cfg ###\n### BEGIN 10_blscfg.cfg ###\nblscfg\n### END 10_blscfg.cfg ###\n"
        ),
    )
    .unwrap();
    let state = root.path().join("var/lib/apex");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    if let Some(content) = force_red {
        let hook = state.join("force-red");
        fs::write(&hook, content).unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o600)).unwrap();
    }
    root
}

fn dry_run(bus: &PrivateBus, root: &Path, grubenv: &[u8]) -> (Output, Value, PathBuf) {
    dry_run_with(bus, root, grubenv, Duration::ZERO)
}

fn dry_run_with(
    bus: &PrivateBus,
    root: &Path,
    grubenv: &[u8],
    ping_delay: Duration,
) -> (Output, Value, PathBuf) {
    let path = root.join("boot/grub2/grubenv");
    fs::write(&path, grubenv).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let address = bus.address.clone();
    let root = root.to_owned();
    let output = runtime.block_on(async move {
        let _logind = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at("/org/freedesktop/login1", FakeManager)
            .unwrap()
            .serve_at(SESSION_PATH, FakeGreeter)
            .unwrap()
            .build()
            .await
            .unwrap();
        let _updated = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("io.github.apex_linux.Apex.Update1")
            .unwrap()
            .serve_at(
                "/io/github/apex_linux/Apex/Update1",
                FakeUpdated { ping_delay },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        // The fakes are served by this runtime while the binary runs on a blocking thread.
        tokio::task::spawn_blocking(move || {
            run(
                &["--dry-run".as_ref(), "--root".as_ref(), root.as_os_str()],
                &[
                    ("DBUS_SYSTEM_BUS_ADDRESS", &address),
                    ("APEX_BOOTC", "/nonexistent/bootc"),
                ],
            )
            .0
        })
        .await
        .unwrap()
    });
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let json: Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| panic!("{err}: {stderr}"));
    (output, json, path)
}

/// bootloader-update.service's drop-in in the image reads this line of the journal to
/// tell a green boot.
fn assert_logs_decision_and_verdict(output: &Output, json: &Value) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let decision = json["decision"].as_str().unwrap();
    let verdict = json["verdict"].as_str().unwrap();
    let digest = json["digest"].as_str().unwrap();
    let logged = format!("decision: {decision} (report {verdict} {digest})");
    assert!(
        stderr.lines().any(|line| line.contains(&logged)),
        "no line contains {logged:?}:\n{stderr}"
    );
}

fn check_outcome<'a>(json: &'a Value, list: &str, name: &str) -> Option<&'a str> {
    json["input"][list]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .map(|c| c["outcome"].as_str().unwrap())
}

fn failed_checks(json: &Value) -> Vec<&str> {
    json["failed_checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect()
}

#[test]
fn dry_run_against_fakes_decides_and_changes_nothing() {
    let Some(bus) = PrivateBus::start() else {
        eprintln!("skipped: no dbus-daemon installed");
        return;
    };

    // The unit checks come from this host's systemd, so the verdict may be either.
    let root = fake_root(None);
    let grubenv = grubenv_with(&[("apex_ok", "0")]);
    let (output, json, path) = dry_run(&bus, root.path(), &grubenv);
    assert!(
        ["green", "red-unarmed"].contains(&json["decision"].as_str().unwrap()),
        "{json:#}"
    );
    assert_logs_decision_and_verdict(&output, &json);
    assert_eq!(json["input"]["booted_digest"], BOOTED);
    assert_eq!(json["input"]["booted_source"], "apex-updated");
    // bootc is missing, so the boot order is unknown.
    assert_eq!(json["input"]["default_deployment"], "unknown");
    assert_eq!(json["input"]["user_session_active"], false);
    assert_eq!(json["input"]["apex_target"], Value::Null);
    assert_eq!(json["input"]["apex_ok"], false);
    assert_eq!(json["digest"], BOOTED);
    for passed in [names::SESSION, names::UPDATER_PING, names::GRUB_CFG_MARKER] {
        assert_eq!(
            check_outcome(&json, "required", passed),
            Some("pass"),
            "{json:#}"
        );
    }
    assert_eq!(check_outcome(&json, "required", names::FORCED_RED), None);
    assert_eq!(json["reboot"], false);
    assert_eq!(fs::read(&path).unwrap(), grubenv);
    assert!(
        !root
            .path()
            .join("var/lib/apex/health-failure.json")
            .exists()
    );

    let root = fake_root(Some(OTHER));
    let (_, json, _) = dry_run(&bus, root.path(), &grubenv);
    assert_eq!(check_outcome(&json, "required", names::FORCED_RED), None);

    let root = fake_root(Some(&format!("{BOOTED}\n")));
    let armed = grubenv_with(&[
        ("apex_target", BOOTED),
        ("apex_counter", "1"),
        ("apex_ok", "0"),
    ]);
    let (output, json, path) = dry_run(&bus, root.path(), &armed);
    assert_eq!(json["decision"], "red-armed-no-session", "{json:#}");
    assert_eq!(json["verdict"], "red");
    assert_logs_decision_and_verdict(&output, &json);
    assert_eq!(json["reboot"], true);
    assert!(
        failed_checks(&json).contains(&names::FORCED_RED),
        "{json:#}"
    );
    assert_eq!(
        check_outcome(&json, "required", names::FORCED_RED),
        Some("fail")
    );
    assert_eq!(json["input"]["apex_counter"], 1);
    let steps: Vec<&str> = json["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["step"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        ["apply-env", "record-health-failure", "report", "reboot"]
    );
    assert_eq!(
        json["steps"][0]["ops"],
        serde_json::json!(["set apex_ok=0"])
    );
    assert_eq!(fs::read(&path).unwrap(), armed);
    assert!(
        !root
            .path()
            .join("var/lib/apex/health-failure.json")
            .exists()
    );
}

#[test]
fn a_daemon_that_holds_ping_during_its_first_refresh_passes() {
    let Some(bus) = PrivateBus::start() else {
        eprintln!("skipped: no dbus-daemon installed");
        return;
    };
    let root = fake_root(None);
    let grubenv = grubenv_with(&[("apex_ok", "0")]);
    let started = Instant::now();
    let (_, json, _) = dry_run_with(&bus, root.path(), &grubenv, Duration::from_secs(21));
    assert!(started.elapsed() >= Duration::from_secs(21));
    assert_eq!(
        check_outcome(&json, "required", names::UPDATER_PING),
        Some("pass"),
        "{json:#}"
    );
}
