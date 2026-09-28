//! bootc for Apex.
//!
//! `apex-updated` is the only component that runs bootc (ADR 0008); this crate is how
//! it does so. It provides:
//!
//! - [`Host`]: a lenient model of `bootc status --format=json --format-version=1`
//!   (`org.containers.bootc/v1`), with query helpers such as
//!   [`Host::booted_digest`], [`Host::staged_download_only`] and
//!   [`BootEntry::deployment_root`];
//! - [`progress`]: a parser for the JSON Lines bootc writes to `--progress-fd`, which
//!   never fails an update;
//! - [`BootcCommand`]: every invocation Apex makes, with its exact argv, environment
//!   and timeout (`docs/specs/M1-IMPLEMENTATION.md` §6);
//! - [`CommandRunner`]: the process boundary, implemented by [`TokioRunner`] (output
//!   caps, progress on fd 3, process-group kill on timeout and cancellation) and, with
//!   the `testing` feature, by the scripted `FakeRunner`;
//! - [`BootcError`] and [`classify_failure`]: failures classified from exit status and
//!   stderr;
//! - [`CheckOutcome`] and [`DownloadOutcome`]: what `bootc upgrade --check` and
//!   `bootc upgrade --download-only` report on stdout;
//! - [`Bootc`]: typed operations tying it all together.
//!
//! The crate contains no `unsafe` code. Mapping the progress pipe to fd 3 needs a
//! `pre_exec` hook, which the `command-fds` crate provides.

mod check;
mod client;
mod command;
mod digest;
mod download;
mod error;
#[cfg(any(test, feature = "testing"))]
mod fake;
mod lock;
pub mod progress;
mod query;
mod runner;
mod status;
mod time;

pub use check::{CheckOutcome, parse_check_output};
pub use client::Bootc;
pub use command::{
    APEX_STABLE, APEX_STABLE_REPOSITORY, APEX_TESTING, APEX_TESTING_REPOSITORY, BOOTC_PROGRAM,
    BootcCommand, CHECK_TIMEOUT, CommandKind, DOWNLOAD_TIMEOUT, ENVIRONMENT, ImageRef,
    ImageRefError, OTHER_TIMEOUT, POLICY_JSON, PROGRAM_OVERRIDE_ENV, PROGRESS_FD, STATUS_TIMEOUT,
    program,
};
pub use digest::{Digest, DigestError};
pub use download::{DownloadOutcome, parse_download_output};
pub use error::{
    BootcError, DETAIL_MAX_CHARS, ErrorKind, ExitState, classify_failure, summarize_stderr,
};
#[cfg(any(test, feature = "testing"))]
pub use fake::{FakeRunner, Scripted};
pub use lock::{SYSROOT_LOCK_PATH, sysroot_lock_held};
pub use progress::{ByteProgress, Progress, ProgressDecoder, Stage};
pub use query::{DeploymentLocation, LocationError, SYSROOT};
pub use runner::{
    CancelToken, CommandOutput, CommandRunner, Invocation, KILL_GRACE, OUTPUT_LIMIT, TokioRunner,
};
pub use status::{
    API_GROUP_PREFIX, API_VERSION_V1, BootEntry, BootEntryComposefs, BootEntryOstree, BootOrder,
    BootType, Bootloader, FilesystemOverlay, FilesystemOverlayAccessMode,
    FilesystemOverlayPersistence, Host, HostSpec, HostStatus, HostType, ImageReference,
    ImageSignature, ImageStatus, KIND_BOOTC_HOST, ObjectMeta, StatusParseError, Store,
};
pub use time::parse_rfc3339_unix;
