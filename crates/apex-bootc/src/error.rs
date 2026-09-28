use std::fmt;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use crate::command::CommandKind;
use crate::status::StatusParseError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitState {
    Code(i32),
    Signal(i32),
}

impl ExitState {
    pub const SUCCESS: Self = Self::Code(0);

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
            // wait(2) reports stopped children only on request, which std and tokio never make.
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

#[derive(Debug, thiserror::Error)]
pub enum BootcError {
    #[error("another operation holds the system update lock: {detail}")]
    Busy { detail: String },
    #[error("network error: {detail}")]
    Network { detail: String },
    #[error("image rejected by the signature policy: {detail}")]
    SignatureRejected { detail: String },
    #[error("not enough disk space: {detail}")]
    NoSpace { detail: String },
    #[error("this system is not booted from a bootc image: {detail}")]
    NotBootcHost { detail: String },
    #[error("the installed bootc does not support `bootc {command}`: {detail}")]
    Unsupported {
        command: CommandKind,
        detail: String,
    },
    #[error("`bootc {command}` timed out after {} s", .timeout.as_secs())]
    Timeout {
        command: CommandKind,
        timeout: Duration,
    },
    #[error("`bootc {command}` was cancelled")]
    Cancelled { command: CommandKind },
    #[error("could not run {}: {source}", .program.display())]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    InvalidStatus(#[from] StatusParseError),
    #[error("`bootc {command}` failed ({exit}): {detail}")]
    Other {
        command: CommandKind,
        exit: ExitState,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Busy,
    Network,
    SignatureRejected,
    NoSpace,
    NotBootcHost,
    Unsupported,
    Timeout,
    Cancelled,
    Spawn,
    InvalidStatus,
    Other,
}

impl BootcError {
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

    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind(),
            ErrorKind::Busy | ErrorKind::Network | ErrorKind::Timeout
        )
    }
}

pub const DETAIL_MAX_CHARS: usize = 512;

const NOT_BOOTC_HOST: &[&str] = &[
    // bootc crates/lib/src/cli.rs, get_storage()
    "system not booted via bootc",
    // ostree src/libostree/ostree-sysroot.c, ostree_sysroot_require_booted_deployment()
    "not currently booted into an ostree system",
];

/// Rarely seen: off a tty bootc waits for the lock silently, so the runner also probes the lock.
const BUSY: &[&str] = &[
    // ostree-ext sysroot.rs SysrootLock::new_from_sysroot(), ostree on_sysroot_lock_timeout()
    "waiting for sysroot lock",
    // rpm-ostree rpmostreed-sysroot.cxx and rpmostreed-transaction.cxx
    "transaction in progress",
];

const NO_SPACE: &[&str] = &[
    // strerror(ENOSPC), glibc
    "no space left on device",
    // strerror(EDQUOT), glibc
    "disk quota exceeded",
    // bootc crates/lib/src/deploy.rs, check_disk_space_inner()
    "insufficient free space",
    // ostree src/libostree/ostree-repo-commit.c, throw_min_free_space_error()
    "min-free-space-size",
    "min-free-space-percent",
];

/// From containers/image `image/signature/*` through the image proxy's `OpenImage`, unless noted.
const SIGNATURE: &[&str] = &[
    // policy_eval_sigstore.go
    "a signature was required, but no signature exists",
    "none of the signatures were accepted",
    "signature for identity",
    "signature for digest",
    // policy_eval_simple.go
    "is rejected by policy",
    // policy_eval.go
    "no signature verification policy found",
    // internal/sigstore_payload.go
    "cryptographic signature verification failed",
    // policy_eval_sigstore.go
    "no public key verified against the rekorset",
    // ostree-ext crates/ostree-ext/src/container/store.rs
    "specifies a default of `insecureacceptanything`",
];

const NETWORK: &[&str] = &[
    // containers/image image/docker/docker_client.go, detectPropertiesHelper()
    "pinging container registry",
    // containers/image image/docker/errors.go: ErrTooManyRequests
    "too many requests to registry",
    // Go net: *net.OpError for dials
    "dial tcp",
    // Go net: DNSError
    "no such host",
    "server misbehaving",
    // glibc gai_strerror(EAI_AGAIN)
    "temporary failure in name resolution",
    // Go net/os, net/http and context timeouts
    "i/o timeout",
    "tls handshake timeout",
    "context deadline exceeded",
    // strerror(ECONNREFUSED / ECONNRESET / ENETUNREACH / EHOSTUNREACH), glibc
    "connection refused",
    "connection reset by peer",
    "network is unreachable",
    "no route to host",
    // containers/image image/docker/errors.go: a struggling registry or CDN
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
];

/// clap 4 usage errors, exit status 2 (`clap_builder` `src/error/format.rs`).
const USAGE: &[&str] = &["unexpected argument", "unrecognized subcommand"];

/// Call only for a failed `exit`. bootc prints any failure as one `error: <context>: <cause>`
/// line with status 1 (bootc `crates/utils/src/lib.rs` `run_main`), so causes are needles.
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

/// bootc prints its whole error chain on one `error: ` line and clap follows its `error:` line
/// with usage text, so the last such line is the informative one.
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

const ESC: char = '\u{1b}';
const CSI_FINAL_BYTES: RangeInclusive<char> = '\u{40}'..='\u{7e}';

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != ESC {
            out.push(c);
            continue;
        }
        if chars.next_if_eq(&'[').is_some() {
            for c in chars.by_ref() {
                if CSI_FINAL_BYTES.contains(&c) {
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
