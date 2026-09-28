//! The Apex update engine: pure decisions over a snapshot of the system.
//!
//! `apex-updated` gathers a [`SystemSnapshot`] (bootc status, power, network, free
//! space and the persisted files) and asks this crate what to do. Nothing here runs
//! bootc, talks to D-Bus, reads the clock or uses async; the only I/O is the
//! [`store`] module's atomic JSON files. The crate does not depend on `apex-bootc`: the
//! daemon maps bootc's JSON into [`HostView`], so each side can change on its own.
//!
//! - [`version`]: Apex image versions (`45.20260924.117`), ordered numerically.
//! - [`mod@rpmvercmp`]: a faithful port of rpm's version comparison, and EVR ordering.
//! - [`manifest`]: `manifest.json`, the package diff and its one-line summary.
//! - [`gates`]: power, metered, space, denylist, bootc flags, version and intent.
//! - [`state`]: the derived `State` property and the staleness rule.
//! - [`plan`]: the automatic run and the validation of user operations.
//! - [`records`] and [`store`]: `intent.json`, `denylist.json`, `last-check.json` and
//!   `health-failure.json`.

#![warn(missing_docs)]

pub mod format;
pub mod gates;
pub mod manifest;
pub mod plan;
pub mod records;
pub mod rpmvercmp;
pub mod snapshot;
pub mod state;
pub mod store;
pub mod version;

#[cfg(test)]
mod test_support;

pub use gates::{Deferral, Gate, Operation, Trigger};
pub use manifest::{
    Change, ChangeKind, Manifest, ManifestError, Package, Summary, diff, summarize,
};
pub use plan::{
    AfterCheck, ApplyError, AutomaticPlan, DiscardError, DownloadError, intent_is_orphaned,
    plan_after_check, plan_automatic, validate_apply, validate_discard, validate_download,
};
pub use records::{DenyEntry, Denylist, HealthFailureRecord, Intent, LastCheck};
pub use rpmvercmp::{Evr, EvrRef, rpmvercmp};
pub use snapshot::{DeploymentView, HostView, Power, RemoteView, SystemSnapshot};
pub use state::{State, derive_state, is_stale};
pub use store::{MemoryState, MemoryStore, StateDir, StateStore, StoreError};
pub use version::Version;
