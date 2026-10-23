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

    pub fn allow_all() -> Self {
        Self::new(true, 0)
    }

    pub fn allow_user(uid: u32) -> Self {
        Self::new(true, uid)
    }

    pub fn deny_user(uid: u32) -> Self {
        Self::new(false, uid)
    }

    /// The method's D-Bus name and the action, in order.
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

#[derive(Debug, Clone)]
pub struct FakeState {
    pub now: u64,
    pub power: Power,
    pub metered: Option<bool>,
    pub sysroot_free: Option<u64>,
    pub boot_free: Option<u64>,
    pub boot_payload: Option<u64>,
    /// `ActiveState` by unit name.
    pub units: HashMap<String, String>,
    pub running_manifest: Option<Manifest>,
    /// By ostree commit checksum.
    pub manifests: HashMap<String, Manifest>,
    pub finalize_stamp: bool,
}

impl Default for FakeState {
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
            finalize_stamp: false,
        }
    }
}

#[derive(Debug, Default)]
pub struct FakeSystem {
    state: Mutex<FakeState>,
}

impl FakeSystem {
    pub fn new(state: FakeState) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(state),
        })
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn update(&self, change: impl FnOnce(&mut FakeState)) {
        change(&mut self.lock());
    }

    pub fn state(&self) -> FakeState {
        self.lock().clone()
    }

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

    fn finalize_failure_stamp(&self) -> bool {
        self.lock().finalize_stamp
    }
}

#[derive(Debug, Clone, Default)]
pub struct SharedGrubWriter {
    inner: Arc<Mutex<RecordingWriter>>,
}

impl SharedGrubWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(env: GrubEnv) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RecordingWriter::new(env))),
        }
    }

    /// `call` counts from zero.
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

    pub fn calls(&self) -> Vec<EditenvCall> {
        self.lock().calls().to_vec()
    }

    pub fn env(&self) -> GrubEnv {
        self.lock().env().clone()
    }

    pub fn boxed(&self) -> Box<dyn GrubEnvWriter + Send> {
        Box::new(self.clone())
    }

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
