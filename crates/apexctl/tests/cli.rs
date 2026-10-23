#![allow(clippy::expect_used, reason = "test helpers panic on broken fixtures")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output};

fn apexctl(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_apexctl"));
    command.args(args);
    command
}

fn run(command: &mut Command) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = command.output().expect("apexctl runs");
    (
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn scratch_repo(dir: &Path) -> std::path::PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let machine = dir.join("hardware/m7400qc");
    fs::create_dir_all(&machine).expect("mkdir");
    fs::copy(
        repo.join("hardware/m7400qc/profile.toml"),
        machine.join("profile.toml"),
    )
    .expect("copy");
    machine.join("profile.toml")
}

/// Placeholders, not this laptop's values.
const MEASUREMENTS: &str = "
[audio.measurements]
date = 2026-09-25
bios = 'M7400QC.311'
kernel = '7.0.0-31-generic'
cold_boot = [ { index = 0x0f, value = 0x7770 }, { index = 0x1b, value = 0x4a4b }, { index = 0x40, value = 0x8800 } ]
after_windows = [ { index = 0x0f, value = 0x7774 }, { index = 0x1b, value = 0x4a4b }, { index = 0x40, value = 0x8800 } ]
";

#[test]
fn render_then_check_then_drift() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = scratch_repo(dir.path());
    let profile = profile.to_str().expect("utf-8");

    let (code, stdout, stderr) = run(&mut apexctl(&["profile", "render", "--profile", profile]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("wrote"), "{stdout}");
    assert!(stderr.contains("no [audio.measurements]"), "{stderr}");
    let generated = dir.path().join("hardware/m7400qc/generated");
    assert!(generated.join("monitors.xml").is_file());
    assert!(!generated.join("alc294-m7400qc.fw").exists());

    let (code, stdout, _) = run(&mut apexctl(&[
        "profile",
        "render",
        "--profile",
        profile,
        "--check",
    ]));
    assert_eq!(code, 0);
    assert!(stdout.contains("is up to date"));

    let xml = generated.join("monitors.xml");
    let edited = fs::read_to_string(&xml)
        .expect("read")
        .replace("<scale>2</scale>", "<scale>1.6666666269302368</scale>");
    fs::write(&xml, edited).expect("write");
    fs::write(generated.join("alc294-m7400qc.fw"), "[codec]\n").expect("write");
    let (code, _, stderr) = run(&mut apexctl(&[
        "profile",
        "render",
        "--profile",
        profile,
        "--check",
    ]));
    assert_eq!(code, 1);
    assert!(
        stderr.contains("-      <scale>1.6666666269302368</scale>"),
        "{stderr}"
    );
    assert!(stderr.contains("+      <scale>2</scale>"), "{stderr}");
    assert!(stderr.contains("+++ /dev/null"), "{stderr}");

    let (code, stdout, _) = run(&mut apexctl(&["profile", "render", "--profile", profile]));
    assert_eq!(code, 0);
    assert!(stdout.contains("removed stale"), "{stdout}");
    let (code, _, _) = run(&mut apexctl(&[
        "profile",
        "render",
        "--profile",
        profile,
        "--check",
    ]));
    assert_eq!(code, 0);
}

#[test]
fn an_invalid_profile_is_an_error_with_its_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = scratch_repo(dir.path());
    let text = fs::read_to_string(&profile)
        .expect("read")
        .replace("coef_nid", "coef_nod");
    fs::write(&profile, text).expect("write");
    let (code, _, stderr) = run(&mut apexctl(&[
        "profile",
        "render",
        "--profile",
        profile.to_str().expect("utf-8"),
    ]));
    assert_eq!(code, 1);
    assert!(stderr.contains("coef_nod"), "{stderr}");
}

#[test]
fn a_board_name_with_a_nul_or_a_line_feed_never_reaches_the_firmware() {
    for board in [r"M7400QC\u0000", r"M7400QC\n[pincfg]\n0x19 0x411111f0"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = scratch_repo(dir.path());
        let text = fs::read_to_string(&profile).expect("read").replace(
            r#"dmi_board_name = "M7400QC""#,
            &format!(r#"dmi_board_name = "{board}""#),
        );
        assert!(text.contains(board), "the profile names the board once");
        fs::write(&profile, format!("{text}{MEASUREMENTS}")).expect("write");
        let (code, stdout, stderr) = run(&mut apexctl(&[
            "profile",
            "render",
            "--profile",
            profile.to_str().expect("utf-8"),
        ]));
        assert_eq!(code, 1, "{board}: {stdout}");
        assert!(
            stderr.contains("machine.dmi_board_name: must be 1 to 64 printable ASCII characters"),
            "{stderr}"
        );
        let generated = dir.path().join("hardware/m7400qc/generated");
        assert!(!generated.join("alc294-m7400qc.fw").exists());
    }
}

#[test]
fn a_measured_profile_renders_firmware_the_kernel_reads_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let profile = scratch_repo(dir.path());
    let text = fs::read_to_string(&profile).expect("read");
    fs::write(&profile, format!("{text}{MEASUREMENTS}")).expect("write");
    let (code, _, stderr) = run(&mut apexctl(&[
        "profile",
        "render",
        "--profile",
        profile.to_str().expect("utf-8"),
    ]));
    assert_eq!(code, 0, "{stderr}");
    let firmware = fs::read_to_string(
        dir.path()
            .join("hardware/m7400qc/generated/alc294-m7400qc.fw"),
    )
    .expect("rendered");
    let parsed = apexctl::patch::parse(&firmware).expect("loadable");
    assert_eq!(
        parsed.verbs.len(),
        4,
        "0x0f restored, 0x1b fixed: {firmware}"
    );
}

#[test]
fn errors_name_their_cause_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("nowhere/profile.toml");
    let (code, _, stderr) = run(&mut apexctl(&[
        "profile",
        "render",
        "--profile",
        missing.to_str().expect("utf-8"),
    ]));
    assert_eq!(code, 1);
    assert_eq!(stderr.matches("(os error 2)").count(), 1, "{stderr}");

    let profile = scratch_repo(dir.path());
    let typo = fs::read_to_string(&profile)
        .expect("read")
        .replace("coef_nid", "coef_nod");
    fs::write(&profile, typo).expect("write");
    let (code, _, stderr) = run(&mut apexctl(&[
        "profile",
        "render",
        "--check",
        "--profile",
        profile.to_str().expect("utf-8"),
    ]));
    assert_eq!(code, 1);
    assert_eq!(
        stderr.matches("unknown field `coef_nod`").count(),
        1,
        "{stderr}"
    );
    assert_eq!(stderr.matches("TOML parse error").count(), 1, "{stderr}");

    let profile = scratch_repo(&dir.path().join("second"));
    let file = dir.path().join("a-file");
    fs::write(&file, "").expect("write");
    let out = file.join("generated");
    let (code, _, stderr) = run(&mut apexctl(&[
        "profile",
        "render",
        "--profile",
        profile.to_str().expect("utf-8"),
        "--out",
        out.to_str().expect("utf-8"),
    ]));
    assert_eq!(code, 1);
    assert_eq!(stderr.matches("(os error 20)").count(), 1, "{stderr}");

    let dump = dir.path().join("codec.txt");
    fs::write(
        &dump,
        "Codec: X\nVendor Id: 0x1\nSubsystem Id: 0x2\nRevision Id: 0x3\n\
         Node 0x20 [Vendor Defined Widget] wcaps 0xf00040: Mono\n    Coeff 0x1b: 4a4b\n",
    )
    .expect("write");
    let (code, _, stderr) = run(&mut apexctl(&[
        "hw",
        "audio",
        "snapshot",
        "--proc",
        dump.to_str().expect("utf-8"),
    ]));
    assert_eq!(code, 1);
    assert_eq!(stderr.matches("malformed Coeff").count(), 1, "{stderr}");
}

#[test]
fn apply_rejects_a_bad_digest_before_contacting_the_service() {
    let (code, stdout, stderr) = run(apexctl(&["update", "apply", "--digest", "nope"]).env(
        "DBUS_SYSTEM_BUS_ADDRESS",
        "unix:path=/nonexistent/apex-test-bus",
    ));
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.is_empty());
    assert!(
        stderr.starts_with("error: invalid value 'nope' for '--digest <DIGEST>'"),
        "{stderr}"
    );
    assert!(!stderr.contains("D-Bus"), "{stderr}");
}

#[test]
fn audio_snapshot_of_a_dump_file() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/alc294-m7400qc-codec0.txt"
    );
    let (code, stdout, stderr) = run(&mut apexctl(&[
        "hw", "audio", "snapshot", "--proc", fixture, "--json",
    ]));
    assert_eq!(code, 0, "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(json["codec"]["vendor_id"], 0x10ec_0294);
    assert_eq!(json["init_verbs"], serde_json::Value::Null);
    let (code, stdout, _) = run(&mut apexctl(&[
        "hw", "audio", "snapshot", "--proc", fixture,
    ]));
    assert_eq!(code, 0);
    assert!(stdout.contains("Realtek ALC294"));
    assert!(stdout.contains("dump_coef"));
}

#[test]
fn hw_report_json_on_an_empty_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_str().expect("utf-8");
    let (code, stdout, stderr) = run(&mut apexctl(&["hw", "report", "--json", "--sysroot", root]));
    assert_eq!(code, 0, "{stderr}");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(json["schema"], 1);
    assert_eq!(json["audio"]["status"], "fail");
    assert_eq!(json["upower"]["status"], "unknown");
}

#[test]
fn update_commands_explain_an_unreachable_bus() {
    for args in [
        &["status"][..],
        &["status", "--json"],
        &["update", "check"],
        &["update", "acknowledge"],
        &["update", "channel", "stable"],
        &["update", "channel", "testing"],
    ] {
        let (code, stdout, stderr) = run(apexctl(args).env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/nonexistent/apex-test-bus",
        ));
        assert_eq!(code, 1, "{args:?}");
        assert!(stdout.is_empty());
        assert!(
            stderr.contains("cannot connect to the system D-Bus"),
            "{stderr}"
        );
    }
}

#[test]
fn build_manifest_with_a_fake_rpm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).expect("mkdir");
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("mkdir");
    let script = format!(
        "#!/bin/sh\n\
         [ \"$1\" = --root ] && [ \"$2\" = '{}' ] && [ \"$3\" = -qa ] && [ \"$4\" = --qf ] || exit 9\n\
         [ \"$5\" = '%{{NAME}}\\t%{{EPOCHNUM}}\\t%{{VERSION}}\\t%{{RELEASE}}\\t%{{ARCH}}\\n' ] || exit 8\n\
         printf 'zsh\\t0\\t5.9\\t14.fc45\\tx86_64\\ngpg-pubkey\\t0\\tf577861e\\t6602e6d4\\t(none)\\nbash\\t0\\t5.3.0\\t2.fc45\\tx86_64\\n'\n",
        root.display()
    );
    let rpm = bin.join("rpm");
    fs::write(&rpm, script).expect("write");
    fs::set_permissions(&rpm, fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let base = format!("quay.io/fedora/fedora-bootc@sha256:{}", "5".repeat(64));
    let (code, stdout, stderr) = run(apexctl(&[
        "build",
        "manifest",
        "--root",
        root.to_str().expect("utf-8"),
        "--version",
        "45.20260924.117",
        "--revision",
        "0123456789abcdef0123456789abcdef01234567",
        "--base",
        &base,
        "--degraded",
        "fingerprint",
        "--degraded",
        "audio",
        "--held",
        "gnome-shell",
    ])
    .env("PATH", path.clone()));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("2 packages"), "{stdout}");
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("usr/share/apex/manifest.json")).expect("written"),
    )
    .expect("JSON");
    assert_eq!(
        manifest["degraded"],
        serde_json::json!(["audio", "fingerprint"])
    );
    assert_eq!(manifest["held"], serde_json::json!(["gnome-shell"]));
    assert_eq!(manifest["packages"][0]["name"], "bash");
    assert_eq!(manifest["fedora"], 45);

    let (code, _, stderr) = run(apexctl(&[
        "build",
        "manifest",
        "--root",
        root.to_str().expect("utf-8"),
        "--version",
        "45.2026-09-24.1",
        "--revision",
        "0123456789abcdef0123456789abcdef01234567",
        "--base",
        &base,
    ])
    .env("PATH", path));
    assert_eq!(code, 1);
    assert!(stderr.contains("45.2026-09-24.1"), "{stderr}");
}

#[test]
fn help_names_every_contract_command() {
    let (code, stdout, _) = run(&mut apexctl(&["--help"]));
    assert_eq!(code, 0);
    for command in ["status", "update", "hw", "profile", "build"] {
        assert!(stdout.contains(command), "{command}");
    }
    assert!(stdout.contains("Exit status"));
    let (_, stdout, _) = run(&mut apexctl(&["update", "--help"]));
    for command in [
        "check",
        "download",
        "apply",
        "discard",
        "rollback",
        "changes",
        "acknowledge",
        "channel",
        "automatic",
    ] {
        assert!(stdout.contains(command), "{command}");
    }
    let (code, stdout, _) = run(&mut apexctl(&["update", "channel", "--help"]));
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Usage: apexctl update channel <CHANNEL>"),
        "{stdout}"
    );
    assert!(stdout.contains("administrator password"), "{stdout}");
    assert!(
        stdout.contains("[possible values: stable, testing]"),
        "{stdout}"
    );
    let (code, stdout, _) = run(&mut apexctl(&["update", "acknowledge", "--help"]));
    assert_eq!(code, 0);
    assert!(stdout.contains("health checks"), "{stdout}");
    let (_, stdout, _) = run(&mut apexctl(&["hw", "report", "--help"]));
    assert!(!stdout.contains("sysroot"), "the test root stays hidden");
}

#[test]
fn usage_errors_exit_1_because_2_means_a_gate_refused() {
    for args in [
        &["update", "channel", "beta"][..],
        &["update", "channel"],
        &["update", "acknowledge", "now"],
        &["update", "reboot"],
        &["--no-such-flag"],
        &[],
    ] {
        let (code, stdout, stderr) = run(&mut apexctl(args));
        assert_eq!(code, 1, "{args:?}: {stderr}");
        assert!(stdout.is_empty(), "{args:?}");
        assert!(
            stderr.starts_with("error:") || stderr.contains("Usage: apexctl <COMMAND>"),
            "{args:?}: {stderr}"
        );
    }
    let (_, _, stderr) = run(&mut apexctl(&["update", "channel", "beta"]));
    assert!(
        stderr.contains("invalid value 'beta'") && stderr.contains("stable, testing"),
        "{stderr}"
    );
    let (code, stdout, _) = run(&mut apexctl(&["--version"]));
    assert_eq!(code, 0);
    assert!(stdout.starts_with("apexctl "), "{stdout}");
}
