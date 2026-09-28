//! What Apex may ask bootc to do, and exactly how it is invoked
//! (`docs/specs/M1-IMPLEMENTATION.md`, section 6).
//!
//! argv is built only from [`BootcCommand`] variants and compile-time constants; the one
//! runtime input, a pinned digest, is validated character by character. The child gets
//! a cleared environment plus [`ENVIRONMENT`].

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::digest::{Digest, DigestError};

/// The bootc executable.
pub const BOOTC_PROGRAM: &str = "/usr/bin/bootc";

/// Environment variable that replaces [`BOOTC_PROGRAM`], honoured in debug builds only.
pub const PROGRAM_OVERRIDE_ENV: &str = "APEX_BOOTC";

/// The strict signature policy every bootc call runs under (ADR 0012).
pub const POLICY_JSON: &str = "/usr/share/apex/containers/policy.json";

/// The complete environment of a bootc child process, applied after `env_clear()`.
pub const ENVIRONMENT: [(&str, &str); 3] = [
    ("PATH", "/usr/bin:/usr/sbin"),
    ("LANG", "C.UTF-8"),
    ("CONTAINERS_POLICY_JSON", POLICY_JSON),
];

/// File descriptor number the progress pipe gets in the child (`--progress-fd 3`).
pub const PROGRESS_FD: i32 = 3;

/// Timeout of `bootc status`.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout of `bootc upgrade --check`.
pub const CHECK_TIMEOUT: Duration = Duration::from_mins(5);
/// Timeout of commands that download an image.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_hours(2);
/// Timeout of every other command.
pub const OTHER_TIMEOUT: Duration = Duration::from_mins(10);

/// Stable channel: the release-key-signed repository (`docs/specs/M1-IMPLEMENTATION.md` §1).
pub const APEX_STABLE: ImageRef = ImageRef::from_static("ghcr.io/sekirokenjii/apex:45");
/// Testing channel: the CI-key-signed repository.
pub const APEX_TESTING: ImageRef = ImageRef::from_static("ghcr.io/sekirokenjii/apex-testing:45");
/// Repository of the stable channel, for [`ImageRef::at_digest`].
pub const APEX_STABLE_REPOSITORY: &str = "ghcr.io/sekirokenjii/apex";
/// Repository of the testing channel, for [`ImageRef::at_digest`].
pub const APEX_TESTING_REPOSITORY: &str = "ghcr.io/sekirokenjii/apex-testing";

/// The bootc executable to run: [`BOOTC_PROGRAM`], or in debug builds the absolute path
/// in `APEX_BOOTC` if it is set.
pub fn program() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(PROGRAM_OVERRIDE_ENV).map(PathBuf::from)
        && path.is_absolute()
    {
        return path;
    }
    PathBuf::from(BOOTC_PROGRAM)
}

/// A container image reference for `bootc switch`.
///
/// It can only come from a `&'static str` (a compile-time constant; use it in a `const`
/// so a malformed literal fails the build) or from a static repository plus a validated
/// digest. Either way it cannot start with `-`, so it can never be read as an option.
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

/// Why a string is not an acceptable [`ImageRef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageRefError {
    /// Empty, or longer than 255 bytes.
    #[error("image reference must be 1 to 255 bytes long")]
    Length,
    /// Does not start with an ASCII letter or digit (this rules out `-`).
    #[error("image reference must start with a letter or digit")]
    BadStart,
    /// Contains a byte outside `[A-Za-z0-9._:/@-]`.
    #[error("image reference contains a character outside [A-Za-z0-9._:/@-]")]
    BadCharacter,
    /// Has no `/`, or contains `//` or `..`.
    #[error("image reference must be a registry path without empty or dot-dot components")]
    BadPath,
    /// A repository passed to [`ImageRef::at_digest`] already has a tag or digest.
    #[error("repository must not carry a tag or digest")]
    TaggedRepository,
    /// The digest passed to [`ImageRef::at_digest`] is invalid.
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

/// A repository has no `@` at all and no `:` in its last path component (a `:` before
/// the first `/` is a registry port).
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
    /// Builds a reference from a compile-time constant.
    ///
    /// Use it to initialise a `const`, so a malformed literal is a build error.
    ///
    /// # Panics
    ///
    /// Panics (a compile-time error in a `const` context) if `reference` is not a valid
    /// image reference.
    pub const fn from_static(reference: &'static str) -> Self {
        match validate_reference(reference) {
            Ok(()) => Self(Repr::Static(reference)),
            Err(_) => panic!("invalid image reference literal"),
        }
    }

    /// Like [`ImageRef::from_static`] but returns an error instead of panicking.
    pub const fn try_from_static(reference: &'static str) -> Result<Self, ImageRefError> {
        match validate_reference(reference) {
            Ok(()) => Ok(Self(Repr::Static(reference))),
            Err(e) => Err(e),
        }
    }

    /// `repository@digest`, e.g. to switch to one specific build. The repository is a
    /// compile-time constant; the digest is the only runtime input and must be
    /// `sha256:` followed by 64 lowercase hex digits.
    pub fn at_digest(repository: &'static str, digest: &str) -> Result<Self, ImageRefError> {
        validate_repository(repository)?;
        let digest = Digest::parse(digest)?;
        Ok(Self(Repr::Pinned { repository, digest }))
    }

    /// The pinned digest, if this reference was built with [`ImageRef::at_digest`].
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

/// Every bootc invocation Apex makes (`docs/specs/M1-IMPLEMENTATION.md` §6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BootcCommand {
    /// `bootc status --format=json --format-version=1`
    Status,
    /// `bootc upgrade --check`
    UpgradeCheck,
    /// `bootc upgrade --download-only [--progress-fd 3]`
    UpgradeDownloadOnly {
        /// Pass the progress pipe as fd 3.
        progress: bool,
    },
    /// `bootc upgrade --from-downloaded`
    UpgradeFromDownloaded,
    /// `bootc rollback`
    Rollback,
    /// `bootc switch --enforce-container-sigpolicy --download-only <target>`
    SwitchDownloadOnly {
        /// The image to switch to.
        target: ImageRef,
    },
}

/// A [`BootcCommand`] without its data, for errors and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    /// [`BootcCommand::Status`].
    Status,
    /// [`BootcCommand::UpgradeCheck`].
    UpgradeCheck,
    /// [`BootcCommand::UpgradeDownloadOnly`].
    UpgradeDownloadOnly,
    /// [`BootcCommand::UpgradeFromDownloaded`].
    UpgradeFromDownloaded,
    /// [`BootcCommand::Rollback`].
    Rollback,
    /// [`BootcCommand::SwitchDownloadOnly`].
    SwitchDownloadOnly,
}

impl CommandKind {
    /// The bootc subcommand and its distinguishing flag, e.g. `upgrade --check`.
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

    /// How long the command may run before its process group is killed: status 30 s,
    /// check 5 min, downloads 2 h, everything else 10 min.
    ///
    /// `switch --download-only` downloads a whole image, so it counts as a download.
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
    /// The command without its data.
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

    /// The arguments after the program name.
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

    /// See [`CommandKind::timeout`].
    pub fn timeout(&self) -> Duration {
        self.kind().timeout()
    }

    /// The child expects the progress pipe at fd [`PROGRESS_FD`].
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
        assert_eq!(APEX_STABLE.to_string(), "ghcr.io/sekirokenjii/apex:45");
        assert_eq!(
            APEX_TESTING.to_string(),
            "ghcr.io/sekirokenjii/apex-testing:45"
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
        assert_eq!(r.to_string(), format!("ghcr.io/sekirokenjii/apex@{D}"));
        assert_eq!(r.digest().map(Digest::as_str), Some(D));
        assert_eq!(APEX_STABLE.digest(), None);
        assert_eq!(
            ImageRef::at_digest("ghcr.io/sekirokenjii/apex:45", D),
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
        // The test environment does not set APEX_BOOTC.
        if std::env::var_os(PROGRAM_OVERRIDE_ENV).is_none() {
            assert_eq!(program(), PathBuf::from("/usr/bin/bootc"));
        }
    }
}
