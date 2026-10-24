use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use apex_update::manifest::{MANIFEST_PATH, MANIFEST_SCHEMA};
use apex_update::{Manifest, Package, Version};

use crate::render::{OutputError, write_atomic};

/// `%{EPOCHNUM}` prints 0 rather than `(none)` for no epoch (`RPMTAG_EPOCHNUM`, rpm 6.1).
pub const RPM_QUERY_FORMAT: &str = "%{NAME}\\t%{EPOCHNUM}\\t%{VERSION}\\t%{RELEASE}\\t%{ARCH}\\n";

pub const IMAGE_NAME: &str = "apex";

pub const DEGRADED_COMPONENTS: &[&str] = &["audio", "fingerprint", "nvidia", "shell", "terminal"];

/// rpm lists imported signing keys as packages of this name.
const GPG_PUBKEY: &str = "gpg-pubkey";

#[derive(Debug, thiserror::Error)]
pub enum ManifestBuildError {
    #[error(
        "version {0:?} is not <fedora>.<yyyymmdd>.<run> (e.g. 45.20260924.117) in canonical form"
    )]
    Version(String),
    #[error("revision {0:?} is not a full lowercase git commit id (40 or 64 hex digits)")]
    Revision(String),
    #[error("base image {0:?} is not pinned by digest (<repository>@sha256:<64 hex digits>)")]
    Base(String),
    #[error(
        "unknown degraded component {0:?} (known: audio, fingerprint, nvidia, shell, terminal)"
    )]
    Degraded(String),
    #[error("held family {0:?} is not a package name ([a-z0-9][a-z0-9._+-]*)")]
    Held(String),
    #[error("rpm output line {line}: {reason}: {text:?}")]
    RpmLine {
        line: usize,
        reason: &'static str,
        text: String,
    },
    #[error("rpm listed no packages under {0}")]
    NoPackages(String),
    #[error("cannot run rpm: {0}")]
    RpmSpawn(std::io::Error),
    #[error("rpm failed ({status}): {stderr}")]
    RpmFailed { status: String, stderr: String },
    #[error("cannot serialize the manifest: {0}")]
    Encode(apex_update::ManifestError),
    #[error(transparent)]
    Write(#[from] OutputError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInput {
    pub version: String,
    pub revision: String,
    pub base: String,
    pub degraded: Vec<String>,
    pub held: Vec<String>,
}

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

/// Must accept what `apex-lane` and `image.sh` accept as a family name.
fn is_family_name(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'+' | b'-')
        })
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltManifest {
    pub manifest: Manifest,
    pub duplicates: Vec<(String, String)>,
}

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
    if let Some(family) = input.held.iter().find(|family| !is_family_name(family)) {
        return Err(ManifestBuildError::Held(family.clone()));
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
        // Unreachable error: every Fedora release fits u32.
        fedora: u32::try_from(version.fedora())
            .map_err(|_| ManifestBuildError::Version(input.version.clone()))?,
        revision: input.revision.clone(),
        base_image: input.base.clone(),
        degraded: input.degraded.clone(),
        held: input.held.clone(),
        packages,
    };
    manifest.normalize();
    Ok(BuiltManifest {
        manifest,
        duplicates: duplicates.into_iter().collect(),
    })
}

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

pub fn manifest_path(root: &Path) -> PathBuf {
    root.join(MANIFEST_PATH.trim_start_matches('/'))
}

/// Mode 0644: the unprivileged Updates app reads it.
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
            held: vec!["gnome-shell".to_owned()],
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
        assert_eq!(m.held, ["gnome-shell"]);
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
        assert!(with(|i| i.held.push("Gnome Shell".into())).contains("Gnome Shell"));
        assert!(with(|i| i.held.push(String::new())).contains("held family"));
        assert!(with(|i| i.held.push("-x".into())).contains("-x"));
        let mut shell = input();
        shell.degraded = vec!["shell".to_owned(), "terminal".to_owned()];
        shell.held = vec!["kernel-cachyos".to_owned(), "libadwaita".to_owned()];
        assert!(
            build(&shell, Vec::new()).is_ok(),
            "degraded=shell and lane families"
        );
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
        let entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1);
    }
}
