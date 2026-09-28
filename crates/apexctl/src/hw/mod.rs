//! Read-only hardware diagnostics: `apexctl hw report` and `apexctl hw audio snapshot`.
//!
//! Everything reads sysfs, procfs or efivarfs attributes through a [`SysRoot`], so the
//! tests run against a fake tree in a temporary directory. Nothing here writes, and
//! nothing reads `/proc/driver/nvidia` (opening it would wake the dGPU) or a device's
//! `config` space. `hw report` reads only cached attributes that do not resume a
//! runtime-suspended device. `hw audio snapshot` reads one file that does: the ALC294's
//! `/proc/asound/card*/codec#*`, which powers that codec and its controller (on the
//! M7400QC the APU's HD Audio controller, not a GPU's audio function) up while the kernel
//! prints it. It picks that file by the
//! hwdep's cached ids and opens no other codec's file ([`audio::snapshot`]).

pub mod audio;
pub mod machine;
pub mod report;
pub mod upower;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The root that `/sys`, `/proc` and `/usr` are resolved under (`/` on the laptop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysRoot {
    root: PathBuf,
}

/// An attribute read: `Ok(None)` when it does not exist, `Err` with the reason when it
/// exists but cannot be read.
pub type Attr = Result<Option<String>, String>;

impl SysRoot {
    /// A root prefix.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `true` for the running system (root `/`), where D-Bus queries make sense.
    pub fn is_host(&self) -> bool {
        self.root == Path::new("/")
    }

    /// `absolute` (e.g. `/sys/class/sound`) under the root.
    pub fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    /// Reads a text attribute, trimmed.
    pub fn read(&self, absolute: &str) -> Attr {
        read_trimmed(&self.path(absolute))
    }

    /// Reads raw bytes (efivars).
    pub fn read_bytes(&self, absolute: &str) -> Result<Option<Vec<u8>>, String> {
        match fs::read(self.path(absolute)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Names of the entries of a directory, sorted; empty when it does not exist.
    pub fn list(&self, absolute: &str) -> Vec<String> {
        list_dir(&self.path(absolute))
    }
}

/// Reads a file as trimmed text; `Ok(None)` when missing.
pub fn read_trimmed(path: &Path) -> Attr {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).trim().to_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Sorted entry names of `dir`; empty when it cannot be listed.
pub fn list_dir(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Parses a sysfs hex attribute such as `0x10ec0294` or `04f3`.
pub fn parse_hex(text: &str) -> Option<u32> {
    let digits = text.trim().trim_start_matches("0x");
    u32::from_str_radix(digits, 16).ok()
}
