//! The watchdog's systemd side, against this host's `systemd-analyze` and `systemd-run`; as root
//! the latter ignores `DBUS_SYSTEM_BUS_ADDRESS` and talks to the real systemd, so its test skips.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]
#![allow(
    clippy::unused_self,
    reason = "zbus interface methods take &self; the fake records into shared state"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apex_boot::ASSESS_BINARY_PATH;
use apex_boot::emergency::{
    DROP_IN_FILE_NAME, EMERGENCY_DROP_IN, REBOOT_UNIT, RESCUE_DROP_IN, SHELL_UNITS, SYSTEMD_RUN,
    WATCHDOG_FLAG, schedule_args,
};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

const SYSTEMD_ANALYZE: &str = "/usr/bin/systemd-analyze";
const HOST_UNIT_DIR: &str = "/usr/lib/systemd/system";

fn assignments(unit: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in unit.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            name.clone_into(&mut section);
            continue;
        }
        let (key, value) = line.split_once('=').expect("key=value");
        assert!(
            !value.ends_with('\\'),
            "continuation lines are not expected"
        );
        out.push((section.clone(), key.to_owned(), value.to_owned()));
    }
    out
}

#[test]
fn embedded_copies_are_the_installed_files() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    for (unit, text) in [
        ("emergency.service", EMERGENCY_DROP_IN),
        ("rescue.service", RESCUE_DROP_IN),
    ] {
        assert!(SHELL_UNITS.contains(&unit));
        let file = data.join(format!("{unit}.d")).join(DROP_IN_FILE_NAME);
        assert_eq!(fs::read_to_string(&file).unwrap(), text, "{file:?}");
    }
    assert_eq!(EMERGENCY_DROP_IN, RESCUE_DROP_IN);
}

/// A condition would skip the shell, an empty `ExecStartPre=` would drop its `plymouth
/// --wait quit`, and a command without `-` would fail it.
#[test]
fn the_drop_in_only_adds_the_watchdog() {
    let all = assignments(EMERGENCY_DROP_IN);
    assert_eq!(
        all,
        [(
            "Service".to_owned(),
            "ExecStartPre".to_owned(),
            format!("-{ASSESS_BINARY_PATH} {WATCHDOG_FLAG}")
        )]
    );
}

fn verify(unit: &Path) -> (Option<i32>, String) {
    let output = Command::new(SYSTEMD_ANALYZE)
        .args(["verify", "--man=no"])
        .arg(unit)
        .env_clear()
        .env("PATH", "/usr/bin:/usr/sbin")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code(), text)
}

#[test]
fn systemd_analyze_accepts_the_drop_ins_with_the_host_units() {
    if !Path::new(SYSTEMD_ANALYZE).exists() {
        eprintln!("skipped: no systemd-analyze");
        return;
    }
    for unit in SHELL_UNITS {
        let host_unit = Path::new(HOST_UNIT_DIR).join(unit);
        if !host_unit.exists() {
            eprintln!("skipped: no {}", host_unit.display());
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join(unit);
        fs::copy(&host_unit, &copy).unwrap();
        let drop_ins = dir.path().join(format!("{unit}.d"));
        fs::create_dir(&drop_ins).unwrap();
        let drop_in = drop_ins.join(DROP_IN_FILE_NAME);

        // The host unit alone, to tell its own diagnostics from ours.
        let (code, alone) = verify(&copy);
        assert_eq!(code, Some(0), "{unit} alone: {alone}");

        fs::write(&drop_in, EMERGENCY_DROP_IN).unwrap();
        let (code, with_drop_in) = verify(&copy);
        assert_eq!(code, Some(0), "{unit}: {with_drop_in}");
        assert_eq!(with_drop_in, alone, "{unit}: the drop-in adds diagnostics");

        // A broken copy is reported with the drop-in's own path: verify did read it.
        fs::write(&drop_in, format!("{EMERGENCY_DROP_IN}NoSuchSetting=1\n")).unwrap();
        let (_, broken) = verify(&copy);
        assert!(
            broken.contains(&format!("{DROP_IN_FILE_NAME}:")) && broken.contains("NoSuchSetting"),
            "{unit}: {broken}"
        );
    }
}

#[test]
fn the_transient_units_are_valid_unit_settings() {
    if !Path::new(SYSTEMD_ANALYZE).exists() {
        eprintln!("skipped: no systemd-analyze");
        return;
    }
    let mut unit = vec!["CollectMode=inactive-or-failed".to_owned()];
    let mut timer_unit = unit.clone();
    let mut timer = vec![
        "RemainAfterElapse=no".to_owned(),
        format!("Unit={REBOOT_UNIT}.service"),
    ];
    let mut service = Vec::new();
    let args = schedule_args();
    let split = args.iter().position(|a| a == "--").unwrap();
    for arg in &args[..split] {
        if let Some(value) = arg.strip_prefix("--description=") {
            unit.push(format!("Description={value}"));
            timer_unit.push(format!("Description={value}"));
        } else if let Some(value) = arg.strip_prefix("--on-active=") {
            timer.push(format!("OnActiveSec={value}"));
        } else if let Some(setting) = arg.strip_prefix("--timer-property=") {
            if setting.starts_with("DefaultDependencies=") {
                timer_unit.push(setting.to_owned());
            } else {
                timer.push(setting.to_owned());
            }
        } else if let Some(setting) = arg.strip_prefix("--property=") {
            if setting.starts_with("DefaultDependencies=") {
                unit.push(setting.to_owned());
            } else {
                service.push(setting.to_owned());
            }
        } else {
            assert!(
                ["--quiet", "--no-ask-password", "--collect"].contains(&arg.as_str())
                    || arg.starts_with("--unit="),
                "unexpected systemd-run option {arg}"
            );
        }
    }
    service.push(format!("ExecStart={}", args[split + 1..].join(" ")));
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, sections: &[(&str, &[String])]| {
        let mut text = String::new();
        for (section, lines) in sections {
            writeln!(text, "[{section}]").unwrap();
            for line in *lines {
                text.push_str(line);
                text.push('\n');
            }
        }
        let path = dir.path().join(name);
        fs::write(&path, text).unwrap();
        path
    };
    let service_file = write(
        &format!("{REBOOT_UNIT}.service"),
        &[("Unit", &unit), ("Service", &service)],
    );
    let timer_file = write(
        &format!("{REBOOT_UNIT}.timer"),
        &[("Unit", &timer_unit), ("Timer", &timer)],
    );
    for file in [&service_file, &timer_file] {
        let (code, output) = verify(file);
        assert_eq!(code, Some(0), "{}: {output}", file.display());
        assert_eq!(output, "", "{}", file.display());
    }
    // A wrong setting in the same place is reported: the check is not blind.
    fs::write(
        &service_file,
        fs::read_to_string(&service_file).unwrap() + "ExecConditionTypo=/bin/true\n",
    )
    .unwrap();
    let (_, output) = verify(&service_file);
    assert!(output.contains("ExecConditionTypo"), "{output}");
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
  <type>system</type>
  <listen>unix:abstract=apex-boot-emergency-units-{}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                std::process::id()
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

type Properties = Vec<(String, OwnedValue)>;
type Recorded = (String, String, Properties, Vec<(String, Properties)>);

/// `JobRemoved` goes out before the reply; the client queues it until it waits for the job.
struct FakeManager {
    calls: Arc<Mutex<Vec<Recorded>>>,
}

const JOB: &str = "/org/freedesktop/systemd1/job/4242";

#[zbus::interface(name = "org.freedesktop.systemd1.Manager")]
impl FakeManager {
    async fn start_transient_unit(
        &self,
        name: String,
        mode: String,
        properties: Properties,
        aux: Vec<(String, Properties)>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        self.calls
            .lock()
            .unwrap()
            .push((name.clone(), mode, properties, aux));
        let job = ObjectPath::try_from(JOB).unwrap();
        Self::job_removed(&emitter, 4242, job.clone(), &name, "done").await?;
        Ok(job.into())
    }

    #[zbus(signal)]
    async fn job_removed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        job: ObjectPath<'_>,
        unit: &str,
        result: &str,
    ) -> zbus::Result<()>;
}

fn render(properties: &Properties) -> BTreeMap<String, String> {
    properties
        .iter()
        .map(|(name, value)| (name.clone(), format!("{}", **value)))
        .collect()
}

fn running_as_root() -> bool {
    fs::metadata("/proc/self").map_or(true, |m| m.uid() == 0)
}

#[test]
fn the_hosts_systemd_run_sends_the_intended_units() {
    if !Path::new(SYSTEMD_RUN).exists() || !Path::new("/run/systemd/system").exists() {
        eprintln!("skipped: no systemd-run, or not booted with systemd");
        return;
    }
    if running_as_root() {
        eprintln!("skipped: root's systemd-run would talk to the real systemd");
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        eprintln!("skipped: no dbus-daemon installed");
        return;
    };
    let calls = run_against_fake_systemd(&bus);
    assert_eq!(calls.len(), 1, "one StartTransientUnit call");
    let (name, mode, timer, aux) = &calls[0];
    assert_eq!(name, &format!("{REBOOT_UNIT}.timer"));
    assert_eq!(mode, "fail");
    assert_eq!(aux.len(), 1, "one service");
    assert_eq!(aux[0].0, format!("{REBOOT_UNIT}.service"));
    check_units(&render(timer), &render(&aux[0].1));
}

fn run_against_fake_systemd(bus: &PrivateBus) -> Vec<Recorded> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let address = bus.address.clone();
    let recorded = Arc::clone(&calls);
    let output = runtime.block_on(async move {
        let _systemd = zbus::connection::Builder::address(address.as_str())
            .unwrap()
            .name("org.freedesktop.systemd1")
            .unwrap()
            .serve_at("/org/freedesktop/systemd1", FakeManager { calls: recorded })
            .unwrap()
            .build()
            .await
            .unwrap();
        tokio::task::spawn_blocking(move || {
            let mut child = Command::new(SYSTEMD_RUN)
                .args(schedule_args())
                .env_clear()
                .env("PATH", "/usr/bin:/usr/sbin")
                .env("LANG", "C.UTF-8")
                .env("DBUS_SYSTEM_BUS_ADDRESS", &address)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                assert!(std::time::Instant::now() < deadline, "systemd-run hung");
                std::thread::sleep(Duration::from_millis(20));
            }
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    });
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "systemd-run: {stderr}");
    assert!(
        output.stdout.is_empty() && stderr.is_empty(),
        "--quiet: {stderr}"
    );
    let calls = calls.lock().unwrap();
    calls.clone()
}

fn check_units(timer: &BTreeMap<String, String>, service: &BTreeMap<String, String>) {
    let description = "\"Apex: reboot so GRUB counts the next boot of the update on trial\"";
    for (key, value) in [
        ("Description", description),
        ("CollectMode", "\"inactive-or-failed\""),
        ("DefaultDependencies", "false"),
        ("AccuracyUSec", "uint64 1000000"),
        ("RemainAfterElapse", "false"),
        ("TimersMonotonic", "[(\"OnActiveSec\", uint64 60000000)]"),
    ] {
        assert_eq!(
            timer.get(key).map(String::as_str),
            Some(value),
            "timer {key}: {timer:#?}"
        );
    }
    for (key, value) in [
        ("Description", description),
        ("CollectMode", "\"inactive-or-failed\""),
        ("DefaultDependencies", "false"),
        (
            "ExecCondition",
            "[(\"/usr/bin/systemctl\", [\"/usr/bin/systemctl\", \"--quiet\", \"is-active\", \
             \"emergency.service\", \"rescue.service\"], false)]",
        ),
        (
            "ExecStart",
            "[(\"/usr/bin/systemctl\", [\"/usr/bin/systemctl\", \"reboot\"], false)]",
        ),
    ] {
        assert_eq!(
            service.get(key).map(String::as_str),
            Some(value),
            "service {key}: {service:#?}"
        );
    }
    for props in [timer, service] {
        for key in [
            "Requires",
            "Wants",
            "After",
            "Before",
            "BindsTo",
            "Conflicts",
        ] {
            assert!(!props.contains_key(key), "{key}: {props:#?}");
        }
    }
    assert!(!service.contains_key("ExecStartPre"), "{service:#?}");
}
