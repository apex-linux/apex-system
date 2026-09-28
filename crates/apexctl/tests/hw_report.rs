//! `hw report` and `hw audio snapshot` against a fake sysfs/procfs tree.

#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use std::fs;
use std::path::Path;

use apexctl::hw::report::{Status, assess, gather, render};
use apexctl::hw::upower::UpowerFacts;
use apexctl::hw::{SysRoot, audio};

const CODEC: &str = include_str!("fixtures/alc294-m7400qc-codec0.txt");
const FIRMWARE: &str = "\
# test firmware
[codec]
0x10ec0294 0x10431ab2 0

[verb]
0x20 0x500 0x001b
0x20 0x400 0x4e4b
";

fn put(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
    let path = root.join(path.trim_start_matches('/'));
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

/// A healthy M7400QC: fix loaded, limit on, dGPU in D3cold, reader present, Secure Boot on.
fn healthy(root: &Path) {
    put(root, "/proc/asound/card1/codec#0", CODEC);
    put(
        root,
        "/proc/asound/card0/codec#0",
        "Codec: ATI R6xx HDMI\nAddress: 0\nVendor Id: 0x1002aa01\nSubsystem Id: 0x00aa0100\nRevision Id: 0x100700\n",
    );
    put(root, "/sys/class/sound/hwC0D0/vendor_id", "0x1002aa01\n");
    put(root, "/sys/class/sound/hwC0D0/subsystem_id", "0x00aa0100\n");
    put(root, "/sys/class/sound/hwC1D0/vendor_id", "0x10ec0294\n");
    put(root, "/sys/class/sound/hwC1D0/subsystem_id", "0x10431ab2\n");
    put(
        root,
        "/sys/class/sound/hwC1D0/init_verbs",
        "0x20 0x500 0x001b\n0x20 0x400 0x4e4b\n",
    );
    put(root, "/usr/lib/firmware/apex/alc294-m7400qc.fw", FIRMWARE);
    put(
        root,
        "/sys/module/snd_hda_intel/parameters/patch",
        "apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw,(null),(null)\n",
    );
    put(
        root,
        "/sys/class/power_supply/BAT0/charge_control_end_threshold",
        "80\n",
    );
    let gpu = "/sys/bus/pci/devices/0000:01:00.0";
    put(root, &format!("{gpu}/vendor"), "0x10de\n");
    put(root, &format!("{gpu}/device"), "0x25a2\n");
    put(root, &format!("{gpu}/power_state"), "D3cold\n");
    put(root, &format!("{gpu}/power/runtime_status"), "suspended\n");
    put(root, &format!("{gpu}/power/control"), "auto\n");
    put(root, &format!("{gpu}/power/runtime_active_time"), "2500\n");
    put(
        root,
        &format!("{gpu}/power/runtime_suspended_time"),
        "97500\n",
    );
    put(root, "/sys/bus/usb/devices/usb3/idVendor", "1d6b\n");
    put(root, "/sys/bus/usb/devices/usb3/idProduct", "0002\n");
    put(root, "/sys/bus/usb/devices/3-1/idVendor", "04f3\n");
    put(root, "/sys/bus/usb/devices/3-1/idProduct", "0c6e\n");
    put(root, "/sys/bus/usb/devices/3-1:1.0/bInterfaceClass", "ff\n");
    let guid = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
    put(
        root,
        &format!("/sys/firmware/efi/efivars/SecureBoot-{guid}"),
        [6, 0, 0, 0, 1],
    );
    put(
        root,
        &format!("/sys/firmware/efi/efivars/SetupMode-{guid}"),
        [6, 0, 0, 0, 0],
    );
}

#[test]
fn a_healthy_laptop_passes_everything() {
    let dir = tempfile::tempdir().expect("tempdir");
    healthy(dir.path());
    let mut facts = gather(&SysRoot::new(dir.path()));
    facts.upower = Some(UpowerFacts {
        supported: Some(true),
        enabled: Some(true),
        end_threshold: Some(80),
    });
    let report = assess(&facts);
    assert_eq!(
        report.audio.status,
        Status::Pass,
        "{}",
        report.audio.summary
    );
    assert_eq!(report.audio.hwdep.as_deref(), Some("hwC1D0"));
    assert_eq!(
        report.audio.patch_parameter.as_deref(),
        Some(
            "apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw,apex/alc294-m7400qc.fw"
        )
    );
    assert_eq!(report.battery.status, Status::Pass);
    assert_eq!(report.upower.status, Status::Pass);
    assert_eq!(report.dgpu.status, Status::Pass, "{}", report.dgpu.summary);
    assert_eq!(report.dgpu.suspended_percent, Some(97.5));
    assert_eq!(report.dgpu.driver, None);
    assert_eq!(report.fingerprint.status, Status::Pass);
    assert_eq!(report.fingerprint.devices, ["3-1"]);
    assert_eq!(report.secure_boot.status, Status::Pass);
    insta::assert_snapshot!("healthy_report_text", render(&report));
    insta::assert_json_snapshot!("healthy_report_json", report);
}

#[test]
fn problems_are_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    healthy(root);
    // The kernel did not load the firmware; the limit is off; the dGPU is awake and its
    // runtime PM is disabled; the reader is gone; Secure Boot is off.
    put(root, "/sys/class/sound/hwC1D0/init_verbs", "");
    put(
        root,
        "/sys/class/power_supply/BAT0/charge_control_end_threshold",
        "100\n",
    );
    put(
        root,
        "/sys/bus/pci/devices/0000:01:00.0/power/control",
        "on\n",
    );
    fs::remove_dir_all(root.join("sys/bus/usb/devices/3-1")).expect("rm");
    put(
        root,
        "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c",
        [6, 0, 0, 0, 0],
    );
    let mut facts = gather(&SysRoot::new(root));
    facts.upower = Some(UpowerFacts {
        supported: Some(false),
        enabled: Some(false),
        end_threshold: None,
    });
    let report = assess(&facts);
    assert_eq!(report.audio.status, Status::Fail);
    assert_eq!(report.battery.status, Status::Warn);
    assert_eq!(report.upower.status, Status::Fail);
    assert_eq!(report.dgpu.status, Status::Fail);
    assert!(report.dgpu.summary.contains("power/control = on"));
    assert_eq!(report.fingerprint.status, Status::Fail);
    assert_eq!(report.secure_boot.status, Status::Warn);
    insta::assert_snapshot!("broken_report_text", render(&report));
}

#[test]
fn an_image_without_the_fix_skips_audio_and_an_empty_tree_is_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    healthy(dir.path());
    fs::remove_file(dir.path().join("usr/lib/firmware/apex/alc294-m7400qc.fw")).expect("rm");
    let report = assess(&gather(&SysRoot::new(dir.path())));
    assert_eq!(report.audio.status, Status::Skip);
    assert!(report.audio.summary.contains("degraded=audio"));
    assert_eq!(report.upower.status, Status::Unknown);

    let empty = tempfile::tempdir().expect("tempdir");
    let report = assess(&gather(&SysRoot::new(empty.path())));
    assert_eq!(report.audio.status, Status::Fail);
    assert_eq!(report.battery.status, Status::Fail);
    assert_eq!(report.dgpu.status, Status::Unknown);
    assert_eq!(report.secure_boot.status, Status::Warn);
}

#[test]
fn audio_snapshot_finds_the_alc294_and_its_init_verbs() {
    let dir = tempfile::tempdir().expect("tempdir");
    healthy(dir.path());
    let snapshot = audio::snapshot(&SysRoot::new(dir.path())).expect("found");
    assert_eq!(snapshot.card, Some(1));
    assert_eq!(snapshot.hwdep.as_deref(), Some("hwC1D0"));
    assert_eq!(
        snapshot.init_verbs.as_deref(),
        Some(
            &[
                "0x20 0x500 0x001b".to_owned(),
                "0x20 0x400 0x4e4b".to_owned()
            ][..]
        )
    );
    assert!(audio::render(&snapshot).contains("SET_PROC_COEF 0x4e4b"));

    let empty = tempfile::tempdir().expect("tempdir");
    let error = audio::snapshot(&SysRoot::new(empty.path())).expect_err("nothing there");
    assert!(error.to_string().contains("no ALC294"));
    assert!(error.to_string().contains("--proc FILE"), "{error}");
}

#[test]
fn audio_snapshot_opens_only_the_codec_file_its_hwdep_names() {
    // Reading a codec file powers that codec (and its controller) up, so the ALC294 is
    // picked by the hwdep's cached sysfs ids. Here card0 holds a dump that also claims to
    // be the ALC294, and it comes first: it must not be the one read.
    let dir = tempfile::tempdir().expect("tempdir");
    healthy(dir.path());
    put(
        dir.path(),
        "/proc/asound/card0/codec#0",
        "not a codec dump at all\n",
    );
    let snapshot = audio::snapshot(&SysRoot::new(dir.path())).expect("found");
    assert!(
        snapshot.source.ends_with("proc/asound/card1/codec#0"),
        "{}",
        snapshot.source
    );
    assert_eq!(snapshot.card, Some(1));

    // The file the hwdep points at must describe the ALC294.
    put(
        dir.path(),
        "/proc/asound/card1/codec#0",
        "Codec: ATI R6xx HDMI\nAddress: 0\nVendor Id: 0x1002aa01\nSubsystem Id: 0x00aa0100\nRevision Id: 0x100700\n",
    );
    let error = audio::snapshot(&SysRoot::new(dir.path())).expect_err("wrong codec");
    assert!(
        error.to_string().ends_with(
            "describes codec 0x1002aa01 (subsystem 0x00aa0100), not the ALC294 that hwC1D0 names"
        ),
        "{error}"
    );

    // Without an ALC294 hwdep nothing under /proc/asound is opened, and the message lists
    // the hwdeps seen, from sysfs.
    fs::remove_dir_all(dir.path().join("sys/class/sound/hwC1D0")).expect("rm");
    let error = audio::snapshot(&SysRoot::new(dir.path())).expect_err("no hwdep");
    let text = error.to_string();
    assert!(
        text.contains("codecs seen: hwC0D0 0x1002aa01/0x00aa0100."),
        "{text}"
    );
}

#[test]
fn a_broken_alc294_dump_is_reported_with_its_line_not_as_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    healthy(dir.path());
    let broken = CODEC.replacen("Pin Default 0x", "Pin Default ", 1);
    put(dir.path(), "/proc/asound/card1/codec#0", broken);
    let error = audio::snapshot(&SysRoot::new(dir.path())).expect_err("malformed");
    let text = error.to_string();
    assert!(text.contains("card1/codec#0: line "), "{text}");
    assert!(text.contains("malformed Pin Default"), "{text}");
    assert!(!text.contains("no ALC294"), "{text}");
    // Printed once, not once more as a `source()`.
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn a_failed_coef_read_keeps_the_snapshot_usable() {
    // dump_coef=1 and one failed read: the kernel prints its -1 as 0xffffffff.
    let dir = tempfile::tempdir().expect("tempdir");
    healthy(dir.path());
    let with_coefs = CODEC.replacen(
        "  Processing caps: benign=0, ncoeff=130\n",
        "  Processing caps: benign=0, ncoeff=130\n    Coeff 0x00: 0x0002\n    Coeff 0x01: 0xffffffff\n    Coeff 0x1b: 0x4a4b\n",
        1,
    );
    assert_ne!(
        with_coefs, CODEC,
        "the fixture has the COEF widget's caps line"
    );
    put(dir.path(), "/proc/asound/card1/codec#0", with_coefs);
    let snapshot = audio::snapshot(&SysRoot::new(dir.path())).expect("parses");
    let widget = snapshot.codec.node(0x20).expect("COEF widget");
    assert_eq!(widget.coefs.get(&0x1b), Some(&0x4a4b));
    assert_eq!(widget.unreadable_coefs.get(&0x01), Some(&0xffff_ffff));
    let text = audio::render(&snapshot);
    assert!(text.contains("0x1b = 0x4a4b  bit 10 CLEAR"), "{text}");
    assert!(
        text.contains(
            "Not readable (0xffffffff: the kernel's read of the slot failed): 0x01 (0xffffffff)"
        ),
        "{text}"
    );
}

#[test]
fn an_installed_firmware_with_a_nul_fails_the_report() {
    // The kernel's patch loader loops forever on a NUL; hw report must not call it a pass.
    let dir = tempfile::tempdir().expect("tempdir");
    healthy(dir.path());
    let firmware = FIRMWARE.replace("# test firmware", "# test firmware M7400QC\0");
    put(
        dir.path(),
        "/usr/lib/firmware/apex/alc294-m7400qc.fw",
        firmware,
    );
    let report = assess(&gather(&SysRoot::new(dir.path())));
    assert_eq!(report.audio.status, Status::Fail);
    assert!(
        report
            .audio
            .summary
            .contains("line 1: control character 0x00"),
        "{}",
        report.audio.summary
    );
}
