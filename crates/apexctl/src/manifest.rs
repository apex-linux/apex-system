//! `apexctl build manifest`: writes `/usr/share/apex/manifest.json` (contract §3) in the
//! final image stage.
//!
//! The package list comes from `rpm --root DIR -qa --qf` with [`RPM_QUERY_FORMAT`].
//! `%{EPOCHNUM}` is rpm's extension tag that prints `0` for a package without an epoch
//! (`RPMTAG_EPOCHNUM` = 5019 in `include/rpm/rpmtag.h`, still present in rpm 6.1), so no
//! `(none)` has to be interpreted. The `gpg-pubkey` pseudo packages that an rpmdb keystore
//! lists are signing keys, not software, and are skipped.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use apex_update::manifest::{MANIFEST_PATH, MANIFEST_SCHEMA};
use apex_update::{Manifest, Package, Version};

use crate::render::{OutputError, write_atomic};

/// The `--queryformat` passed to rpm: one tab-separated line per package.
pub const RPM_QUERY_FORMAT: &str = "%{NAME}\\t%{EPOCHNUM}\\t%{VERSION}\\t%{RELEASE}\\t%{ARCH}\\n";

/// Image name recorded in the manifest.
pub const IMAGE_NAME: &str = "apex";

/// Components the `degraded` list may name (contract §3).
pub const DEGRADED_COMPONENTS: &[&str] = &["audio", "fingerprint", "nvidia"];

/// Name of the rpm pseudo packages that hold imported signing keys.
const GPG_PUBKEY: &str = "gpg-pubkey";

/// Why a manifest could not be built.
#[derive(Debug, thiserror::Error)]
pub enum ManifestBuildError {
    /// The version is not `<fedora>.<yyyymmdd>.<run>`.
    #[error(
        "version {0:?} is not <fedora>.<yyyymmdd>.<run> (e.g. 45.20260924.117) in canonical form"
    )]
    Version(String),
    /// The revision is not a full lowercase git commit id.
    #[error("revision {0:?} is not a full lowercase git commit id (40 or 64 hex digits)")]
    Revision(String),
    /// The base image is not pinned by digest.
    #[error("base image {0:?} is not pinned by digest (<repository>@sha256:<64 hex digits>)")]
    Base(String),
    /// A `--degraded` value outside the contract's list.
    #[error("unknown degraded component {0:?} (known: audio, fingerprint, nvidia)")]
    Degraded(String),
    /// An rpm output line that does not have five fields.
    #[error("rpm output line {line}: {reason}: {text:?}")]
    RpmLine {
        /// Line number (from 1).
        line: usize,
        /// What is wrong.
        reason: &'static str,
        /// The line.
        text: String,
    },
    /// rpm listed no package at all.
    #[error("rpm listed no packages under {0}")]
    NoPackages(String),
    /// Running rpm failed.
    #[error("cannot run rpm: {0}")]
    RpmSpawn(std::io::Error),
    /// rpm exited with an error.
    #[error("rpm failed ({status}): {stderr}")]
    RpmFailed {
        /// Exit status.
        status: String,
        /// Its stderr, trimmed.
        stderr: String,
    },
    /// Serializing failed. The cause is part of the message, so it is not also the
    /// `source()` (no `#[from]`): `apexctl` prints the whole chain.
    #[error("cannot serialize the manifest: {0}")]
    Encode(apex_update::ManifestError),
    /// Writing failed.
    #[error(transparent)]
    Write(#[from] OutputError),
}

/// The command-line inputs of the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInput {
    /// `--version`, e.g. `45.20260924.117`.
    pub version: String,
    /// `--revision`: the git commit of the Apex repository.
    pub revision: String,
    /// `--base`: the digest-pinned base image.
    pub base: String,
    /// `--degraded` values, in any order.
    pub degraded: Vec<String>,
}

/// Validates a version string: exactly three numeric fields, canonical (no leading zeros),
/// an 8-digit date with a plausible month and day.
pub fn validate_version(text: &str) -> Result<Version, ManifestBuildError> {
    let error = || ManifestBuildError::Version(text.to_owned());
    let version = Version::parse(text).ok_or_else(error)?;
    let date = version.date();
    let (month, day) = ((date / 100) % 100, date % 100);
    let canonical = version.to_string() == text;
    if version.fields().len() != 3
        || !canonical
        || !(10_000_000..=99_999_999).contains(&date)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return Err(error());
    }
    Ok(version)
}

fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_revision(text: &str) -> Result<(), ManifestBuildError> {
    if matches!(text.len(), 40 | 64) && is_lower_hex(text) {
        Ok(())
    } else {
        Err(ManifestBuildError::Revision(text.to_owned()))
    }
}

fn validate_base(text: &str) -> Result<(), ManifestBuildError> {
    let valid = text
        .split_once("@sha256:")
        .is_some_and(|(repository, hex)| {
            !repository.is_empty()
                && !repository.contains(char::is_whitespace)
                && hex.len() == 64
                && is_lower_hex(hex)
        });
    if valid {
        Ok(())
    } else {
        Err(ManifestBuildError::Base(text.to_owned()))
    }
}

/// Parses rpm's output for [`RPM_QUERY_FORMAT`], skipping `gpg-pubkey`.
pub fn parse_rpm_output(output: &str) -> Result<Vec<Package>, ManifestBuildError> {
    let mut packages = Vec::new();
    for (index, text) in output.lines().enumerate() {
        if text.is_empty() {
            continue;
        }
        let bad = |reason| ManifestBuildError::RpmLine {
            line: index + 1,
            reason,
            text: text.to_owned(),
        };
        let fields: Vec<&str> = text.split('\t').collect();
        let [name, epoch, version, release, arch] = fields.as_slice() else {
            return Err(bad("expected 5 tab-separated fields"));
        };
        if *name == GPG_PUBKEY {
            continue;
        }
        if [name, version, release, arch]
            .iter()
            .any(|field| field.is_empty() || **field == "(none)")
        {
            return Err(bad("empty or (none) field"));
        }
        let epoch: u32 = epoch.parse().map_err(|_| bad("epoch is not a number"))?;
        packages.push(Package {
            name: (*name).to_owned(),
            epoch,
            version: (*version).to_owned(),
            release: (*release).to_owned(),
            arch: (*arch).to_owned(),
        });
    }
    Ok(packages)
}

/// A manifest plus the `(name, arch)` keys that appeared more than once (the newest EVR
/// is kept; the caller warns about the rest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltManifest {
    /// The normalized manifest.
    pub manifest: Manifest,
    /// Keys listed more than once, sorted.
    pub duplicates: Vec<(String, String)>,
}

/// Builds the manifest from validated inputs and the package list (pure).
pub fn build(
    input: &ManifestInput,
    packages: Vec<Package>,
) -> Result<BuiltManifest, ManifestBuildError> {
    let version = validate_version(&input.version)?;
    validate_revision(&input.revision)?;
    validate_base(&input.base)?;
    for component in &input.degraded {
        if !DEGRADED_COMPONENTS.contains(&component.as_str()) {
            return Err(ManifestBuildError::Degraded(component.clone()));
        }
    }
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for package in &packages {
        if !seen.insert(package.key()) {
            duplicates.insert((package.name.clone(), package.arch.clone()));
        }
    }
    let mut manifest = Manifest {
        schema: MANIFEST_SCHEMA,
        name: IMAGE_NAME.to_owned(),
        version: input.version.clone(),
        // A version has at least three fields, so fedora() cannot fail; the release fits
        // u32 for every Fedora there will be.
        fedora: u32::try_from(version.fedora())
            .map_err(|_| ManifestBuildError::Version(input.version.clone()))?,
        revision: input.revision.clone(),
        base_image: input.base.clone(),
        degraded: input.degraded.clone(),
        packages,
    };
    manifest.normalize();
    Ok(BuiltManifest {
        manifest,
        duplicates: duplicates.into_iter().collect(),
    })
}

/// Runs `rpm --root ROOT -qa --qf …` and parses its output.
pub fn query_rpm(root: &Path) -> Result<Vec<Package>, ManifestBuildError> {
    let output = Command::new("rpm")
        .arg("--root")
        .arg(root)
        .args(["-qa", "--qf", RPM_QUERY_FORMAT])
        .env("LC_ALL", "C.UTF-8")
        .output()
        .map_err(ManifestBuildError::RpmSpawn)?;
    if !output.status.success() {
        return Err(ManifestBuildError::RpmFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let packages = parse_rpm_output(&String::from_utf8_lossy(&output.stdout))?;
    if packages.is_empty() {
        return Err(ManifestBuildError::NoPackages(root.display().to_string()));
    }
    Ok(packages)
}

/// `ROOT/usr/share/apex/manifest.json`.
pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST_PATH.trim_start_matches('/'))
}

/// Writes the manifest atomically with mode 0644 (readable by the unprivileged Updates
/// app), creating `/usr/share/apex` when needed. Returns the path written.
pub fn write(root: &Path, manifest: &Manifest) -> Result<PathBuf, ManifestBuildError> {
    let path = manifest_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| OutputError {
            action: "create",
            path: dir.to_path_buf(),
            error,
        })?;
    }
    let json = manifest.to_json().map_err(ManifestBuildError::Encode)?;
    write_atomic(&path, &json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RPM_SAMPLE: &str = "\
zsh\t0\t5.9\t14.fc45\tx86_64
bash\t0\t5.3.0\t2.fc45\tx86_64
gpg-pubkey\t0\tf577861e\t6602e6d4\t(none)
glibc\t0\t2.42\t4.fc45\ti686
glibc\t0\t2.42\t4.fc45\tx86_64
NetworkManager\t1\t1.54.0\t1.fc45\tx86_64
kernel-core\t0\t7.2.7\t300.fc45\tx86_64
kernel-core\t0\t7.2.9\t300.fc45\tx86_64
";

    fn input() -> ManifestInput {
        ManifestInput {
            version: "45.20260924.117".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            base: format!("quay.io/fedora/fedora-bootc@sha256:{}", "5".repeat(64)),
            degraded: vec!["fingerprint".to_owned(), "audio".to_owned()],
        }
    }

    #[test]
    fn builds_the_contract_shape() {
        let packages = parse_rpm_output(RPM_SAMPLE).unwrap();
        assert_eq!(packages.len(), 7, "gpg-pubkey is skipped");
        let built = build(&input(), packages).unwrap();
        let m = &built.manifest;
        assert_eq!(m.schema, 1);
        assert_eq!(m.name, "apex");
        assert_eq!(m.fedora, 45);
        assert_eq!(m.degraded, ["audio", "fingerprint"]);
        let keys: Vec<(&str, &str)> = m.packages.iter().map(Package::key).collect();
        // Byte order: uppercase sorts before lowercase.
        assert_eq!(
            keys,
            [
                ("NetworkManager", "x86_64"),
                ("bash", "x86_64"),
                ("glibc", "i686"),
                ("glibc", "x86_64"),
                ("kernel-core", "x86_64"),
                ("zsh", "x86_64"),
            ]
        );
        assert_eq!(
            m.packages[4].version, "7.2.9",
            "the newest duplicate is kept"
        );
        assert_eq!(m.packages[0].epoch, 1);
        assert_eq!(
            built.duplicates,
            [("kernel-core".to_owned(), "x86_64".to_owned())]
        );
        insta::assert_snapshot!(String::from_utf8(m.to_json().unwrap()).unwrap());
    }

    #[test]
    fn rejects_bad_inputs() {
        let with = |f: fn(&mut ManifestInput)| {
            let mut i = input();
            f(&mut i);
            build(&i, Vec::new()).map(|_| ()).unwrap_err().to_string()
        };
        assert!(with(|i| i.version = "45.20260924".into()).contains("version"));
        assert!(with(|i| i.version = "45.20260924.117.1".into()).contains("version"));
        assert!(with(|i| i.version = "45.20261324.1".into()).contains("version"));
        assert!(with(|i| i.version = "45.020260924.1".into()).contains("version"));
        assert!(with(|i| i.version = "45.2026092.1".into()).contains("version"));
        assert!(with(|i| i.revision = "ABCDEF".into()).contains("revision"));
        assert!(with(|i| i.base = "quay.io/fedora/fedora-bootc:45".into()).contains("pinned"));
        assert!(with(|i| i.degraded.push("wifi".into())).contains("wifi"));
    }

    #[test]
    fn rejects_malformed_rpm_lines() {
        assert!(matches!(
            parse_rpm_output("bash\t0\t5.3\n"),
            Err(ManifestBuildError::RpmLine { line: 1, .. })
        ));
        assert!(matches!(
            parse_rpm_output("ok\t0\t1\t1\tnoarch\nbash\tx\t5.3\t1\tx86_64\n"),
            Err(ManifestBuildError::RpmLine { line: 2, .. })
        ));
        assert!(matches!(
            parse_rpm_output("bash\t0\t5.3\t1\t(none)\n"),
            Err(ManifestBuildError::RpmLine { .. })
        ));
    }

    #[test]
    fn writes_atomically_with_mode_0644() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let built = build(&input(), parse_rpm_output(RPM_SAMPLE).unwrap()).unwrap();
        let path = write(root.path(), &built.manifest).unwrap();
        assert_eq!(path, root.path().join("usr/share/apex/manifest.json"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(Manifest::from_json(&bytes).unwrap(), built.manifest);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        // Nothing else is left in the directory.
        let entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1);
    }
}
