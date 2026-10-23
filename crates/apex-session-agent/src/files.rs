use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::css;
use crate::paths::Paths;

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("cannot write {}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} leads to {}, outside the agent's config and state directories", .link.display(), .target.display())]
    Outside { link: PathBuf, target: PathBuf },
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

/// A symlinked `gtk.css`, as dotfile managers leave it, is edited where it points, and only
/// inside the agent's own directories.
fn resolve(path: &Path, paths: &Paths) -> Result<PathBuf, FileError> {
    let is_link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return Ok(path.to_owned());
    }
    let target = fs::canonicalize(path).map_err(io_error(path))?;
    let canonical = |home: &Path| fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
    let homes = Paths {
        config_home: canonical(&paths.config_home),
        state_home: canonical(&paths.state_home),
    };
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
    write_atomic(&paths.apex_css(), css::radius_css(radius).as_bytes(), 0o644)
}

/// Adds the import line once; the user's own text is kept byte for byte.
pub fn ensure_import(paths: &Paths) -> Result<bool, FileError> {
    let path = resolve(&paths.gtk_css(), paths)?;
    let (text, mode) = match fs::read(&path) {
        Ok(bytes) => {
            let mode = fs::metadata(&path).map_or(0o644, |m| m.permissions().mode() & 0o7777);
            (String::from_utf8_lossy(&bytes).into_owned(), mode)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), 0o644),
        Err(error) => return Err(io_error(&path)(error)),
    };
    match css::with_import(&text) {
        Some(edited) => write_atomic(&path, edited.as_bytes(), mode),
        None => Ok(false),
    }
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
        let paths = Paths {
            config_home: dir.path().join("config"),
            state_home: dir.path().join("state"),
        };
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
    fn removing_a_missing_marker_is_fine() {
        let (_dir, paths) = tree();
        remove_marker(&paths).unwrap();
        fs::create_dir_all(paths.state_dir()).unwrap();
        fs::write(paths.guard_marker(), "").unwrap();
        remove_marker(&paths).unwrap();
        assert!(!paths.guard_marker().exists());
    }
}
