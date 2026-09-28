//! Test doubles (cargo feature `testing`; the daemon never enables it).
//!
//! - [`FakeAuthorizer`]: allows every polkit action (or refuses all), with a chosen caller
//!   uid for the root-only methods, and records which action each method asked for.
//! - [`FakeSystem`]: every provider in one scriptable object.
//! - [`SharedGrubWriter`]: `apex_boot::RecordingWriter` behind a shared handle, so a test
//!   can read the recorded `grub2-editenv` calls after the engine used the writer. It is
//!   also the engine's [`GrubEnvSource`], so what the engine reads is what it wrote.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use apex_boot::grubenv::ReadError;
use apex_boot::writer::EditenvCall;
use apex_boot::{GrubEnv, GrubEnvWriter, RecordingWriter, WriteError};
use apex_dbus::{ApexError, PolkitAction};
use apex_update::{Manifest, Power};
use zbus::message::Header;

use crate::BoxFuture;
use crate::auth::{Authorizer, refusal};
use crate::providers::{
    BOOT, BootPayload, Clock, DeploymentFiles, DiskSpace, FileError, GrubEnvSource, ManifestSource,
    MeteredSource, PowerSource, Providers, SYSROOT, UnitStates,
};

/// An [`Authorizer`] that answers without polkit or a bus, and remembers which action each
/// check asked for ([`FakeAuthorizer::requests`]; clones share the record).
#[derive(Debug, Clone)]
pub struct FakeAuthorizer {
    allow: bool,
    uid: u32,
    requests: Arc<Mutex<Vec<(String, PolkitAction)>>>,
}

impl FakeAuthorizer {
    fn new(allow: bool, uid: u32) -> Self {
        Self {
            allow,
            uid,
            requests: Arc::default(),
        }
    }

    /// Allows every action; the caller is root.
    pub fn allow_all() -> Self {
        Self::new(true, 0)
    }

    /// Allows every polkit action; the caller is `uid`.
    pub fn allow_user(uid: u32) -> Self {
        Self::new(true, uid)
    }

    /// Refuses every polkit action; the caller is `uid`.
    pub fn deny_user(uid: u32) -> Self {
        Self::new(false, uid)
    }

    /// Every polkit check so far, in order: the method's D-Bus name and the action asked for.
    pub fn requests(&self) -> Vec<(String, PolkitAction)> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Authorizer for FakeAuthorizer {
    fn authorize<'a>(
        &'a self,
        header: &'a Header<'_>,
        action: PolkitAction,
    ) -> BoxFuture<'a, Result<(), ApexError>> {
        let member = header.member().map(ToString::to_string).unwrap_or_default();
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((member, action));
        let result = if self.allow {
            Ok(())
        } else {
            Err(refusal(action, false, false))
        };
        Box::pin(async move { result })
    }

    fn caller_uid<'a>(&'a self, _header: &'a Header<'_>) -> BoxFuture<'a, Result<u32, ApexError>> {
        let uid = self.uid;
        Box::pin(async move { Ok(uid) })
    }
}

/// What [`FakeSystem`] reports.
#[derive(Debug, Clone)]
pub struct FakeState {
    /// Unix seconds.
    pub now: u64,
    /// Power supply.
    pub power: Power,
    /// Metered network.
    pub metered: Option<bool>,
    /// Free bytes on `/sysroot`.
    pub sysroot_free: Option<u64>,
    /// Free bytes on `/boot`.
    pub boot_free: Option<u64>,
    /// Kernel plus initramfs of any tree.
    pub boot_payload: Option<u64>,
    /// `ActiveState` by unit name.
    pub units: HashMap<String, String>,
    /// The running image's manifest.
    pub running_manifest: Option<Manifest>,
    /// Deployment manifests by ostree commit checksum.
    pub manifests: HashMap<String, Manifest>,
    /// os-release `VERSION_ID`.
    pub fedora: Option<u32>,
    /// The finalization failure stamp exists.
    pub finalize_stamp: bool,
}

impl Default for FakeState {
    /// A healthy laptop on AC: plenty of space, not metered, nothing running.
    fn default() -> Self {
        Self {
            now: 1_790_300_000,
            power: Power {
                on_ac: true,
                battery_percent: Some(80),
            },
            metered: Some(false),
            sysroot_free: Some(100 * 1024 * 1024 * 1024),
            boot_free: Some(600 * 1024 * 1024),
            boot_payload: Some(120 * 1024 * 1024),
            units: HashMap::new(),
            running_manifest: None,
            manifests: HashMap::new(),
            fedora: Some(45),
            finalize_stamp: false,
        }
    }
}

/// Every provider, answering from a [`FakeState`] the test can change at any time.
#[derive(Debug, Default)]
pub struct FakeSystem {
    state: Mutex<FakeState>,
}

impl FakeSystem {
    /// A fake answering from `state`.
    pub fn new(state: FakeState) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes what the fake reports.
    pub fn update(&self, change: impl FnOnce(&mut FakeState)) {
        change(&mut self.lock());
    }

    /// A copy of what the fake reports.
    pub fn state(&self) -> FakeState {
        self.lock().clone()
    }

    /// Providers that all answer from this fake.
    pub fn providers(self: &Arc<Self>) -> Providers {
        Providers {
            clock: self.clone(),
            power: self.clone(),
            metered: self.clone(),
            space: self.clone(),
            payload: self.clone(),
            units: self.clone(),
            files: self.clone(),
        }
    }
}

impl Clock for FakeSystem {
    fn now(&self) -> u64 {
        self.lock().now
    }
}

impl PowerSource for FakeSystem {
    fn power(&self) -> BoxFuture<'_, Power> {
        let power = self.lock().power;
        Box::pin(async move { power })
    }
}

impl MeteredSource for FakeSystem {
    fn metered(&self) -> BoxFuture<'_, Option<bool>> {
        let metered = self.lock().metered;
        Box::pin(async move { metered })
    }
}

impl DiskSpace for FakeSystem {
    fn free_bytes(&self, path: &Path) -> Option<u64> {
        let state = self.lock();
        if path == Path::new(SYSROOT) {
            state.sysroot_free
        } else if path == Path::new(BOOT) {
            state.boot_free
        } else {
            None
        }
    }
}

impl BootPayload for FakeSystem {
    fn boot_payload(&self, _root: &Path) -> Option<u64> {
        self.lock().boot_payload
    }
}

impl UnitStates for FakeSystem {
    fn active_state<'a>(&'a self, unit: &'a str) -> BoxFuture<'a, Option<String>> {
        let state = self.lock().units.get(unit).cloned();
        Box::pin(async move { state })
    }
}

impl DeploymentFiles for FakeSystem {
    fn manifest<'a>(
        &'a self,
        source: &'a ManifestSource,
    ) -> BoxFuture<'a, Result<Option<Manifest>, FileError>> {
        let state = self.lock();
        let manifest = match source {
            ManifestSource::Running => state.running_manifest.clone(),
            ManifestSource::Deployment(location) => {
                state.manifests.get(location.checksum()).cloned()
            }
        };
        Box::pin(async move { Ok(manifest) })
    }

    fn fedora_release(&self) -> Option<u32> {
        self.lock().fedora
    }

    fn finalize_failure_stamp(&self) -> bool {
        self.lock().finalize_stamp
    }
}

/// A [`RecordingWriter`] the test keeps a handle to. Cloning shares it.
#[derive(Debug, Clone, Default)]
pub struct SharedGrubWriter {
    inner: Arc<Mutex<RecordingWriter>>,
}

impl SharedGrubWriter {
    /// A writer over a fresh 1024-byte environment block (as `grub2-editenv create`).
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer over `env`, for a boot counter the test sets up beforehand (any values,
    /// also ones `grub2-editenv` calls from Apex would never write).
    pub fn with_env(env: GrubEnv) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RecordingWriter::new(env))),
        }
    }

    /// A writer whose call number `call` (zero-based) fails.
    pub fn failing_at(call: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(
                RecordingWriter::new(GrubEnv::new()).failing_at(call),
            )),
        }
    }

    fn lock(&self) -> MutexGuard<'_, RecordingWriter> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<EditenvCall> {
        self.lock().calls().to_vec()
    }

    /// The environment block after the successful calls.
    pub fn env(&self) -> GrubEnv {
        self.lock().env().clone()
    }

    /// A boxed handle for `EngineParts::grub`.
    pub fn boxed(&self) -> Box<dyn GrubEnvWriter + Send> {
        Box::new(self.clone())
    }

    /// A shared handle for `EngineParts::grubenv`: reads see the writes.
    pub fn source(&self) -> Arc<dyn GrubEnvSource> {
        Arc::new(self.clone())
    }
}

impl GrubEnvSource for SharedGrubWriter {
    fn read(&self) -> Result<Option<GrubEnv>, ReadError> {
        Ok(Some(self.env()))
    }
}

impl GrubEnvWriter for SharedGrubWriter {
    fn set(&mut self, assignments: &[(&str, &str)]) -> Result<(), WriteError> {
        self.lock().set(assignments)
    }

    fn unset(&mut self, names: &[&str]) -> Result<(), WriteError> {
        self.lock().unset(names)
    }
}
