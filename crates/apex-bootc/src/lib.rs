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
