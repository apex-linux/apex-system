//! Read only cached attributes: `/proc/driver/nvidia` or a device's `config` space would
//! wake the dGPU. The one exception is the ALC294's codec file ([`audio::snapshot`]).

pub mod audio;
pub mod idle;
pub mod machine;
pub mod report;
pub mod upower;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysRoot {
    root: PathBuf,
}

/// `Ok(None)`: the attribute does not exist.
pub type Attr = Result<Option<String>, String>;

impl SysRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn is_host(&self) -> bool {
        self.root == Path::new("/")
    }

    pub fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    pub fn read(&self, absolute: &str) -> Attr {
        read_trimmed(&self.path(absolute))
    }

    pub fn read_bytes(&self, absolute: &str) -> Result<Option<Vec<u8>>, String> {
        match fs::read(self.path(absolute)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn list(&self, absolute: &str) -> Vec<String> {
        list_dir(&self.path(absolute))
    }
}

pub fn read_trimmed(path: &Path) -> Attr {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).trim().to_owned())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

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

pub fn parse_hex(text: &str) -> Option<u32> {
    let digits = text.trim().trim_start_matches("0x");
    u32::from_str_radix(digits, 16).ok()
}
