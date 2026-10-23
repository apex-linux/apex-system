#![allow(
    dead_code,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers: each test binary uses its own part, and a failure is the test failure"
)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use apex_session_agent::SCHEMA_ID;

/// A copy of apex-desktop's `desktop/schemas/` file, which this repository does not hold.
const SCHEMA_XML: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/data/io.github.apex_linux.Apex.Desktop.gschema.xml"
);

pub(crate) fn settings_from_the_compiled_schema(dir: &Path) -> gio::Settings {
    let schemas = dir.join("schemas");
    fs::create_dir_all(&schemas).unwrap();
    fs::copy(
        SCHEMA_XML,
        schemas.join("io.github.apex_linux.Apex.Desktop.gschema.xml"),
    )
    .unwrap();
    let status = Command::new("glib-compile-schemas")
        .arg("--strict")
        .arg(&schemas)
        .status()
        .expect("glib-compile-schemas (Fedora: glib2) must be installed");
    assert!(status.success());
    let source = gio::SettingsSchemaSource::from_directory(&schemas, None, false).unwrap();
    let schema = source.lookup(SCHEMA_ID, false).expect("the Apex schema");
    let backend = gio::memory_settings_backend_new();
    gio::Settings::new_full(&schema, Some(&backend), None)
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
