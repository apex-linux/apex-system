use std::fs::File;
use std::io;
use std::path::Path;

use nix::fcntl::{FcntlArg, fcntl};
use nix::libc;

pub const SYSROOT_LOCK_PATH: &str = "/sysroot/ostree/lock";

/// ostree holds an exclusive OFD lock on this file (libglnx `glnx_make_lock_file`), and
/// `F_OFD_GETLK` reports a conflicting lock without taking one, so probing never delays bootc.
pub fn sysroot_lock_held(path: &Path) -> io::Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let unlocked = i16::try_from(libc::F_UNLCK).map_err(io::Error::other)?;
    let mut query = libc::flock {
        l_type: i16::try_from(libc::F_WRLCK).map_err(io::Error::other)?,
        l_whence: i16::try_from(libc::SEEK_SET).map_err(io::Error::other)?,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    fcntl(&file, FcntlArg::F_OFD_GETLK(&mut query))?;
    Ok(query.l_type != unlocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn ofd_lock(file: &File, l_type: libc::c_int) -> nix::Result<libc::c_int> {
        let lock = libc::flock {
            l_type: i16::try_from(l_type).unwrap(),
            l_whence: i16::try_from(libc::SEEK_SET).unwrap(),
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        fcntl(file, FcntlArg::F_OFD_SETLK(&lock))
    }

    #[test]
    fn missing_lock_file_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!sysroot_lock_held(&dir.path().join("ostree/lock")).unwrap());
    }

    #[test]
    fn detects_an_ofd_write_lock_held_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(!sysroot_lock_held(&path).unwrap());
        ofd_lock(&holder, libc::F_WRLCK).unwrap();
        assert!(sysroot_lock_held(&path).unwrap());
        ofd_lock(&holder, libc::F_UNLCK).unwrap();
        assert!(!sysroot_lock_held(&path).unwrap());
        ofd_lock(&holder, libc::F_WRLCK).unwrap();
        drop(holder);
        assert!(
            !sysroot_lock_held(&path).unwrap(),
            "closing the holder releases the lock"
        );
    }

    #[test]
    fn a_shared_lock_also_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        std::fs::write(&path, b"").unwrap();
        let holder = File::open(&path).unwrap();
        ofd_lock(&holder, libc::F_RDLCK).unwrap();
        assert!(sysroot_lock_held(&path).unwrap());
    }
}
