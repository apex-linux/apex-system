#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test: a failed expectation is the test failure"
)]

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use apex_boot::checks::grub_cfg::MarkerError;
use apexctl::boot::{self, Layout, RepairError};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bootupd-0.2.35")
}

fn golden() -> String {
    fs::read_to_string(fixture().join("grub.cfg")).unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

const OLD_CSUM: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const NEW_CSUM: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// A root partition with two deployments as ostree lays them out, its boot partition mounted
/// on `boot/`, and grubenv and bootuuid.cfg already written by bootupd.
fn sysroot(dir: &Path) -> PathBuf {
    for (csum, version) in [(OLD_CSUM, 1), (NEW_CSUM, 2)] {
        let deployment = dir.join(format!("ostree/deploy/default/deploy/{csum}.0"));
        copy_tree(
            &fixture().join("grub2-static"),
            &deployment.join(boot::STATIC_DIR),
        );
        let link_dir = dir.join(format!("ostree/boot.1.1/default/{csum}"));
        fs::create_dir_all(&link_dir).unwrap();
        symlink(
            format!("../../../deploy/default/deploy/{csum}.0"),
            link_dir.join("0"),
        )
        .unwrap();
        let entries = dir.join(boot::LOADER_ENTRIES);
        fs::create_dir_all(&entries).unwrap();
        fs::write(
            entries.join(format!("ostree-{version}.conf")),
            format!(
                "title Apex ({version})\nversion {version}\n\
                 options root=UUID=1 rw ostree=/ostree/boot.1/default/{csum}/0\n"
            ),
        )
        .unwrap();
    }
    symlink("boot.1.1", dir.join("ostree/boot.1")).unwrap();
    let grub = dir.join(boot::GRUB_DIR);
    fs::create_dir_all(&grub).unwrap();
    fs::write(
        grub.join("grubenv"),
        "# GRUB Environment Block\napex_ok=1\n",
    )
    .unwrap();
    fs::write(grub.join("bootuuid.cfg"), "set BOOT_UUID=\"1\"\n").unwrap();
    dir.join(format!(
        "ostree/deploy/default/deploy/{NEW_CSUM}.0/{}",
        boot::STATIC_DIR
    ))
}

#[test]
fn the_rebuild_matches_bootupd_0_2_35_byte_for_byte() {
    let rebuilt = boot::rebuild(&fixture().join("grub2-static")).unwrap();
    assert_eq!(rebuilt.text, golden());
    assert_eq!(
        rebuilt.dropins,
        [
            "01_users.cfg",
            "07_apex-menu.cfg",
            "08_apex-bootcount.cfg",
            "10_blscfg.cfg",
            "14_menu_show_once.cfg",
            "30_uefi-firmware.cfg",
            "41_custom.cfg",
        ],
        "configs.d/README.md is skipped as bootupd skips it"
    );
}

#[test]
fn a_mounted_system_uses_the_default_entrys_deployment() {
    let dir = tempfile::tempdir().unwrap();
    let static_dir = sysroot(dir.path());
    let layout = Layout::sysroot(dir.path()).unwrap();
    assert_eq!(layout.static_dir, static_dir.canonicalize().unwrap());
    assert_eq!(layout.grub_cfg(), dir.path().join("boot/grub2/grub.cfg"));
    assert_eq!(Layout::host().grub_cfg(), Path::new("/boot/grub2/grub.cfg"));
    assert_eq!(
        Layout::host().static_dir,
        Path::new("/usr/lib/bootupd/grub2-static")
    );
}

#[test]
fn a_deployment_link_out_of_the_mounted_root_or_no_entry_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    sysroot(dir.path());
    let link = dir
        .path()
        .join(format!("ostree/boot.1.1/default/{NEW_CSUM}/0"));
    fs::remove_file(&link).unwrap();
    symlink("/usr", &link).unwrap();
    fs::remove_file(dir.path().join("boot/loader/entries/ostree-1.conf")).unwrap();
    let error = Layout::sysroot(dir.path()).unwrap_err();
    assert!(matches!(error, RepairError::NoDeployment { .. }), "{error}");

    let empty = tempfile::tempdir().unwrap();
    let error = Layout::sysroot(empty.path()).unwrap_err();
    assert!(
        error.to_string().contains("boot partition"),
        "the message says what to mount: {error}"
    );
}

#[test]
fn writing_keeps_a_backup_and_touches_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    sysroot(dir.path());
    let layout = Layout::sysroot(dir.path()).unwrap();
    fs::write(layout.grub_cfg(), "set default=0\n").unwrap();
    let grubenv_before = fs::read(layout.grub_dir.join("grubenv")).unwrap();
    let mtime = |name: &str| {
        fs::metadata(layout.grub_dir.join(name))
            .unwrap()
            .modified()
            .unwrap()
    };
    let grubenv_mtime = mtime("grubenv");

    let rebuilt = boot::rebuild(&layout.static_dir).unwrap();
    let current = boot::read_current(&layout).unwrap();
    let diff = boot::diff(&layout, current.as_deref(), &rebuilt);
    assert!(diff.contains("-set default=0"), "{diff}");
    assert!(diff.contains("+# apex-bootcount v1"), "{diff}");

    let mut flushed = Vec::new();
    let backup = boot::write(&layout, current.as_deref(), &rebuilt, |dir| {
        flushed.push(dir.to_owned());
    })
    .unwrap();
    assert_eq!(flushed, std::slice::from_ref(&layout.grub_dir));
    assert_eq!(backup, Some(layout.backup()));
    assert_eq!(
        fs::read_to_string(layout.backup()).unwrap(),
        "set default=0\n"
    );
    assert_eq!(fs::read_to_string(layout.grub_cfg()).unwrap(), golden());
    for written in [layout.grub_cfg(), layout.backup()] {
        let mode = fs::metadata(&written).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, boot::MODE, "{}", written.display());
    }
    assert_eq!(
        fs::read(layout.grub_dir.join("grubenv")).unwrap(),
        grubenv_before
    );
    assert_eq!(mtime("grubenv"), grubenv_mtime);
    let mut names: Vec<_> = fs::read_dir(&layout.grub_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["bootuuid.cfg", "grub.cfg", "grub.cfg.apex-bak", "grubenv"]
    );
    assert_eq!(
        boot::read_current(&layout).unwrap().as_deref(),
        Some(rebuilt.text.as_bytes()),
        "a second run finds nothing to do"
    );
    let shown = boot::render_written(&layout, &rebuilt, backup.as_deref());
    assert!(shown.contains("grub.cfg.apex-bak"), "{shown}");
    assert!(shown.contains("08_apex-bootcount.cfg"), "{shown}");
}

#[test]
fn a_missing_grub_cfg_is_written_without_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    sysroot(dir.path());
    let layout = Layout::sysroot(dir.path()).unwrap();
    let rebuilt = boot::rebuild(&layout.static_dir).unwrap();
    assert_eq!(boot::read_current(&layout).unwrap(), None);
    let diff = boot::diff(&layout, None, &rebuilt);
    assert!(diff.starts_with("--- /dev/null"), "{diff}");
    assert_eq!(boot::write(&layout, None, &rebuilt, |_| {}).unwrap(), None);
    assert!(!layout.backup().exists());
    assert_eq!(fs::read_to_string(layout.grub_cfg()).unwrap(), golden());
}

#[test]
fn static_configs_without_the_boot_counter_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let static_dir = sysroot(dir.path());
    fs::remove_file(static_dir.join("configs.d/08_apex-bootcount.cfg")).unwrap();
    let error = boot::rebuild(&static_dir).unwrap_err();
    assert!(
        matches!(error, RepairError::Marker(MarkerError::Missing)),
        "{error}"
    );
    assert!(error.to_string().contains("nothing was changed"), "{error}");
}

fn is_root() -> bool {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    boot::effective_uid(&status) == Some(0)
}

/// CI sets it, so the root half fails instead of passing unseen when the job loses root.
const REQUIRE_ROOT: &str = "APEXCTL_TEST_REQUIRE_ROOT";

const NOBODY: &str = "65534";

fn repair_command(root: &Path, extra: &[&str], as_nobody: bool) -> Command {
    let exe = env!("CARGO_BIN_EXE_apexctl");
    let mut command = if as_nobody {
        let mut setpriv = Command::new("/usr/bin/setpriv");
        setpriv.args([
            &format!("--reuid={NOBODY}"),
            &format!("--regid={NOBODY}"),
            "--clear-groups",
            "--",
            exe,
        ]);
        setpriv
    } else {
        Command::new(exe)
    };
    command
        .args(["boot", "repair-grub", "--sysroot"])
        .arg(root)
        .args(extra);
    command
}

fn run(mut command: Command) -> (i32, String, String) {
    let output = command.output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn repair(root: &Path, extra: &[&str]) -> (i32, String, String) {
    run(repair_command(root, extra, false))
}

fn scratch_system() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    sysroot(dir.path());
    let grub_cfg = dir.path().join("boot/grub2/grub.cfg");
    fs::write(&grub_cfg, "set default=0\n").unwrap();
    (dir, grub_cfg)
}

/// Run as root, it drops to nobody, so the refusal is tested in CI's root container too.
#[test]
fn the_command_refuses_without_root() {
    let (dir, grub_cfg) = scratch_system();
    let (code, _, stderr) = run(repair_command(dir.path(), &[], is_root()));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("sudo apexctl boot repair-grub"), "{stderr}");
    assert_eq!(fs::read_to_string(&grub_cfg).unwrap(), "set default=0\n");
}

#[test]
fn the_command_checks_then_repairs_as_root() {
    if !is_root() {
        assert!(
            std::env::var_os(REQUIRE_ROOT).is_none(),
            "{REQUIRE_ROOT} is set, but the tests do not run as root"
        );
        return;
    }
    let (dir, grub_cfg) = scratch_system();
    let (code, _, stderr) = repair(dir.path(), &["--check"]);
    assert_eq!(code, 1, "drift: {stderr}");
    assert!(stderr.contains("+# apex-bootcount v1"), "{stderr}");
    assert_eq!(fs::read_to_string(&grub_cfg).unwrap(), "set default=0\n");

    let (code, stdout, stderr) = repair(dir.path(), &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Saved the old grub.cfg"), "{stdout}");
    assert_eq!(fs::read_to_string(&grub_cfg).unwrap(), golden());

    assert_eq!(repair(dir.path(), &["--check"]).0, 0);
    assert_eq!(repair(dir.path(), &[]).0, 3, "nothing to do");
}
