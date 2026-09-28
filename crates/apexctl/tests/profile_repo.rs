//! Sync tests against the repository: the real hardware profile, the firmware list of the
//! image checks, the committed renders and the constants compiled into `apexctl`.

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

/// Package names listed in an `image/packages/*.txt` file: one per line, `#` starts a
/// comment.
fn package_list(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|name| !name.is_empty())
        .collect()
}

#[test]
fn base_packages_install_every_profile_firmware_package() {
    let base = repo().join("image/packages/base.txt");
    let Ok(text) = std::fs::read_to_string(&base) else {
        eprintln!("{} not found; nothing to compare", base.display());
        return;
    };
    let listed = package_list(&text);
    let profile = profile();
    let missing: Vec<&String> = profile
        .firmware
        .packages
        .iter()
        .filter(|package| !listed.contains(&package.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "image/packages/base.txt must install every [firmware].packages entry; missing: {missing:?}"
    );
}

#[test]
fn firmware_list_mirrors_the_profile() {
    let list = render::firmware_list_path(&profile_path())
        .expect("the profile sits in hardware/<machine>/");
    let Ok(text) = std::fs::read_to_string(&list) else {
        // The sync is only required when the image checks exist.
        eprintln!("{} not found; nothing to compare", list.display());
        return;
    };
    let profile = profile();
    assert_eq!(
        render::compare_firmware_list(&profile, &text),
        None,
        "image/checks/firmware-files.txt must list exactly [firmware].required_files, in order"
    );
    assert_eq!(
        render::parse_firmware_list(&text),
        profile.firmware.required_files
    );
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
    // ADR 0003 and the display research: version 2, logical layout, eDP-1 SDC 0x4154
    // serial 0x00000000, 2880x1800 at 90.001434326171875 Hz, scale 2.
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
