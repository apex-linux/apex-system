#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use std::path::{Path, PathBuf};

use apexctl::hw::machine;
use apexctl::profile::Profile;
use apexctl::render::{self, AudioOutcome};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn profile_path() -> PathBuf {
    repo().join("hardware/m7400qc/profile.toml")
}

fn profile() -> Profile {
    Profile::load(&profile_path()).expect("hardware/m7400qc/profile.toml is valid")
}

#[test]
fn compiled_in_machine_facts_match_the_profile() {
    let profile = profile();
    assert_eq!(profile.machine.id, machine::MACHINE_ID);
    assert_eq!(profile.audio.codec.vendor_id, machine::ALC294_VENDOR_ID);
    assert_eq!(
        profile.audio.codec.subsystem_id,
        machine::ALC294_SUBSYSTEM_ID
    );
    assert_eq!(u16::from(profile.audio.codec.coef_nid), machine::COEF_NID);
    assert_eq!(profile.battery.name, machine::BATTERY);
    assert_eq!(
        profile.battery.charge_end_threshold,
        machine::CHARGE_END_THRESHOLD
    );
    assert_eq!(profile.gpu.discrete.pci, machine::DGPU_PCI);
    assert_eq!(profile.gpu.discrete.vendor, machine::DGPU_VENDOR);
    assert_eq!(profile.gpu.discrete.device, machine::DGPU_DEVICE);
    assert_eq!(profile.fingerprint.usb_vendor, machine::FINGERPRINT_VENDOR);
    assert_eq!(
        profile.fingerprint.usb_product,
        machine::FINGERPRINT_PRODUCT
    );
    assert_eq!(
        format!("/usr/lib/firmware/{}", render::firmware_reference(&profile)),
        machine::PATCH_FIRMWARE_PATH
    );
}

#[test]
fn committed_renders_match_the_profile() {
    let profile = profile();
    let rendered = render::render(&profile).expect("the profile renders");
    let generated = render::default_output_dir(&profile_path());
    let differences = render::check_outputs(&generated, &rendered).expect("readable");
    let diffs: Vec<String> = differences.iter().map(|d| d.diff.clone()).collect();
    assert!(
        differences.is_empty(),
        "hardware/m7400qc/generated is out of date; run `just render`:\n{}",
        diffs.join("\n")
    );
    if rendered.audio == AudioOutcome::NotMeasured {
        assert!(
            !generated
                .join(render::firmware_file_name(&profile))
                .exists()
        );
        assert!(!generated.join(render::AUDIO_CONF).exists());
    }
}

#[test]
fn rendered_monitors_xml_is_the_documented_one() {
    let xml = render::monitors_xml(&profile());
    insta::assert_snapshot!(xml);
    for needle in [
        "<monitors version=\"2\">",
        "<layoutmode>logical</layoutmode>",
        "<scale>2</scale>",
        "<connector>eDP-1</connector>",
        "<vendor>SDC</vendor>",
        "<product>0x4154</product>",
        "<serial>0x00000000</serial>",
        "<width>2880</width>",
        "<height>1800</height>",
        "<rate>90.001434326171875</rate>",
    ] {
        assert!(xml.contains(needle), "{needle}");
    }
}
