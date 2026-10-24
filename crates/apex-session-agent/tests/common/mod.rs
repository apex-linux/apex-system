#![allow(
    dead_code,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers: each test binary uses its own part, and a failure is the test failure"
)]

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use apex_session_agent::{INTERFACE_SCHEMA_ID, SCHEMA_ID};

/// A copy of apex-desktop's `desktop/schemas/` file, and the one stock key the agent reads.
const SCHEMA_FILES: [&str; 2] = [
    "io.github.apex_linux.Apex.Desktop.gschema.xml",
    "org.gnome.desktop.interface.gschema.xml",
];

/// The Apex settings and the interface settings, on one memory backend.
pub(crate) fn settings_from_the_compiled_schemas(dir: &Path) -> (gio::Settings, gio::Settings) {
    let schemas = dir.join("schemas");
    fs::create_dir_all(&schemas).unwrap();
    for name in SCHEMA_FILES {
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
        fs::copy(data.join(name), schemas.join(name)).unwrap();
    }
    let status = Command::new("glib-compile-schemas")
        .arg("--strict")
        .arg(&schemas)
        .status()
        .expect("glib-compile-schemas (Fedora: glib2) must be installed");
    assert!(status.success());
    let source = gio::SettingsSchemaSource::from_directory(&schemas, None, false).unwrap();
    let backend = gio::memory_settings_backend_new();
    let settings = |id: &str| {
        let schema = source.lookup(id, false).expect(id);
        gio::Settings::new_full(&schema, Some(&backend), None)
    };
    (settings(SCHEMA_ID), settings(INTERFACE_SCHEMA_ID))
}

pub(crate) fn wait_for(what: &str, done: impl Fn() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        if !context.iteration(false) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub(crate) fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// A private `dbus-daemon`, standing in for the session and the system bus.
pub(crate) struct Bus {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl Bus {
    pub(crate) fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("bus.conf");
        fs::write(
            &config,
            format!(
                "<busconfig><listen>unix:dir={}</listen><auth>EXTERNAL</auth>\
                 <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
                 <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
                dir.path().display()
            ),
        )
        .unwrap();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon (Fedora: dbus-daemon) must be installed");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().to_owned(),
            _dir: dir,
        }
    }

    pub(crate) fn connect(&self) -> gio::DBusConnection {
        gio::DBusConnection::for_address_sync(
            &self.address,
            gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
            None,
            gio::Cancellable::NONE,
        )
        .unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
