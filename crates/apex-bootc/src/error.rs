//! Errors of bootc invocations, and the classification of a failed run from its exit
//! status and stderr.
//!
//! bootc reports every failure as `error: <context>: <cause>: …` on stderr with exit
//! status 1 (bootc `crates/utils/src/lib.rs`, `run_main`, printing an `anyhow` chain),
//! so the cause is recognised from well-known substrings of the underlying libraries.
//! Each needle below cites where its text is produced. Matching is case-insensitive,
//! checked in a fixed order from most to least specific, and anything unrecognised is
//! [`BootcError::Other`]. Nothing here panics.

use std::fmt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use crate::command::CommandKind;
use crate::status::StatusParseError;

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitState {
    /// It exited with this status code.
    Code(i32),
    /// It was killed by this signal.
    Signal(i32),
}

impl ExitState {
    /// Exit code 0.
    pub const SUCCESS: Self = Self::Code(0);

    /// The process exited with code 0.
    pub fn success(self) -> bool {
        self == Self::SUCCESS
    }
}

impl From<ExitStatus> for ExitState {
    fn from(status: ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt as _;
        match (status.code(), status.signal()) {
            (Some(code), _) => Self::Code(code),
            (None, Some(signal)) => Self::Signal(signal),
            // wait(2) only reports stopped/continued children when asked to; neither
            // std nor tokio asks, so this is unreachable in practice.
            (None, None) => Self::Code(-1),
        }
    }
}

impl fmt::Display for ExitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(code) => write!(f, "exit status {code}"),
            Self::Signal(signal) => write!(f, "killed by signal {signal}"),
        }
    }
}

/// A bootc operation failed.
///
/// `detail` fields carry the last meaningful line of bootc's stderr (ANSI escapes and
/// the `error: ` prefix removed, at most [`DETAIL_MAX_CHARS`] characters), suitable
/// for logs and for the daemon's `LastError`.
#[derive(Debug, thiserror::Error)]
pub enum BootcError {
    /// The ostree sysroot lock is held by another process (another bootc, rpm-ostree,
    /// or `ostree-finalize-staged`).
    #[error("another operation holds the system update lock: {detail}")]
    Busy {
        /// Evidence.
        detail: String,
    },
    /// The registry could not be reached or failed transiently.
    #[error("network error: {detail}")]
    Network {
        /// Last line of stderr.
        detail: String,
    },
    /// The image failed the signature policy (`CONTAINERS_POLICY_JSON`).
    #[error("image rejected by the signature policy: {detail}")]
    SignatureRejected {
        /// Last line of stderr.
        detail: String,
    },
    /// Not enough disk space.
    #[error("not enough disk space: {detail}")]
    NoSpace {
        /// Last line of stderr.
        detail: String,
    },
    /// The running system is not a bootc-managed image.
    #[error("this system is not booted from a bootc image: {detail}")]
    NotBootcHost {
        /// Evidence.
        detail: String,
    },
    /// The installed bootc rejected the command line (clap usage error, exit status 2),
    /// e.g. a flag it does not know yet.
    #[error("the installed bootc does not support `bootc {command}`: {detail}")]
    Unsupported {
        /// The command that was rejected.
        command: CommandKind,
        /// Last line of stderr.
        detail: String,
    },
    /// The command ran longer than its timeout and its process group was killed.
    #[error("`bootc {command}` timed out after {} s", .timeout.as_secs())]
    Timeout {
        /// The command.
        command: CommandKind,
        /// The timeout that expired.
        timeout: Duration,
    },
    /// The command was cancelled and its process group was killed.
    #[error("`bootc {command}` was cancelled")]
    Cancelled {
        /// The command.
        command: CommandKind,
    },
    /// bootc could not be started (or waited for).
    #[error("could not run {}: {source}", .program.display())]
    Spawn {
        /// The executable.
        program: PathBuf,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// `bootc status` printed something that is not a v1 status document.
    #[error(transparent)]
    InvalidStatus(#[from] StatusParseError),
    /// Any other failure.
    #[error("`bootc {command}` failed ({exit}): {detail}")]
    Other {
        /// The command.
        command: CommandKind,
        /// How bootc ended.
        exit: ExitState,
        /// Last line of stderr.
        detail: String,
    },
}

/// A [`BootcError`] without its data, e.g. for mapping to D-Bus error names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// [`BootcError::Busy`].
    Busy,
    /// [`BootcError::Network`].
    Network,
    /// [`BootcError::SignatureRejected`].
    SignatureRejected,
    /// [`BootcError::NoSpace`].
    NoSpace,
    /// [`BootcError::NotBootcHost`].
    NotBootcHost,
    /// [`BootcError::Unsupported`].
    Unsupported,
    /// [`BootcError::Timeout`].
    Timeout,
    /// [`BootcError::Cancelled`].
    Cancelled,
    /// [`BootcError::Spawn`].
    Spawn,
    /// [`BootcError::InvalidStatus`].
    InvalidStatus,
    /// [`BootcError::Other`].
    Other,
}

impl BootcError {
    /// The variant, without data.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Busy { .. } => ErrorKind::Busy,
            Self::Network { .. } => ErrorKind::Network,
            Self::SignatureRejected { .. } => ErrorKind::SignatureRejected,
            Self::NoSpace { .. } => ErrorKind::NoSpace,
            Self::NotBootcHost { .. } => ErrorKind::NotBootcHost,
            Self::Unsupported { .. } => ErrorKind::Unsupported,
            Self::Timeout { .. } => ErrorKind::Timeout,
            Self::Cancelled { .. } => ErrorKind::Cancelled,
            Self::Spawn { .. } => ErrorKind::Spawn,
            Self::InvalidStatus(_) => ErrorKind::InvalidStatus,
            Self::Other { .. } => ErrorKind::Other,
        }
    }

    /// Transient failures worth retrying later without user action.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind(),
            ErrorKind::Busy | ErrorKind::Network | ErrorKind::Timeout
        )
    }
}

/// Longest `detail` kept, in characters.
pub const DETAIL_MAX_CHARS: usize = 512;

/// Needles for [`BootcError::NotBootcHost`].
const NOT_BOOTC_HOST: &[&str] = &[
    // bootc crates/lib/src/cli.rs, get_storage(): anyhow!("System not booted via bootc")
    "system not booted via bootc",
    // ostree src/libostree/ostree-sysroot.c, ostree_sysroot_require_booted_deployment()
    "not currently booted into an ostree system",
];

/// Needles for [`BootcError::Busy`]. bootc itself waits for the lock silently when
/// stderr is not a terminal (its spinner is an indicatif bar, hidden off-tty), so the
/// runner also probes the lock after a timeout ([`crate::sysroot_lock_held`]).
const BUSY: &[&str] = &[
    // ostree-ext crates/ostree-ext/src/sysroot.rs, SysrootLock::new_from_sysroot()
    // (spinner text) and ostree src/ostree/ot-admin-functions.c, on_sysroot_lock_timeout()
    "waiting for sysroot lock",
    // rpm-ostree src/daemon/rpmostreed-sysroot.cxx ("Transaction in progress: %s") and
    // src/daemon/rpmostreed-transaction.cxx ("System transaction in progress")
    "transaction in progress",
];

/// Needles for [`BootcError::NoSpace`].
const NO_SPACE: &[&str] = &[
    // strerror(ENOSPC), glibc
    "no space left on device",
    // strerror(EDQUOT), glibc
    "disk quota exceeded",
    // bootc crates/lib/src/deploy.rs, check_disk_space_inner():
    // "Insufficient free space for {image} (available: … required: …)"
    "insufficient free space",
    // ostree src/libostree/ostree-repo-commit.c, throw_min_free_space_error():
    // "min-free-space-size %luMB would be exceeded…", "min-free-space-percent '%u%%' …"
    "min-free-space-size",
    "min-free-space-percent",
];

/// Needles for [`BootcError::SignatureRejected`], from containers/image
/// (github.com/containers/container-libs, `image/signature/…`) as surfaced by the
/// containers-image-proxy (`common/pkg/json-proxy/handler.go`, `OpenImage`), and from
/// ostree-ext.
const SIGNATURE: &[&str] = &[
    // policy_eval_sigstore.go: PolicyRequirementError("A signature was required, but no signature exists")
    "a signature was required, but no signature exists",
    // policy_eval_sigstore.go: "None of the signatures were accepted, reasons: …"
    "none of the signatures were accepted",
    // policy_eval_sigstore.go: "Signature for identity %q is not accepted"
    "signature for identity",
    // policy_eval_sigstore.go: "Signature for digest %s does not match"
    "signature for digest",
    // policy_eval_simple.go: "Running image %s is rejected by policy." and
    // "Any signatures for image %s are rejected by policy."
    "is rejected by policy",
    // policy_eval.go: "No signature verification policy found for image %s"
    "no signature verification policy found",
    // internal/sigstore_payload.go: "cryptographic signature verification failed: …"
    "cryptographic signature verification failed",
    // policy_eval_sigstore.go: "No public key verified against the RekorSET: …"
    "no public key verified against the rekorset",
    // ostree-ext crates/ostree-ext/src/container/store.rs: "containers-policy.json
    // specifies a default of `insecureAcceptAnything`; refusing usage"
    "specifies a default of `insecureacceptanything`",
];

/// Needles for [`BootcError::Network`].
const NETWORK: &[&str] = &[
    // containers/image image/docker/docker_client.go, detectPropertiesHelper():
    // "pinging container registry %s: %w"
    "pinging container registry",
    // containers/image image/docker/errors.go: ErrTooManyRequests
    "too many requests to registry",
    // Go net: *net.OpError for dials, "dial tcp <addr>: …"
    "dial tcp",
    // Go net: DNSError "no such host", "server misbehaving"
    "no such host",
    "server misbehaving",
    // glibc gai_strerror(EAI_AGAIN)
    "temporary failure in name resolution",
    // Go net/os: timeouts ("i/o timeout"), net/http ("TLS handshake timeout"),
    // context ("context deadline exceeded")
    "i/o timeout",
    "tls handshake timeout",
    "context deadline exceeded",
    // strerror(ECONNREFUSED / ECONNRESET / ENETUNREACH / EHOSTUNREACH), glibc
    "connection refused",
    "connection reset by peer",
    "network is unreachable",
    "no route to host",
    // containers/image image/docker/errors.go, unexpected HTTP statuses from a
    // struggling registry or CDN
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
];

/// Needles for [`BootcError::Unsupported`] (with exit status 2): clap 4 usage errors
/// (`clap_builder` `src/error/format.rs`: "unexpected argument '…' found",
/// "unrecognized subcommand '…'").
const USAGE: &[&str] = &["unexpected argument", "unrecognized subcommand"];

/// Classifies a failed bootc run. Call only for a non-successful `exit`.
pub fn classify_failure(command: CommandKind, exit: ExitState, stderr: &[u8]) -> BootcError {
    let text = String::from_utf8_lossy(stderr);
    let lower = text.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    let mut detail = summarize_stderr(&text);
    if detail.is_empty() {
        "no error output".clone_into(&mut detail);
    }

    if exit == ExitState::Code(2) && has(USAGE) {
        return BootcError::Unsupported { command, detail };
    }
    if has(NOT_BOOTC_HOST) {
        BootcError::NotBootcHost { detail }
    } else if has(BUSY) {
        BootcError::Busy { detail }
    } else if has(NO_SPACE) {
        BootcError::NoSpace { detail }
    } else if has(SIGNATURE) {
        BootcError::SignatureRejected { detail }
    } else if has(NETWORK) {
        BootcError::Network { detail }
    } else {
        BootcError::Other {
            command,
            exit,
            detail,
        }
    }
}

/// The most informative line of `stderr`: the last line starting with `error: ` (bootc
/// prints its whole error chain on one such line; clap follows its `error:` line with
/// usage text), else the last non-empty line. ANSI escapes and the `error: ` prefix are
/// removed and the result is truncated to [`DETAIL_MAX_CHARS`] characters.
pub fn summarize_stderr(stderr: &str) -> String {
    let clean = strip_ansi(stderr);
    let lines = || clean.lines().map(str::trim);
    let line = lines()
        .rfind(|l| l.starts_with("error: "))
        .or_else(|| lines().rfind(|l| !l.is_empty()))
        .unwrap_or_default();
    let line = line.strip_prefix("error: ").unwrap_or(line);
    line.chars().take(DETAIL_MAX_CHARS).collect()
}

/// Removes ANSI CSI sequences (`ESC [ … final-byte`) and other lone escapes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next_if_eq(&'[').is_some() {
            // Parameter and intermediate bytes, then one final byte in 0x40..=0x7e.
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn summary_takes_last_line_without_colour_or_prefix() {
        let s = "note: something\n\u{1b}[31merror: \u{1b}[0mUpgrading: Pulling: boom\n\n";
        assert_eq!(summarize_stderr(s), "Upgrading: Pulling: boom");
        let clap = "error: unexpected argument '--x' found\n\nUsage: bootc upgrade\n\nFor more information, try '--help'.\n";
        assert_eq!(summarize_stderr(clap), "unexpected argument '--x' found");
        assert_eq!(summarize_stderr("warning\nlast words\n"), "last words");
        assert_eq!(summarize_stderr(""), "");
        assert_eq!(
            summarize_stderr(&"x".repeat(2000)).chars().count(),
            DETAIL_MAX_CHARS
        );
    }

    #[test]
    fn exit_state_display() {
        assert_eq!(ExitState::Code(1).to_string(), "exit status 1");
        assert_eq!(ExitState::Signal(9).to_string(), "killed by signal 9");
        assert!(ExitState::SUCCESS.success());
        assert!(!ExitState::Signal(0).success());
    }

    proptest! {
        #[test]
        fn classification_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048), code in any::<i32>()) {
            let e = classify_failure(CommandKind::UpgradeCheck, ExitState::Code(code), &bytes);
            let _ = e.to_string();
            prop_assert!(summarize_stderr(&String::from_utf8_lossy(&bytes)).chars().count() <= DETAIL_MAX_CHARS);
        }
    }
}
