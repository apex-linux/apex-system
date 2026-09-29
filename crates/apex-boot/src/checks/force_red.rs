//! The file names one digest, so the qualification hook cannot outlive the deployment it targets.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::{CheckResult, names};
use crate::digest::Digest;

pub const FORCE_RED_PATH: &str = "/var/lib/apex/force-red";

/// A digest is 71 bytes; anything much larger is not a hook file.
pub const MAX_LEN: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookFile {
    Absent,
    Ignored(String),
    Names(String),
}

/// Only root can create the file in `/var/lib/apex` (0700); the mode checks keep that true
/// if the directory is ever misconfigured.
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
        let (_dir, path) = hook_dir(format!("{OTHER}\n{BOOTED}\n").as_bytes());
        assert_eq!(check(&read(&path), Some(&booted())), None);
    }

    #[test]
    fn unsafe_or_odd_files_are_ignored() {
        let (_dir, path) = hook_dir(BOOTED.as_bytes());
        fs::set_permissions(&path, Permissions::from_mode(0o646)).unwrap();
        assert!(matches!(read(&path), HookFile::Ignored(r) if r.contains("group or others")));
        let (dir, path) = hook_dir(BOOTED.as_bytes());
        fs::set_permissions(dir.path(), Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(read(&path), HookFile::Ignored(r) if r.contains("group or others")));
        let (dir, target) = hook_dir(BOOTED.as_bytes());
        let link = dir.path().join("force-red-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(read(&link), HookFile::Ignored(r) if r.contains("regular file")));
        let (dir, _) = hook_dir(b"");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(matches!(read(&sub), HookFile::Ignored(_)));
        let (_dir, path) = hook_dir(&vec![b' '; 5000]);
        assert!(matches!(read(&path), HookFile::Ignored(r) if r.contains("larger")));
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
        assert!(matches!(read(&fifo), HookFile::Ignored(r) if r.contains("regular file")));
    }
}
