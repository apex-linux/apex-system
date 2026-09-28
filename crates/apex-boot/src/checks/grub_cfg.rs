//! The installed `/boot/grub2/grub.cfg` and the boot counting snippet in it.
//!
//! bootupd writes the static GRUB config only at `bootc install`, so an image that ships a
//! new snippet does not change what GRUB runs. Without the snippet the counter never moves
//! and a broken update is never rolled back, which is why the snippet's presence is a
//! required check ([`GrubCfgMarkerCheck`]). Any version this image can drive
//! ([`MIN_COMPATIBLE_VERSION`] up to [`MARKER_VERSION`]) passes it; an older compatible
//! version is only reported ([`GrubCfgCurrentCheck`]). Otherwise the first image that ships
//! a new snippet version would fail its own assessment on every machine installed earlier,
//! and be rolled back and denylisted there, while the only repair (reinstalling the
//! snippet) has to come from that very image.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use crate::checks::{Check, names};
use crate::snippet::{MARKER, MARKER_PREFIX, MARKER_VERSION, MIN_COMPATIBLE_VERSION};

/// The GRUB config bootupd generates.
pub const GRUB_CFG_PATH: &str = "/boot/grub2/grub.cfg";

/// Why the snippet is not usable in a GRUB config.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkerError {
    /// No marker line at all.
    #[error("grub.cfg does not contain the Apex boot counting snippet ({MARKER:?})")]
    Missing,
    /// The snippet is a version this image cannot drive: older than
    /// [`MIN_COMPATIBLE_VERSION`], newer than [`MARKER_VERSION`] (installed by a later
    /// image), or not a version number at all.
    #[error("grub.cfg contains {found:?}; this image works with snippet versions {min} to {max}")]
    Unsupported {
        /// The marker line found.
        found: String,
        /// The oldest version the image drives.
        min: u32,
        /// The newest version the image drives (the one it ships).
        max: u32,
    },
    /// More than one copy of the snippet (of any version) is included, so every boot
    /// would be counted more than once.
    #[error("grub.cfg contains the Apex boot counting snippet {count} times")]
    Duplicated {
        /// How many marker lines there are.
        count: usize,
    },
    /// The snippet comes after `blscfg`, so it cannot choose between the boot entries
    /// reliably.
    #[error("the Apex boot counting snippet comes after blscfg in grub.cfg")]
    AfterBlscfg,
}

/// Checks the text of a GRUB config: exactly one snippet marker line (any line starting
/// with [`MARKER_PREFIX`], trailing whitespace and `\r` ignored) whose version this image
/// can drive ([`MIN_COMPATIBLE_VERSION`] to [`MARKER_VERSION`]), placed before the `blscfg`
/// command that creates the boot entries. Returns the version found.
pub fn scan(grub_cfg: &str) -> Result<u32, MarkerError> {
    scan_for(grub_cfg, MIN_COMPATIBLE_VERSION..=MARKER_VERSION)
}

/// [`scan`] for an image that drives the snippet versions in `supported`.
pub fn scan_for(grub_cfg: &str, supported: RangeInclusive<u32>) -> Result<u32, MarkerError> {
    let mut markers: Vec<(usize, &str)> = Vec::new();
    let mut blscfg_line = None;
    for (index, line) in grub_cfg.lines().enumerate() {
        let line = line.trim_end();
        if line.starts_with(MARKER_PREFIX) {
            markers.push((index, line));
        } else if line.trim_start() == "blscfg" {
            blscfg_line.get_or_insert(index);
        }
    }
    let (marker, found) = match markers.as_slice() {
        [] => return Err(MarkerError::Missing),
        [one] => *one,
        many => return Err(MarkerError::Duplicated { count: many.len() }),
    };
    let version = marker_version(found)
        .filter(|v| supported.contains(v))
        .ok_or_else(|| MarkerError::Unsupported {
            found: found.to_owned(),
            min: *supported.start(),
            max: *supported.end(),
        })?;
    match blscfg_line {
        Some(blscfg) if blscfg < marker => Err(MarkerError::AfterBlscfg),
        _ => Ok(version),
    }
}

/// The version of a marker line: the decimal number after [`MARKER_PREFIX`], in its one
/// canonical spelling.
fn marker_version(line: &str) -> Option<u32> {
    let digits = line.strip_prefix(MARKER_PREFIX)?;
    digits
        .parse::<u32>()
        .ok()
        .filter(|v| v.to_string() == digits)
}

fn read_cfg(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    // Menu titles may hold any bytes; the marker is ASCII, so a lossy view is enough.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The required check reading [`GRUB_CFG_PATH`] (`grub.cfg` is mode 0600, so this needs
/// root): the snippet is there once, before `blscfg`, in a version this image can drive.
#[derive(Debug, Clone)]
pub struct GrubCfgMarkerCheck {
    path: PathBuf,
}

impl GrubCfgMarkerCheck {
    /// The check on `/boot/grub2/grub.cfg`.
    pub fn system() -> Self {
        Self::new(GRUB_CFG_PATH)
    }

    /// The check on another file.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl Check for GrubCfgMarkerCheck {
    fn name(&self) -> &str {
        names::GRUB_CFG_MARKER
    }

    fn run(&self) -> Result<(), String> {
        scan(&read_cfg(&self.path)?)
            .map(drop)
            .map_err(|err| err.to_string())
    }
}

/// The reported check [`names::GRUB_CFG_CURRENT`]: the snippet in `grub.cfg` is the
/// version this image ships. An older, still compatible one works but misses the newer
/// behaviour; `apexctl boot repair-grub` (M2) reinstalls it.
#[derive(Debug, Clone)]
pub struct GrubCfgCurrentCheck {
    path: PathBuf,
}

impl GrubCfgCurrentCheck {
    /// The check on another file.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl Check for GrubCfgCurrentCheck {
    fn name(&self) -> &str {
        names::GRUB_CFG_CURRENT
    }

    fn run(&self) -> Result<(), String> {
        current(scan(&read_cfg(&self.path)?), MARKER_VERSION)
    }
}

/// The reported verdict on a scan result for an image that ships version `shipped`.
fn current(scanned: Result<u32, MarkerError>, shipped: u32) -> Result<(), String> {
    match scanned {
        Ok(found) if found == shipped => Ok(()),
        Ok(found) => Err(format!(
            "grub.cfg has boot counting snippet v{found}, this image ships v{shipped}; \
             reinstall it with `apexctl boot repair-grub`"
        )),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snippet::SNIPPET;

    /// What bootupd generates: the pre config, then each fragment wrapped in markers.
    fn bootupd_cfg(fragments: &[(&str, &str)]) -> String {
        use std::fmt::Write as _;
        let mut cfg = String::from("# Generated by bootupd / do not edit\n\nset timeout=1\n");
        for (name, body) in fragments {
            write!(cfg, "\n### BEGIN {name} ###\n{body}### END {name} ###\n").unwrap();
        }
        cfg
    }

    /// A snippet of another version, as an older or newer image would have installed it.
    fn snippet_version(version: &str) -> String {
        SNIPPET.replacen(MARKER, &format!("{MARKER_PREFIX}{version}"), 1)
    }

    #[test]
    fn accepts_the_bootupd_layout() {
        let cfg = bootupd_cfg(&[
            ("01_users.cfg", "# users\n"),
            ("08_apex-bootcount.cfg", SNIPPET),
            ("10_blscfg.cfg", "blscfg\n"),
            (
                "30_uefi-firmware.cfg",
                "menuentry 'UEFI Firmware Settings' {\n  fwsetup\n}\n",
            ),
        ]);
        assert_eq!(scan(&cfg), Ok(MARKER_VERSION));
        assert_eq!(scan(&cfg.replace('\n', "\r\n")), Ok(MARKER_VERSION));
    }

    #[test]
    fn missing_snippet() {
        let cfg = bootupd_cfg(&[("10_blscfg.cfg", "blscfg\n")]);
        assert_eq!(scan(&cfg), Err(MarkerError::Missing));
        assert_eq!(scan(""), Err(MarkerError::Missing));
        // Indented or commented differently is not the marker.
        assert_eq!(scan("  # apex-bootcount v1\n"), Err(MarkerError::Missing));
    }

    #[test]
    fn unsupported_versions() {
        for version in ["0", "999", "x", "01", "1.1", ""] {
            let body = snippet_version(version);
            let cfg = bootupd_cfg(&[
                ("08_apex-bootcount.cfg", body.as_str()),
                ("10_blscfg.cfg", "blscfg\n"),
            ]);
            assert_eq!(
                scan(&cfg),
                Err(MarkerError::Unsupported {
                    found: format!("{MARKER_PREFIX}{version}"),
                    min: MIN_COMPATIBLE_VERSION,
                    max: MARKER_VERSION,
                }),
                "{version:?}"
            );
        }
        // An older copy next to the current one would count every boot twice.
        let both = bootupd_cfg(&[
            ("07_old.cfg", "# apex-bootcount v0\n"),
            ("08_apex-bootcount.cfg", SNIPPET),
            ("10_blscfg.cfg", "blscfg\n"),
        ]);
        assert_eq!(scan(&both), Err(MarkerError::Duplicated { count: 2 }));
    }

    /// Finding: an image that bumps the snippet version must not fail its own required
    /// check on machines that still run an older, compatible snippet. Here a v2 image
    /// that still drives v1 meets a machine installed with v1: the required check passes
    /// and only the reported one asks for a reinstall.
    #[test]
    fn an_older_compatible_snippet_is_reported_not_required() {
        let installed = bootupd_cfg(&[
            ("08_apex-bootcount.cfg", SNIPPET),
            ("10_blscfg.cfg", "blscfg\n"),
        ]);
        let found = scan_for(&installed, 1..=2);
        assert_eq!(found, Ok(1));
        assert_eq!(
            current(found, 2),
            Err(
                "grub.cfg has boot counting snippet v1, this image ships v2; \
                 reinstall it with `apexctl boot repair-grub`"
                    .to_owned()
            )
        );
        // An image that dropped v1 on purpose fails the required check.
        assert!(matches!(
            scan_for(&installed, 2..=2),
            Err(MarkerError::Unsupported { min: 2, max: 2, .. })
        ));
        // Today's image drives every version it declares.
        for version in MIN_COMPATIBLE_VERSION..=MARKER_VERSION {
            let body = snippet_version(&version.to_string());
            let cfg = bootupd_cfg(&[
                ("08_apex-bootcount.cfg", body.as_str()),
                ("10_blscfg.cfg", "blscfg\n"),
            ]);
            assert_eq!(scan(&cfg), Ok(version));
            assert_eq!(current(scan(&cfg), version), Ok(()));
        }
    }

    #[test]
    fn duplicated_snippet() {
        let cfg = bootupd_cfg(&[
            ("08_apex-bootcount.cfg", SNIPPET),
            ("09_copy.cfg", SNIPPET),
            ("10_blscfg.cfg", "blscfg\n"),
        ]);
        assert_eq!(scan(&cfg), Err(MarkerError::Duplicated { count: 2 }));
    }

    #[test]
    fn snippet_after_blscfg() {
        let cfg = bootupd_cfg(&[
            ("10_blscfg.cfg", "blscfg\n"),
            ("50_apex-bootcount.cfg", SNIPPET),
        ]);
        assert_eq!(scan(&cfg), Err(MarkerError::AfterBlscfg));
        // `insmod blscfg` or a comment is not the command.
        let cfg = format!("insmod blscfg\n# blscfg\n{SNIPPET}blscfg\n");
        assert_eq!(scan(&cfg), Ok(MARKER_VERSION));
    }

    #[test]
    fn check_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grub.cfg");
        let check = GrubCfgMarkerCheck::new(&path);
        let current = GrubCfgCurrentCheck::new(&path);
        assert_eq!(check.name(), "grub-cfg-marker");
        assert_eq!(current.name(), "grub-cfg-current");
        assert!(check.run().unwrap_err().starts_with("cannot read"));
        assert!(current.run().unwrap_err().starts_with("cannot read"));
        std::fs::write(&path, b"\xff\xfe menuentry\nblscfg\n").unwrap();
        assert!(check.run().unwrap_err().contains("does not contain"));
        assert!(current.run().unwrap_err().contains("does not contain"));
        let mut cfg = SNIPPET.as_bytes().to_vec();
        cfg.extend_from_slice(b"menuentry '\xff' {\n}\nblscfg\n");
        std::fs::write(&path, cfg).unwrap();
        assert_eq!(check.run(), Ok(()));
        assert_eq!(current.run(), Ok(()));
    }
}
