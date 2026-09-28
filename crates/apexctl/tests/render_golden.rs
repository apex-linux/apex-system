//! Golden output of the audio renderer for the real profile plus sample measurements.
//!
//! The measurement values below are placeholders chosen to exercise every rule (0x1b gets
//! bit 10, 0x0f differs after Windows and is restored, 0x40 does not differ and is not
//! written, 0x2a is an extra measured index). They are not this laptop's values.

#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use apexctl::hda::Verb;
use apexctl::patch;
use apexctl::profile::Profile;
use apexctl::render::{self, AudioOutcome};

const SAMPLE_MEASUREMENTS: &str = "
[audio.measurements]
date = 2026-09-25
bios = \"M7400QC.311\"
kernel = \"7.0.0-31-generic\"
cold_boot = [ { index = 0x0f, value = 0x7770 }, { index = 0x1b, value = 0x4a4b }, { index = 0x2a, value = 0x0012 }, { index = 0x40, value = 0x8800 } ]
after_windows = [ { index = 0x0f, value = 0x7774 }, { index = 0x1b, value = 0x4a4b }, { index = 0x40, value = 0x8800 } ]
with_fix_loaded = [ { index = 0x0f, value = 0x7770 }, { index = 0x1b, value = 0x4e4b }, { index = 0x40, value = 0x8800 } ]
";

fn measured_profile() -> Profile {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../hardware/m7400qc/profile.toml"
    );
    let text = std::fs::read_to_string(path).expect("profile readable");
    Profile::parse(&format!("{text}{SAMPLE_MEASUREMENTS}")).expect("profile with measurements")
}

fn file<'r>(rendered: &'r render::Rendered, name: &str) -> &'r str {
    rendered
        .files
        .iter()
        .find(|f| f.name == name)
        .and_then(|f| f.contents.as_deref())
        .unwrap_or_else(|| panic!("{name} rendered"))
}

#[test]
fn patch_firmware_golden() {
    let rendered = render::render(&measured_profile()).expect("renders");
    let firmware = file(&rendered, "alc294-m7400qc.fw");
    insta::assert_snapshot!(firmware);

    // What the kernel will replay, decoded.
    let parsed = patch::parse(firmware).expect("the kernel-side parser accepts it");
    let effective: Vec<Verb> = parsed.verbs.iter().map(|v| v.effective()).collect();
    assert_eq!(
        effective,
        [
            Verb::SetCoefIndex(0x0f),
            Verb::SetProcCoef(0x7770),
            Verb::SetCoefIndex(0x1b),
            Verb::SetProcCoef(0x4e4b),
        ]
    );
    assert!(matches!(rendered.audio, AudioOutcome::Rendered(ref w) if w.len() == 2));
    assert!(firmware.lines().all(|l| l.len() <= patch::MAX_LINE));
}

#[test]
fn modprobe_conf_golden() {
    let rendered = render::render(&measured_profile()).expect("renders");
    insta::assert_snapshot!(file(&rendered, "apex-audio.conf"));
}
