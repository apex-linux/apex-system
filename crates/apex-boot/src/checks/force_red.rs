//! The qualification hook "forced red" (ADR 0009, Consequences: "an injected red check
//! leads to a counted rollback").
//!
//! The KVM qualification needs a deployment that fails its assessment on purpose, so it can
//! watch GRUB count the boots down and fall back. It writes that deployment's digest to
//! [`FORCE_RED_PATH`] as root. When the file names the booted digest, the assessment adds
//! the failed required check [`names::FORCED_RED`], and the decision table treats the boot
//! like any other red boot.
//!
//! It never influences anything otherwise:
//!
//! - absent file: the normal case, nothing happens;
//! - a file that names another digest, or a boot whose digest is unknown: nothing happens;
//! - a file that is not a regular file (a symlink, a FIFO, a directory), is larger than
//!   [`MAX_LEN`] bytes, cannot be read, or that is writable by group or others, or whose
//!   directory is: ignored, with the reason for the journal.
//!
//! `/var/lib/apex` is `0700 root` (M1 contract §1), so only root can create the file; the
//! mode checks keep that true if the directory is ever misconfigured. Because the file
//! names one digest, it cannot outlive the deployment it targets: after the counted
//! rollback the previous deployment boots with another digest and is judged normally.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::{CheckResult, names};
use crate::digest::Digest;

/// The hook file (below `--root` in tests).
pub const FORCE_RED_PATH: &str = "/var/lib/apex/force-red";

/// Largest hook file that is read. A digest is 71 bytes; anything much larger is not a
/// hook file.
pub const MAX_LEN: u64 = 4096;

/// What the hook file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookFile {
    /// There is no hook file.
    Absent,
    /// There is a file, but it is not used; the reason is for the journal.
    Ignored(String),
    /// The file's content with surrounding whitespace removed.
    Names(String),
}

/// Reads the hook file at `path`. Never fails: every problem makes the file
/// [`HookFile::Ignored`].
pub fn read(path: &Path) -> HookFile {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HookFile::Absent,
        Err(err) => return HookFile::Ignored(format!("cannot inspect {}: {err}", path.display())),
    };
    if !meta.file_type().is_file() {
        return HookFile::Ignored(format!("{} is not a regular file", path.display()));
    }
    if let Err(reason) = owner_only_writable(path, meta.permissions().mode()) {
        return HookFile::Ignored(reason);
    }
    if let Some(dir) = path.parent() {
        match fs::metadata(dir) {
            Ok(dir_meta) => {
                if let Err(reason) = owner_only_writable(dir, dir_meta.permissions().mode()) {
                    return HookFile::Ignored(reason);
                }
            }
            Err(err) => {
                return HookFile::Ignored(format!("cannot inspect {}: {err}", dir.display()));
            }
        }
    }
    let mut content = String::new();
    let read =
        fs::File::open(path).and_then(|file| file.take(MAX_LEN + 1).read_to_string(&mut content));
    match read {
        Err(err) => HookFile::Ignored(format!("cannot read {}: {err}", path.display())),
        Ok(len) if u64::try_from(len).unwrap_or(u64::MAX) > MAX_LEN => {
            HookFile::Ignored(format!("{} is larger than {MAX_LEN} bytes", path.display()))
        }
        Ok(_) => HookFile::Names(content.trim().to_owned()),
    }
}

fn owner_only_writable(path: &Path, mode: u32) -> Result<(), String> {
    if mode & 0o022 == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} is writable by group or others (mode {:o})",
            path.display(),
            mode & 0o7777
        ))
    }
}

/// The failed required check the hook adds for a boot of `booted`, if the file names
/// exactly that digest.
pub fn check(hook: &HookFile, booted: Option<&Digest>) -> Option<CheckResult> {
    match (hook, booted) {
        (HookFile::Names(named), Some(booted)) if named == booted.as_str() => {
            Some(CheckResult::fail(
                names::FORCED_RED,
                format!("{FORCE_RED_PATH} names the booted digest (qualification hook)"),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;

    const BOOTED: &str = "sha256:7777777777777777777777777777777777777777777777777777777777777777";
    const OTHER: &str = "sha256:8888888888888888888888888888888888888888888888888888888888888888";

    fn booted() -> Digest {
        Digest::parse(BOOTED).unwrap()
    }

    /// A private directory with the hook file holding `content`, mode 0600.
    fn hook_dir(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("force-red");
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        (dir, path)
    }

    #[test]
    fn absent_file_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let hook = read(&dir.path().join("force-red"));
        assert_eq!(hook, HookFile::Absent);
        assert_eq!(check(&hook, Some(&booted())), None);
    }

    #[test]
    fn the_booted_digest_forces_red() {
        for content in [
            BOOTED.to_owned(),
            format!("  {BOOTED}\n"),
            format!("{BOOTED}\r\n"),
        ] {
            let (_dir, path) = hook_dir(content.as_bytes());
            let hook = read(&path);
            assert_eq!(hook, HookFile::Names(BOOTED.to_owned()));
            let forced = check(&hook, Some(&booted())).unwrap();
            assert_eq!(forced.name, names::FORCED_RED);
            assert!(!forced.outcome.is_pass());
        }
    }

    #[test]
    fn another_digest_or_an_unknown_boot_changes_nothing() {
        for content in [
            OTHER,
            "",
            "sha256:77",
            "latest",
            BOOTED.to_uppercase().as_str(),
        ] {
            let (_dir, path) = hook_dir(content.as_bytes());
            assert_eq!(check(&read(&path), Some(&booted())), None, "{content:?}");
        }
        let (_dir, path) = hook_dir(BOOTED.as_bytes());
        assert_eq!(check(&read(&path), None), None);
        // Two digests on two lines are not "the booted digest".
        let (_dir, path) = hook_dir(format!("{OTHER}\n{BOOTED}\n").as_bytes());
        assert_eq!(check(&read(&path), Some(&booted())), None);
    }

    #[test]
    fn unsafe_or_odd_files_are_ignored() {
        // Writable by others.
        let (_dir, path) = hook_dir(BOOTED.as_bytes());
        fs::set_permissions(&path, Permissions::from_mode(0o646)).unwrap();
        assert!(matches!(read(&path), HookFile::Ignored(r) if r.contains("group or others")));
        // Its directory writable by the group.
        let (dir, path) = hook_dir(BOOTED.as_bytes());
        fs::set_permissions(dir.path(), Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(read(&path), HookFile::Ignored(r) if r.contains("group or others")));
        // A symlink to a good file.
        let (dir, target) = hook_dir(BOOTED.as_bytes());
        let link = dir.path().join("force-red-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(read(&link), HookFile::Ignored(r) if r.contains("regular file")));
        // A directory.
        let (dir, _) = hook_dir(b"");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(matches!(read(&sub), HookFile::Ignored(_)));
        // Too large.
        let (_dir, path) = hook_dir(&vec![b' '; 5000]);
        assert!(matches!(read(&path), HookFile::Ignored(r) if r.contains("larger")));
        // Not UTF-8.
        let (_dir, path) = hook_dir(b"sha256:\xff");
        assert!(matches!(read(&path), HookFile::Ignored(r) if r.contains("cannot read")));
        assert_eq!(
            check(&HookFile::Ignored(BOOTED.to_owned()), Some(&booted())),
            None
        );
    }

    #[test]
    fn a_fifo_is_ignored_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), Permissions::from_mode(0o700)).unwrap();
        let fifo = dir.path().join("force-red");
        let made = std::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&fifo)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("skipped: mkfifo not available");
            return;
        }
        // Opening a FIFO for reading would block until a writer appears.
        assert!(matches!(read(&fifo), HookFile::Ignored(r) if r.contains("regular file")));
    }
}
