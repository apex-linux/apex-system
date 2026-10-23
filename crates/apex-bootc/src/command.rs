use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::digest::{Digest, DigestError};

pub const BOOTC_PROGRAM: &str = "/usr/bin/bootc";

/// Replaces [`BOOTC_PROGRAM`] in debug builds only.
pub const PROGRAM_OVERRIDE_ENV: &str = "APEX_BOOTC";

pub const POLICY_JSON: &str = "/usr/share/apex/containers/policy.json";

/// The complete environment of a bootc child: the runner clears everything else.
pub const ENVIRONMENT: [(&str, &str); 3] = [
    ("PATH", "/usr/bin:/usr/sbin"),
    ("LANG", "C.UTF-8"),
    ("CONTAINERS_POLICY_JSON", POLICY_JSON),
];

pub const PROGRESS_FD: i32 = 3;

pub const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
pub const CHECK_TIMEOUT: Duration = Duration::from_mins(5);
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_hours(2);
pub const OTHER_TIMEOUT: Duration = Duration::from_mins(10);

pub const APEX_STABLE: ImageRef = ImageRef::from_static("ghcr.io/apex-linux/apex:45");
pub const APEX_TESTING: ImageRef = ImageRef::from_static("ghcr.io/apex-linux/apex-testing:45");
pub const APEX_STABLE_REPOSITORY: &str = "ghcr.io/apex-linux/apex";
pub const APEX_TESTING_REPOSITORY: &str = "ghcr.io/apex-linux/apex-testing";

pub fn program() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(PROGRAM_OVERRIDE_ENV).map(PathBuf::from)
        && path.is_absolute()
    {
        return path;
    }
    PathBuf::from(BOOTC_PROGRAM)
}

/// Built only from a `&'static str`, or a static repository plus a validated digest, and
/// never starting with `-`, so bootc can never read it as an option.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageRef(Repr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Repr {
    Static(&'static str),
    Pinned {
        repository: &'static str,
        digest: Digest,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageRefError {
    #[error("image reference must be 1 to 255 bytes long")]
    Length,
    #[error("image reference must start with a letter or digit")]
    BadStart,
    #[error("image reference contains a character outside [A-Za-z0-9._:/@-]")]
    BadCharacter,
    #[error("image reference must be a registry path without empty or dot-dot components")]
    BadPath,
    #[error("repository must not carry a tag or digest")]
    TaggedRepository,
    #[error(transparent)]
    Digest(#[from] DigestError),
}

const MAX_REFERENCE_LEN: usize = 255;

const fn validate_reference(s: &str) -> Result<(), ImageRefError> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > MAX_REFERENCE_LEN {
        return Err(ImageRefError::Length);
    }
    if !b[0].is_ascii_alphanumeric() {
        return Err(ImageRefError::BadStart);
    }
    let mut has_slash = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'/' | b'@' | b'-')) {
            return Err(ImageRefError::BadCharacter);
        }
        if c == b'/' {
            has_slash = true;
        }
        if i + 1 < b.len() {
            let pair = [c, b[i + 1]];
            if matches!(pair, [b'/', b'/'] | [b'.', b'.']) {
                return Err(ImageRefError::BadPath);
            }
        }
        i += 1;
    }
    if !has_slash || b[b.len() - 1] == b'/' {
        return Err(ImageRefError::BadPath);
    }
    Ok(())
}

/// No `@` anywhere and no `:` after the last `/`: a `:` before it is a registry port.
const fn validate_repository(s: &str) -> Result<(), ImageRefError> {
    if let Err(e) = validate_reference(s) {
        return Err(e);
    }
    let b = s.as_bytes();
    let mut i = b.len();
    while i > 0 {
        i -= 1;
        match b[i] {
            b'/' => break,
            b':' | b'@' => return Err(ImageRefError::TaggedRepository),
            _ => {}
        }
    }
    while i > 0 {
        i -= 1;
        if b[i] == b'@' {
            return Err(ImageRefError::TaggedRepository);
        }
    }
    Ok(())
}

impl ImageRef {
    /// Initialise a `const` with it, so that an invalid literal fails the build.
    pub const fn from_static(reference: &'static str) -> Self {
        match validate_reference(reference) {
            Ok(()) => Self(Repr::Static(reference)),
            Err(_) => panic!("invalid image reference literal"),
        }
    }

    pub const fn try_from_static(reference: &'static str) -> Result<Self, ImageRefError> {
        match validate_reference(reference) {
            Ok(()) => Ok(Self(Repr::Static(reference))),
            Err(e) => Err(e),
        }
    }

    pub fn at_digest(repository: &'static str, digest: &str) -> Result<Self, ImageRefError> {
        validate_repository(repository)?;
        let digest = Digest::parse(digest)?;
        Ok(Self(Repr::Pinned { repository, digest }))
    }

    pub fn digest(&self) -> Option<&Digest> {
        match &self.0 {
            Repr::Static(_) => None,
            Repr::Pinned { digest, .. } => Some(digest),
        }
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Repr::Static(s) => f.write_str(s),
            Repr::Pinned { repository, digest } => write!(f, "{repository}@{digest}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BootcCommand {
    Status,
    UpgradeCheck,
    UpgradeDownloadOnly { progress: bool },
    UpgradeFromDownloaded,
    Rollback,
    SwitchDownloadOnly { target: ImageRef },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    Status,
    UpgradeCheck,
    UpgradeDownloadOnly,
    UpgradeFromDownloaded,
    Rollback,
    SwitchDownloadOnly,
}

impl CommandKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::UpgradeCheck => "upgrade --check",
            Self::UpgradeDownloadOnly => "upgrade --download-only",
            Self::UpgradeFromDownloaded => "upgrade --from-downloaded",
            Self::Rollback => "rollback",
            Self::SwitchDownloadOnly => "switch --download-only",
        }
    }

    pub const fn timeout(self) -> Duration {
        match self {
            Self::Status => STATUS_TIMEOUT,
            Self::UpgradeCheck => CHECK_TIMEOUT,
            Self::UpgradeDownloadOnly | Self::SwitchDownloadOnly => DOWNLOAD_TIMEOUT,
            Self::UpgradeFromDownloaded | Self::Rollback => OTHER_TIMEOUT,
        }
    }
}

impl fmt::Display for CommandKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl BootcCommand {
    pub fn kind(&self) -> CommandKind {
        match self {
            Self::Status => CommandKind::Status,
            Self::UpgradeCheck => CommandKind::UpgradeCheck,
            Self::UpgradeDownloadOnly { .. } => CommandKind::UpgradeDownloadOnly,
            Self::UpgradeFromDownloaded => CommandKind::UpgradeFromDownloaded,
            Self::Rollback => CommandKind::Rollback,
            Self::SwitchDownloadOnly { .. } => CommandKind::SwitchDownloadOnly,
        }
    }

    /// Without the program name.
    pub fn argv(&self) -> Vec<OsString> {
        let fixed: &[&'static str] = match self {
            Self::Status => &["status", "--format=json", "--format-version=1"],
            Self::UpgradeCheck => &["upgrade", "--check"],
            Self::UpgradeDownloadOnly { progress: false } => &["upgrade", "--download-only"],
            Self::UpgradeDownloadOnly { progress: true } => {
                &["upgrade", "--download-only", "--progress-fd", "3"]
            }
            Self::UpgradeFromDownloaded => &["upgrade", "--from-downloaded"],
            Self::Rollback => &["rollback"],
            Self::SwitchDownloadOnly { .. } => {
                &["switch", "--enforce-container-sigpolicy", "--download-only"]
            }
        };
        let mut argv: Vec<OsString> = fixed.iter().map(OsString::from).collect();
        if let Self::SwitchDownloadOnly { target } = self {
            argv.push(target.to_string().into());
        }
        argv
    }

    pub fn timeout(&self) -> Duration {
        self.kind().timeout()
    }

    pub fn uses_progress_fd(&self) -> bool {
        matches!(self, Self::UpgradeDownloadOnly { progress: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:47e5ed613a970b6574bfa954ab25bb6e85656552899aa518b5961d9645102b38";

    #[test]
    fn channel_constants_are_valid() {
        assert_eq!(APEX_STABLE.to_string(), "ghcr.io/apex-linux/apex:45");
        assert_eq!(
            APEX_TESTING.to_string(),
            "ghcr.io/apex-linux/apex-testing:45"
        );
        assert!(validate_repository(APEX_STABLE_REPOSITORY).is_ok());
        assert!(validate_repository(APEX_TESTING_REPOSITORY).is_ok());
    }

    #[test]
    fn static_references_are_validated() {
        use ImageRefError as E;
        let cases: [(&'static str, Result<(), E>); 12] = [
            ("quay.io/fedora/fedora-bootc:45", Ok(())),
            ("localhost:5000/apex@sha256:00", Ok(())),
            ("", Err(E::Length)),
            ("-rf/x", Err(E::BadStart)),
            ("--transport=oci/x", Err(E::BadStart)),
            ("/abs/path", Err(E::BadStart)),
            ("ghcr.io/a b", Err(E::BadCharacter)),
            ("ghcr.io/a;rm", Err(E::BadCharacter)),
            ("ghcr.io/ä", Err(E::BadCharacter)),
            ("noslash", Err(E::BadPath)),
            ("ghcr.io//x", Err(E::BadPath)),
            ("ghcr.io/../x", Err(E::BadPath)),
        ];
        for (s, expected) in cases {
            assert_eq!(ImageRef::try_from_static(s).map(|_| ()), expected, "{s}");
        }
        let long: &'static str = Box::leak("a/".repeat(128).into_boxed_str());
        assert_eq!(ImageRef::try_from_static(long).map(|_| ()), Err(E::Length));
    }

    #[test]
    fn pinned_references() {
        let r = ImageRef::at_digest(APEX_STABLE_REPOSITORY, D).unwrap();
        assert_eq!(r.to_string(), format!("ghcr.io/apex-linux/apex@{D}"));
        assert_eq!(r.digest().map(Digest::as_str), Some(D));
        assert_eq!(APEX_STABLE.digest(), None);
        assert_eq!(
            ImageRef::at_digest("ghcr.io/apex-linux/apex:45", D),
            Err(ImageRefError::TaggedRepository)
        );
        assert_eq!(
            ImageRef::at_digest("ghcr.io/x@sha256:00/y", D),
            Err(ImageRefError::TaggedRepository)
        );
        assert!(ImageRef::at_digest("localhost:5000/apex", D).is_ok());
        assert!(matches!(
            ImageRef::at_digest(APEX_STABLE_REPOSITORY, "sha256:ABC"),
            Err(ImageRefError::Digest(_))
        ));
        assert!(matches!(
            ImageRef::at_digest(APEX_STABLE_REPOSITORY, "latest; rm -rf /"),
            Err(ImageRefError::Digest(DigestError::MissingPrefix))
        ));
    }

    #[test]
    fn only_download_with_progress_uses_fd3() {
        assert!(BootcCommand::UpgradeDownloadOnly { progress: true }.uses_progress_fd());
        assert!(!BootcCommand::UpgradeDownloadOnly { progress: false }.uses_progress_fd());
        assert!(!BootcCommand::Status.uses_progress_fd());
    }

    #[test]
    fn program_defaults_to_usr_bin_bootc() {
        if std::env::var_os(PROGRAM_OVERRIDE_ENV).is_none() {
            assert_eq!(program(), PathBuf::from("/usr/bin/bootc"));
        }
    }
}
