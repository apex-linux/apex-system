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
    AfterCheck, ApplyError, AutomaticPlan, DownloadError, intent_is_orphaned, plan_after_check,
    plan_automatic, validate_apply, validate_download,
};
pub use records::{
    DenyEntry, Denylist, FallbackRecord, HealthFailureRecord, Intent, LastCheck, MajorPinRecord,
    MajorUpgrades, PinRecord, Pins, SnapshotRecord, UpdateSettings,
};
pub use rpmvercmp::{Evr, EvrRef, rpmvercmp};
pub use snapshot::{DeploymentView, HostView, Power, RemoteView, SystemSnapshot};
pub use state::{State, derive_state};
pub use store::{MemoryState, MemoryStore, StateDir, StateStore, StoreError};
pub use version::Version;
