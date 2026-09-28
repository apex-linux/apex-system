//! `apex-updated`: the Apex update service (ARCHITECTURE §5, ADR 0008, ADR 0009).
//!
//! It serves `io.github.sekirokenjii.Apex.Update1` on the system bus and is the only
//! component that runs bootc. The library holds everything except process setup, so the
//! integration tests can drive the real code with fakes:
//!
//! - [`engine`]: executes operations. Every decision comes from the pure `apex-update`
//!   engine; this module gathers the [`apex_update::SystemSnapshot`], runs bootc through
//!   `apex-bootc`, arms the boot counter through `apex-boot`, and keeps the published
//!   [`properties::Properties`] up to date.
//! - [`queue`]: the single operation queue and its admission rules (which calls queue and
//!   which fail fast with `Busy`); [`daemon`] ties it to the engine and runs the worker.
//! - [`service`]: the `#[zbus::interface]` and the `PropertiesChanged` emitter.
//! - [`auth`]: polkit (system-bus-name subject) and the uid-0 check, behind a trait.
//! - [`providers`]: power, metered network, free space, boot payload, unit states, the
//!   deployments' files and the GRUB environment block, each behind a trait.
//! - [`store`], [`channel`], [`view`], [`errors`], [`civil`], [`text`]: small pure helpers.
//!
//! # Lifecycle
//!
//! The service is D-Bus activated. It runs an initial refresh (waiting at most
//! [`engine::PING_WAIT`] for it), requests its name, signals every property once, notifies
//! systemd and exits after [`IDLE_TIMEOUT`] without activity (see [`queue::Activity`]). If
//! the operation worker dies (a panic), it exits with a failure instead of lingering.

#![warn(missing_docs)]

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub mod auth;
pub mod channel;
pub mod civil;
pub mod daemon;
pub mod engine;
pub mod errors;
pub mod properties;
pub mod providers;
pub mod queue;
pub mod service;
pub mod store;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod text;
pub mod view;

/// A boxed, sendable future: the return type of the dyn-compatible provider traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where the binary is installed (M1 contract §2).
pub const BINARY_PATH: &str = "/usr/libexec/apex/apex-updated";

/// The service exits after this long without activity (see [`daemon::Daemon::is_idle`]).
pub const IDLE_TIMEOUT: Duration = Duration::from_mins(5);

/// `Check` calls within this long of the last successful check return at once (XML
/// contract).
pub const CHECK_RATE_LIMIT_SECS: u64 = 60;

/// A method call refreshes the state first when the last refresh is older than this.
pub const REFRESH_MAX_AGE: Duration = Duration::from_secs(10);
