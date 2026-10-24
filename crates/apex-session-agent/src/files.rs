use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::css;
use crate::palette;
use crate::paths::Paths;

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("cannot write {}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} leads to {}, outside the agent's config, data and state directories", .link.display(), .target.display())]
    Outside { link: PathBuf, target: PathBuf },
    #[error("{} is not writable by its owner, so it is left as it is", .path.display())]
    ReadOnly { path: PathBuf },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> FileError + '_ {
    move |source| FileError::Io {
        path: path.to_owned(),
        source,
    }
}

/// A temporary file in the same directory, synced, then renamed over `path`: readers see
/// the old or the new file, never a part. Returns whether the content changed.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<bool, FileError> {
    if fs::read(path).is_ok_and(|old| old == contents) {
        return Ok(false);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io_error(dir))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let written = (|| {
        let _ = fs::remove_file(&tmp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        file.set_permissions(Permissions::from_mode(mode))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.map(|()| true).map_err(io_error(path))
}

/// The deepest part of `path` that exists, resolved, with the rest appended.
fn resolve_existing(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut base = path;
    loop {
        match fs::canonicalize(base) {
            Ok(real) => return Ok(missing.iter().rev().fold(real, |p, part| p.join(part))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (base.parent(), base.file_name()) else {
                    return Err(error);
                };
                missing.push(name.to_owned());
                base = parent;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Dotfile managers link `gtk.css` or the whole `gtk-4.0` directory. The agent writes where
/// a link leads, but only inside the canonical config or state home.
pub fn contained(path: &Path, paths: &Paths) -> Result<PathBuf, FileError> {
    let target = resolve_existing(path).map_err(io_error(path))?;
    let canonical = |home: &Path| resolve_existing(home).unwrap_or_else(|_| home.to_owned());
    let homes = Paths::new(
        canonical(&paths.config_home),
        canonical(&paths.data_home),
        canonical(&paths.state_home),
    );
    if homes.is_ours(&target) {
        Ok(target)
    } else {
        Err(FileError::Outside {
            link: path.to_owned(),
            target,
        })
    }
}

pub fn write_apex_css(paths: &Paths, radius: u32) -> Result<bool, FileError> {
    let path = contained(&paths.apex_css(), paths)?;
    write_atomic(&path, css::radius_css(radius).as_bytes(), 0o644)
}

pub fn write_ptyxis_palette(paths: &Paths, accent: &str) -> Result<bool, FileError> {
    let path = contained(&paths.ptyxis_palette(), paths)?;
    write_atomic(&path, palette::palette(accent).as_bytes(), 0o644)
}

/// Adds the import line once; every byte of the user's own file is kept.
pub fn ensure_import(paths: &Paths) -> Result<bool, FileError> {
    let path = contained(&paths.gtk_css(), paths)?;
    let (bytes, mode) = match fs::read(&path) {
        Ok(bytes) => {
            let mode = fs::metadata(&path).map_or(0o644, |m| m.permissions().mode() & 0o7777);
            (bytes, mode)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (Vec::new(), 0o644),
        Err(error) => return Err(io_error(&path)(error)),
    };
    let Some(edited) = css::with_import(&bytes) else {
        return Ok(false);
    };
    if mode & 0o200 == 0 {
        return Err(FileError::ReadOnly { path });
    }
    write_atomic(&path, &edited, mode)
}

pub fn write_notified(paths: &Paths, digest: &str) -> Result<bool, FileError> {
    let path = contained(&paths.updates_notified(), paths)?;
    write_atomic(&path, format!("{digest}\n").as_bytes(), 0o644)
}

/// An unreadable or odd file counts as none: the worst case is one more notification.
pub fn read_notified(paths: &Paths) -> Option<String> {
    let path = contained(&paths.updates_notified(), paths).ok()?;
    let text = fs::read_to_string(path).ok()?;
    let digest = text.strip_suffix('\n')?;
    let valid = !digest.is_empty() && !digest.contains(char::is_whitespace);
    valid.then(|| digest.to_owned())
}

pub fn remove_marker(paths: &Paths) -> Result<(), FileError> {
    let marker = paths.guard_marker();
    match fs::remove_file(&marker) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_error(&marker)(error)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tree() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            dir.path().join("config"),
            dir.path().join("data"),
            dir.path().join("state"),
        );
        (dir, paths)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writes_create_the_directory_and_leave_no_temporary_file() {
        let (_dir, paths) = tree();
        assert!(write_apex_css(&paths, 9).unwrap());
        let text = fs::read_to_string(paths.apex_css()).unwrap();
        assert!(text.contains("--window-radius: 9px;"));
        let names: Vec<_> = fs::read_dir(paths.gtk_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["apex.css"]);
        assert!(
            !write_apex_css(&paths, 9).unwrap(),
            "unchanged content is not rewritten"
        );
        assert!(write_apex_css(&paths, 10).unwrap());
    }

    #[test]
    fn the_import_is_added_once_and_keeps_the_mode() {
        let (_dir, paths) = tree();
        fs::create_dir_all(paths.gtk_dir()).unwrap();
        fs::write(paths.gtk_css(), "label { color: red; }\r\n").unwrap();
        fs::set_permissions(paths.gtk_css(), Permissions::from_mode(0o600)).unwrap();
        assert!(ensure_import(&paths).unwrap());
        assert!(!ensure_import(&paths).unwrap());
        assert_eq!(
            fs::read_to_string(paths.gtk_css()).unwrap(),
            "@import url(\"apex.css\");\r\nlabel { color: red; }\r\n"
        );
        assert_eq!(mode(&paths.gtk_css()), 0o600);
    }

    #[test]
    fn a_missing_gtk_css_is_created() {
        let (_dir, paths) = tree();
        assert!(ensure_import(&paths).unwrap());
        assert_eq!(
            fs::read_to_string(paths.gtk_css()).unwrap(),
            "@import url(\"apex.css\");\n"
        );
    }

    #[test]
    fn a_symlink_is_followed_only_inside_our_directories() {
        let (dir, paths) = tree();
        fs::create_dir_all(paths.gtk_dir()).unwrap();
        let inside = paths.config_home.join("dotfiles-gtk.css");
        fs::write(&inside, "window {}\n").unwrap();
        symlink(&inside, paths.gtk_css()).unwrap();
        assert!(ensure_import(&paths).unwrap());
        assert!(
            fs::symlink_metadata(paths.gtk_css())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::read_to_string(&inside).unwrap().starts_with("@import"));

        let outside = dir.path().join("elsewhere.css");
        fs::write(&outside, "window {}\n").unwrap();
        fs::remove_file(paths.gtk_css()).unwrap();
        symlink(&outside, paths.gtk_css()).unwrap();
        assert!(matches!(
            ensure_import(&paths),
            Err(FileError::Outside { .. })
        ));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "window {}\n");
    }

    #[test]
    fn a_linked_gtk_directory_outside_our_homes_is_refused_for_both_files() {
        let (dir, paths) = tree();
        let dotfiles = dir.path().join("dotfiles/gtk-4.0");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::write(dotfiles.join("gtk.css"), "window {}\n").unwrap();
        fs::create_dir_all(&paths.config_home).unwrap();
        symlink(&dotfiles, paths.gtk_dir()).unwrap();
        assert!(matches!(
            write_apex_css(&paths, 9),
            Err(FileError::Outside { .. })
        ));
        assert!(matches!(
            ensure_import(&paths),
            Err(FileError::Outside { .. })
        ));
        assert!(!dotfiles.join("apex.css").exists());
        assert_eq!(
            fs::read_to_string(dotfiles.join("gtk.css")).unwrap(),
            "window {}\n"
        );
    }

    #[test]
    fn a_linked_gtk_directory_inside_our_homes_is_followed() {
        let (_dir, paths) = tree();
        let real = paths.config_home.join("dotfiles/gtk-4.0");
        fs::create_dir_all(&real).unwrap();
        symlink(&real, paths.gtk_dir()).unwrap();
        assert!(write_apex_css(&paths, 9).unwrap());
        assert!(ensure_import(&paths).unwrap());
        assert!(real.join("apex.css").exists());
        assert!(real.join("gtk.css").exists());
    }

    #[test]
    fn a_gtk_css_without_owner_write_is_left_alone() {
        let (_dir, paths) = tree();
        fs::create_dir_all(paths.gtk_dir()).unwrap();
        fs::write(paths.gtk_css(), "window {}\n").unwrap();
        fs::set_permissions(paths.gtk_css(), Permissions::from_mode(0o444)).unwrap();
        assert!(matches!(
            ensure_import(&paths),
            Err(FileError::ReadOnly { .. })
        ));
        assert_eq!(fs::read_to_string(paths.gtk_css()).unwrap(), "window {}\n");

        fs::set_permissions(paths.gtk_css(), Permissions::from_mode(0o644)).unwrap();
        fs::write(paths.gtk_css(), "@import url(\"apex.css\");\n").unwrap();
        fs::set_permissions(paths.gtk_css(), Permissions::from_mode(0o444)).unwrap();
        assert!(
            !ensure_import(&paths).unwrap(),
            "nothing to add is no error"
        );
    }

    #[test]
    fn a_gtk_css_in_another_encoding_keeps_its_bytes() {
        let (_dir, paths) = tree();
        fs::create_dir_all(paths.gtk_dir()).unwrap();
        let latin1 = b"/* caf\xe9 */\nwindow {}\n";
        fs::write(paths.gtk_css(), latin1).unwrap();
        assert!(ensure_import(&paths).unwrap());
        let mut expected = b"@import url(\"apex.css\");\n".to_vec();
        expected.extend_from_slice(latin1);
        assert_eq!(fs::read(paths.gtk_css()).unwrap(), expected);
    }

    #[test]
    fn the_palette_is_written_where_ptyxis_looks_and_follows_the_accent() {
        let (_dir, paths) = tree();
        assert!(write_ptyxis_palette(&paths, "green").unwrap());
        assert!(!write_ptyxis_palette(&paths, "green").unwrap());
        let text = fs::read_to_string(paths.ptyxis_palette()).unwrap();
        assert!(text.contains("Cursor=#3a944a\n"));
        assert_eq!(mode(&paths.ptyxis_palette()), 0o644);
        assert!(write_ptyxis_palette(&paths, "purple").unwrap());
        assert!(
            fs::read_to_string(paths.ptyxis_palette())
                .unwrap()
                .contains("Cursor=#9141ac\n")
        );
    }

    #[test]
    fn a_linked_palette_directory_outside_our_homes_is_refused() {
        let (dir, paths) = tree();
        let dotfiles = dir.path().join("dotfiles/palettes");
        fs::create_dir_all(&dotfiles).unwrap();
        let ptyxis = paths.data_home.join("org.gnome.Ptyxis");
        fs::create_dir_all(&ptyxis).unwrap();
        symlink(&dotfiles, ptyxis.join("palettes")).unwrap();
        assert!(matches!(
            write_ptyxis_palette(&paths, "blue"),
            Err(FileError::Outside { .. })
        ));
        assert!(!dotfiles.join("apex.palette").exists());
    }

    #[test]
    fn the_notified_digest_round_trips() {
        let (dir, paths) = tree();
        assert_eq!(read_notified(&paths), None);
        let digest = format!("sha256:{}", "b".repeat(64));
        assert!(write_notified(&paths, &digest).unwrap());
        assert!(!write_notified(&paths, &digest).unwrap());
        assert_eq!(read_notified(&paths), Some(digest));
        assert_eq!(mode(&paths.updates_notified()), 0o644);
        for odd in ["", "\n", "two words\n", "no newline"] {
            fs::write(paths.updates_notified(), odd).unwrap();
            assert_eq!(read_notified(&paths), None, "{odd:?}");
        }

        let outside = dir.path().join("elsewhere");
        fs::write(&outside, "sha256:x\n").unwrap();
        fs::remove_file(paths.updates_notified()).unwrap();
        symlink(&outside, paths.updates_notified()).unwrap();
        assert_eq!(read_notified(&paths), None);
        assert!(matches!(
            write_notified(&paths, "sha256:y"),
            Err(FileError::Outside { .. })
        ));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "sha256:x\n");
    }

    #[test]
    fn removing_a_missing_marker_is_fine() {
        let (_dir, paths) = tree();
        remove_marker(&paths).unwrap();
        fs::create_dir_all(paths.marker_dir()).unwrap();
        fs::write(paths.guard_marker(), "").unwrap();
        remove_marker(&paths).unwrap();
        assert!(!paths.guard_marker().exists());
    }
}
