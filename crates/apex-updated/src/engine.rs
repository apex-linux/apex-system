use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use apex_boot::writer::GRUBENV_PATH;
use apex_boot::{
    ArmingRefused, BootVars, Editenv, EnvOp, GrubEnvWriter, WriteError, apply_ops, arm_ops,
    check_arming, disarm_ops,
};
use apex_bootc::{BootEntry, Bootc, CancelToken, CommandRunner, DownloadOutcome, Host};
use apex_dbus::{ApexError, BootVerdict, Change, Channel, ErrorKind, Hold};
use apex_update::state::is_available;
use apex_update::{
    AfterCheck, ApplyError, AutomaticPlan, DenyEntry, FallbackRecord, HealthFailureRecord,
    HostView, Intent, LastCheck, Manifest, RemoteView, SystemSnapshot, Trigger, UpdateSettings,
    Version, diff, intent_is_orphaned, plan_after_check, plan_automatic, validate_apply,
    validate_download,
};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::REFRESH_MAX_AGE;
use crate::channel::channel_of_image;
use crate::errors::{self, from_bootc, from_store};
use crate::lane_lock;
use crate::properties::{self, Properties, Work};
use crate::providers::{
    ASSESS_UNIT, BOOT, BOOT_COMPLETE_UNIT, FileError, GrubEnvSource, ManifestSource, Providers,
    SYSROOT,
};
use crate::queue::Op;
use crate::store::{Store, unflushed_is_done};
use crate::view::{self, Manifests, RemoteImage, deployment_dict, image_timestamp};

// apex-boot-assess's DAEMON_PING_WAIT must equal this; its test reads this line.
pub const PING_WAIT: Duration = Duration::from_secs(20);

/// Shorter than the 15 s `main` gives a cancelled operation at shutdown, so the arming
/// decision is still made then.
pub const CONFIRM_WAIT: Duration = Duration::from_secs(10);

pub const GRUBENV_READ_WAIT: Duration = Duration::from_secs(10);

pub const FALLBACK_REASON: &str =
    "The update did not start correctly in three attempts, so the previous version was restored.";

pub const MARK_BAD_REASON: &str = "Marked as bad when going back to the previous version.";

pub const STATUS_SKIPPED: &str =
    "The update state is not read again while an update operation is running.";

/// Remounts /boot read-write in a private mount namespace: bootc keeps /boot read-only, and
/// the host must never see it writable. `apex-updated.service` must allow this.
pub fn grubenv_writer() -> Editenv {
    Editenv::system()
}

pub struct EngineParts<R> {
    pub runner: R,
    pub store: Arc<dyn Store>,
    pub grub: Box<dyn GrubEnvWriter + Send>,
    pub grubenv: Arc<dyn GrubEnvSource>,
    pub providers: Providers,
}

impl<R: fmt::Debug> fmt::Debug for EngineParts<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineParts")
            .field("runner", &self.runner)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ArmingError {
    #[error(
        "The running system is not a container image deployment, so the previous version could not be restored if the update failed."
    )]
    NoBootedDeployment,
    #[error("There is no downloaded update to install.")]
    NothingStaged,
    #[error("The downloaded update is the version that is already running.")]
    StagedIsBooted,
}

/// The fallback goes to GRUB entry 1, which ostree's finalization makes the booted
/// deployment; it must be a real one, never the firmware-setup entry.
pub fn arming_precondition(host: &HostView) -> Result<(), ArmingError> {
    let booted = host
        .booted
        .as_ref()
        .ok_or(ArmingError::NoBootedDeployment)?;
    let staged = host.staged.as_ref().ok_or(ArmingError::NothingStaged)?;
    if staged.digest == booted.digest {
        return Err(ArmingError::StagedIsBooted);
    }
    Ok(())
}

pub fn arming_refusal(refused: &ArmingRefused) -> ApexError {
    let text = match refused {
        ArmingRefused::StagedIsBooted => ArmingError::StagedIsBooted.to_string(),
        ArmingRefused::BootedUnconfirmed { .. } => "The running version has not passed its startup check yet, and installing another update now would remove the last version known to work. Wait until the check has passed, or go back to the previous version.".to_owned(),
        ArmingRefused::BootedFailed { .. } => "The running version failed its startup check. Go back to the previous version first, so that a version known to work stays installed.".to_owned(),
    };
    ApexError::PolicyDenied(text)
}

pub fn remote_view(check: &LastCheck) -> Option<RemoteView> {
    check.remote_digest.as_ref().map(|digest| RemoteView {
        digest: digest.clone(),
        version: check.remote_version.as_deref().and_then(Version::parse),
    })
}

pub fn change_tuple(change: &apex_update::Change) -> Change {
    let evr =
        |e: &Option<apex_update::Evr>| e.as_ref().map(ToString::to_string).unwrap_or_default();
    (
        change.name.clone(),
        change.arch.clone(),
        change.kind.as_str().to_owned(),
        evr(&change.from_evr),
        evr(&change.to_evr),
    )
}

pub fn changes_reply(
    booted: &[apex_update::Package],
    staged: &[apex_update::Package],
) -> Vec<Change> {
    let mut changes: Vec<Change> = diff(booted, staged).iter().map(change_tuple).collect();
    // The interface requires this order; do not rely on `diff` keeping it.
    changes.sort_unstable_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    changes
}

#[derive(Debug, Clone, Default)]
struct HostSummary {
    booted: apex_dbus::Deployment,
    staged: apex_dbus::Deployment,
    rollback: apex_dbus::Deployment,
    channel: Option<Channel>,
    spec_image: Option<String>,
}

#[derive(Debug, Clone)]
struct RefreshFailure {
    message: String,
    /// Fails `Ping`, so the boot assessment sees a broken updater.
    bootc_broken: bool,
}

#[derive(Debug, Default)]
struct ManifestCache {
    running: RunningManifest,
    /// By ostree commit checksum, which names the content.
    deployments: HashMap<String, Option<Arc<Manifest>>>,
    running_holds: RunningHolds,
    /// `None` when the image has no lane lock.
    deployment_holds: HashMap<String, Option<Vec<Hold>>>,
}

/// `Read(None)` when the running image has no lane lock.
#[derive(Debug, Clone, Default)]
enum RunningHolds {
    #[default]
    NotRead,
    Read(Option<Vec<Hold>>),
}

#[derive(Debug, Clone, Default)]
enum RunningManifest {
    #[default]
    NotRead,
    Read(Option<Arc<Manifest>>),
}

#[derive(Debug, Default)]
struct EngineState {
    host: Option<Host>,
    snapshot: Option<SystemSnapshot>,
    summary: HostSummary,
    refreshed_at: Option<Instant>,
    refresh_failure: Option<RefreshFailure>,
    last_check: LastCheck,
    /// Digest and build time, when a check in this process learned it.
    remote_timestamp: Option<(String, u64)>,
    manifests: ManifestCache,
    work: Option<Work>,
    last_error: String,
    settings: UpdateSettings,
    last_fallback: Option<String>,
}

pub struct Engine<R> {
    bootc: Bootc<R>,
    running_bootc_commands: AtomicUsize,
    store: Arc<dyn Store>,
    grub: Arc<Mutex<Box<dyn GrubEnvWriter + Send>>>,
    grubenv: Arc<dyn GrubEnvSource>,
    providers: Providers,
    state: Mutex<EngineState>,
    published: watch::Sender<Properties>,
    /// Set once a refresh has determined the state, or recorded why it could not.
    settled: watch::Sender<bool>,
    first_refresh_deadline: Instant,
}

struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn start(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<R: fmt::Debug> fmt::Debug for Engine<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("bootc", &self.bootc)
            .field("providers", &self.providers)
            .finish_non_exhaustive()
    }
}

impl<R: CommandRunner> Engine<R> {
    pub fn new(parts: EngineParts<R>) -> Self {
        Self {
            bootc: Bootc::new(parts.runner),
            running_bootc_commands: AtomicUsize::new(0),
            store: parts.store,
            grub: Arc::new(Mutex::new(parts.grub)),
            grubenv: parts.grubenv,
            providers: parts.providers,
            state: Mutex::new(EngineState::default()),
            published: watch::Sender::new(Properties::default()),
            settled: watch::Sender::new(false),
            first_refresh_deadline: Instant::now() + PING_WAIT,
        }
    }

    /// Until then the properties are placeholders that read as "no such deployment". The
    /// deadline is absolute: a slow first refresh delays callers by at most `PING_WAIT`.
    pub async fn first_refresh(&self) {
        let mut settled = self.settled.subscribe();
        let _ = tokio::time::timeout_at(
            self.first_refresh_deadline,
            settled.wait_for(|settled| *settled),
        )
        .await;
    }

    fn mark_settled(&self) {
        self.settled
            .send_if_modified(|settled| !std::mem::replace(settled, true));
    }

    pub fn subscribe(&self) -> watch::Receiver<Properties> {
        self.published.subscribe()
    }

    pub fn properties(&self) -> Properties {
        self.published.borrow().clone()
    }

    fn lock(&self) -> MutexGuard<'_, EngineState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> u64 {
        self.providers.clock.now()
    }

    /// bootc holds the sysroot lock for a whole command, and `bootc status` would wait for it.
    pub fn bootc_busy(&self) -> bool {
        self.running_bootc_commands.load(Ordering::SeqCst) > 0
    }

    async fn marking_bootc_busy<T>(&self, command: impl Future<Output = T>) -> T {
        let _in_flight = InFlight::start(&self.running_bootc_commands);
        command.await
    }

    pub fn refresh_due(&self) -> bool {
        self.lock()
            .refreshed_at
            .is_none_or(|at| at.elapsed() >= REFRESH_MAX_AGE)
    }

    fn publish(&self) {
        let properties = build_properties(&self.lock()).sanitized();
        self.published.send_if_modified(|current| {
            if *current == properties {
                false
            } else {
                *current = properties;
                true
            }
        });
    }

    fn set_work(&self, work: Option<Work>) {
        self.lock().work = work;
        self.publish();
    }

    fn set_progress(&self, fraction: f64) {
        {
            let mut state = self.lock();
            if let Some(Work::Downloading { progress }) = &mut state.work {
                *progress = Some(fraction);
            }
        }
        self.publish();
    }

    fn snapshot(&self) -> Result<SystemSnapshot, ApexError> {
        let snapshot = self.lock().snapshot.clone();
        snapshot.ok_or_else(|| self.state_unknown())
    }

    fn state_unknown(&self) -> ApexError {
        match &self.lock().refresh_failure {
            Some(failure) => ApexError::Internal(format!(
                "The update state could not be determined: {}",
                failure.message
            )),
            None => ApexError::Busy(
                "The update state is not known yet. Try again in a moment.".to_owned(),
            ),
        }
    }

    /// Any active-session process may call `Apply` without a prompt, so made-up digests must
    /// not each queue a full refresh. The queued operation still decides on its own refresh.
    pub fn precheck_apply(&self, expected: &str) -> Result<(), ApexError> {
        let state = self.lock();
        let fresh = state
            .refreshed_at
            .is_some_and(|at| at.elapsed() < REFRESH_MAX_AGE);
        let Some(snapshot) = state.snapshot.as_ref().filter(|_| fresh) else {
            return Ok(());
        };
        let refusal = match snapshot.host.staged.as_ref() {
            None => ApplyError::NothingStaged,
            Some(staged) if staged.digest != expected => ApplyError::DigestMismatch {
                expected: expected.to_owned(),
                staged: staged.digest.clone(),
            },
            Some(staged) if !staged.download_only => ApplyError::AlreadyApplied,
            Some(_) => return Ok(()),
        };
        Err(errors::from_apply(&refusal))
    }

    fn host(&self) -> Option<Host> {
        self.lock().host.clone()
    }

    pub async fn execute(&self, op: &Op, cancel: &CancelToken) -> Result<(), ApexError> {
        let result = match op {
            Op::Refresh => return self.refresh(cancel).await,
            Op::Check => self.check(cancel).await,
            Op::Download {
                override_soft_gates,
            } => self.download_op(*override_soft_gates, cancel).await,
            Op::Apply { expected_digest } => self.apply(expected_digest, cancel).await,
            Op::Rollback { mark_bad } => self.rollback(*mark_bad, cancel).await,
            Op::Automatic => self.automatic(cancel).await,
            Op::Fallback { digest } => self.fallback(digest, cancel).await,
        };
        match &result {
            Ok(()) => self.lock().last_error.clear(),
            Err(error) if errors::is_recorded(error) => {
                self.lock().last_error = error.message();
            }
            Err(_) => {}
        }
        self.publish();
        result
    }

    pub async fn refresh(&self, cancel: &CancelToken) -> Result<(), ApexError> {
        let host = self.read_status(cancel).await?;
        self.rebuild(host).await
    }

    async fn refresh_after_acting(&self, cancel: &CancelToken) {
        if let Err(error) = self.refresh(cancel).await {
            tracing::warn!(error = %error.message(), "refresh after the operation failed");
        }
    }

    /// Skipped while this engine's own bootc command holds the sysroot lock; the skip records
    /// nothing, as the operation reads the status itself afterwards.
    async fn read_status(&self, cancel: &CancelToken) -> Result<Host, ApexError> {
        if self.bootc_busy() {
            tracing::debug!("bootc status not run: this daemon's bootc command is running");
            return Err(ApexError::Busy(STATUS_SKIPPED.to_owned()));
        }
        let host = match self.bootc.status(cancel).await {
            Ok(host) => host,
            Err(error) => {
                // A held lock or a slow bootc leaves the last known state standing.
                let transient = error.is_transient()
                    || matches!(error, apex_bootc::BootcError::Cancelled { .. });
                self.record_refresh_failure(&errors::with_sources(&error), !transient, transient);
                return Err(from_bootc(&error));
            }
        };
        if let Err(error) = host.require_bootc_host() {
            self.record_refresh_failure(&error.to_string(), true, false);
            return Err(from_bootc(&error));
        }
        Ok(host)
    }

    fn record_refresh_failure(&self, message: &str, bootc_broken: bool, transient: bool) {
        if transient {
            tracing::warn!(error = message, "cannot determine the update state now");
        } else {
            tracing::error!(error = message, "cannot determine the update state");
        }
        {
            let mut state = self.lock();
            if transient && state.snapshot.is_some() {
                drop(state);
                self.mark_settled();
                return;
            }
            state.refresh_failure = Some(RefreshFailure {
                message: message.to_owned(),
                bootc_broken,
            });
            state.last_error = format!("The update state could not be determined: {message}");
        }
        self.publish();
        self.mark_settled();
    }

    async fn probe(&self, host: &Host) -> Probes {
        let (power, metered, boot_complete) = tokio::join!(
            self.providers.power.power(),
            self.providers.metered.metered(),
            self.providers.units.active_state(BOOT_COMPLETE_UNIT),
        );
        let space = &self.providers.space;
        let payload = &self.providers.payload;
        Probes {
            power,
            metered,
            finalize_failed: self.providers.files.finalize_failure_stamp()
                || boot_complete.as_deref() == Some("failed"),
            sysroot_free: space.free_bytes(Path::new(SYSROOT)).unwrap_or(0),
            boot_free: space.free_bytes(Path::new(BOOT)).unwrap_or(0),
            // Finalization copies the staged kernel; the running one is the fallback estimate.
            boot_payload: host
                .staged()
                .and_then(|entry| entry.deployment_root().ok())
                .and_then(|root| payload.boot_payload(&root))
                .or_else(|| payload.boot_payload(Path::new("/"))),
        }
    }

    /// The state files are read last, under the state lock, so records read before a direct
    /// operation changed them under the same lock never overwrite its change.
    async fn rebuild(&self, host: Host) -> Result<(), ApexError> {
        let probes = self.probe(&host).await;
        let running = logged(self.running_manifest().await, "the running image");
        let staged = match host.staged() {
            Some(entry) => logged(
                self.deployment_manifest(entry).await,
                "the staged deployment",
            ),
            None => None,
        };
        let rollback = match host.rollback() {
            Some(entry) => logged(
                self.deployment_manifest(entry).await,
                "the rollback deployment",
            ),
            None => None,
        };
        let manifests = Manifests {
            booted: running.as_deref(),
            staged: staged.as_deref(),
            rollback: rollback.as_deref(),
        };
        let host_view = view::host_view(&host, &manifests, probes.finalize_failed);
        let mut summary = summarize(&host, &manifests);
        self.attach_holds(&host, &mut summary).await;

        let mut state = self.lock();
        let records = match read_records(self.store.as_ref()) {
            Ok(records) => records,
            Err(error) => {
                drop(state);
                let mapped = from_store(&error);
                self.record_refresh_failure(&mapped.message(), false, false);
                return Err(mapped);
            }
        };
        match self.store.read_settings() {
            Ok(settings) => state.settings = settings,
            Err(error) => {
                tracing::warn!(error = %errors::with_sources(&error), "ignoring settings.json");
            }
        }
        state.last_fallback = records.fallback.map(|record| record.digest);
        let file_check = self.store.read_last_check().unwrap_or_else(|error| {
            tracing::warn!(error = %errors::with_sources(&error), "ignoring last-check.json");
            LastCheck::default()
        });
        // A check recorded in this process wins over a file that could not be written.
        if file_check.at >= state.last_check.at {
            state.last_check = file_check;
        }
        let mut snapshot = SystemSnapshot {
            power: probes.power,
            metered: probes.metered,
            sysroot_free: probes.sysroot_free,
            boot_free: probes.boot_free,
            image_size: None,
            boot_payload: probes.boot_payload,
            host: host_view,
            remote: remote_view(&state.last_check),
            intent: records.intent,
            denylist: records.denylist,
            health_failure: records.health.map(|record| record.digest),
            now: self.now(),
        };
        if intent_is_orphaned(&snapshot) {
            tracing::info!(
                intent = ?snapshot.intent,
                "clearing the intent: its update is neither staged to boot nor running"
            );
            match unflushed_is_done(self.store.clear_intent(), "clearing the intent") {
                Ok(()) => snapshot.intent = None,
                Err(error) => {
                    tracing::warn!(error = %errors::with_sources(&error), "cannot clear the intent");
                }
            }
        }
        let keep: Vec<String> = [host.booted(), host.staged(), host.rollback()]
            .into_iter()
            .flatten()
            .filter_map(|e| e.ostree.as_ref().map(|o| o.checksum.clone()))
            .collect();
        state
            .manifests
            .deployments
            .retain(|checksum, _| keep.contains(checksum));
        state
            .manifests
            .deployment_holds
            .retain(|checksum, _| keep.contains(checksum));
        state.host = Some(host);
        state.snapshot = Some(snapshot);
        state.summary = summary;
        if state.refresh_failure.take().is_some() {
            state.last_error.clear();
        }
        state.refreshed_at = Some(Instant::now());
        drop(state);
        self.publish();
        self.mark_settled();
        Ok(())
    }

    async fn running_manifest(&self) -> Result<Option<Arc<Manifest>>, FileError> {
        if let RunningManifest::Read(cached) = self.lock().manifests.running.clone() {
            return Ok(cached);
        }
        let manifest = self
            .providers
            .files
            .manifest(&ManifestSource::Running)
            .await?
            .map(Arc::new);
        self.lock().manifests.running = RunningManifest::Read(manifest.clone());
        Ok(manifest)
    }

    async fn deployment_manifest(
        &self,
        entry: &BootEntry,
    ) -> Result<Option<Arc<Manifest>>, FileError> {
        let Ok(location) = entry.location() else {
            return Ok(None);
        };
        let checksum = location.checksum().to_owned();
        if let Some(cached) = self.lock().manifests.deployments.get(&checksum) {
            return Ok(cached.clone());
        }
        let manifest = self
            .providers
            .files
            .manifest(&ManifestSource::Deployment(location))
            .await?
            .map(Arc::new);
        self.lock()
            .manifests
            .deployments
            .insert(checksum, manifest.clone());
        Ok(manifest)
    }

    async fn attach_holds(&self, host: &Host, summary: &mut HostSummary) {
        summary.booted.holds = self.holds(None).await;
        if let Some(entry) = host.staged() {
            summary.staged.holds = self.holds(Some(entry)).await;
        }
        if let Some(entry) = host.rollback() {
            summary.rollback.holds = self.holds(Some(entry)).await;
        }
    }

    /// The running image's holds for `None`. A lock that cannot be read leaves the key out.
    async fn holds(&self, entry: Option<&BootEntry>) -> Option<Vec<Hold>> {
        let source = match entry {
            None => {
                if let RunningHolds::Read(cached) = self.lock().manifests.running_holds.clone() {
                    return cached;
                }
                ManifestSource::Running
            }
            Some(entry) => {
                let location = entry.location().ok()?;
                if let Some(cached) = self
                    .lock()
                    .manifests
                    .deployment_holds
                    .get(location.checksum())
                {
                    return cached.clone();
                }
                ManifestSource::Deployment(location)
            }
        };
        let holds = match self.providers.files.lane_lock(&source).await {
            Ok(lock) => lock.as_deref().map(lane_lock::holds),
            Err(error) => {
                tracing::warn!(error = %errors::with_sources(&error), "cannot read a lane lock");
                return None;
            }
        };
        let mut state = self.lock();
        match &source {
            ManifestSource::Running => {
                state.manifests.running_holds = RunningHolds::Read(holds.clone());
            }
            ManifestSource::Deployment(location) => {
                state
                    .manifests
                    .deployment_holds
                    .insert(location.checksum().to_owned(), holds.clone());
            }
        }
        holds
    }

    fn record_check(&self, record: LastCheck, timestamp: Option<u64>) {
        if let Err(error) = unflushed_is_done(
            self.store.write_last_check(&record),
            "saving last-check.json",
        ) {
            tracing::warn!(error = %errors::with_sources(&error), "cannot save last-check.json");
        }
        let mut state = self.lock();
        state.remote_timestamp = record.remote_digest.clone().zip(timestamp);
        if let Some(snapshot) = &mut state.snapshot {
            snapshot.remote = remote_view(&record);
        }
        state.last_check = record;
    }

    /// An earlier instance may have written the file, and a record that could not be saved
    /// is still known here.
    fn recorded_check(&self) -> LastCheck {
        let file = self.store.read_last_check().unwrap_or_else(|error| {
            tracing::warn!(error = %errors::with_sources(&error), "ignoring last-check.json");
            LastCheck::default()
        });
        let known = self.lock().last_check.clone();
        if file.at >= known.at { file } else { known }
    }

    async fn check(&self, cancel: &CancelToken) -> Result<(), ApexError> {
        let now = self.now();
        let outcome = self
            .marking_bootc_busy(self.bootc.check(cancel))
            .await
            .map_err(|e| from_bootc(&e))?;
        let host = self.read_status(cancel).await?;
        let remote = view::remote_after_check(&outcome, &host, &self.recorded_check());
        if remote.is_none() {
            tracing::warn!(?outcome, "the check did not name the channel's image");
        }
        let timestamp = remote.as_ref().and_then(|r| r.timestamp);
        let (remote_digest, remote_version, remote_size) = remote
            .map(
                |RemoteImage {
                     digest,
                     version,
                     size,
                     ..
                 }| (Some(digest), version, size),
            )
            .unwrap_or_default();
        self.record_check(
            LastCheck {
                at: now,
                remote_digest,
                remote_version,
                remote_size,
            },
            timestamp,
        );
        tracing::info!(?outcome, "checked for updates");
        self.rebuild(host).await
    }

    /// `downloading` stays published until the refreshed state replaces it, so clients never
    /// see the state fall back to `available` in between.
    async fn download(&self, trigger: Trigger, cancel: &CancelToken) -> Result<(), ApexError> {
        self.set_work(Some(Work::Downloading { progress: None }));
        let (sink, mut updates) = mpsc::channel(32);
        let run = self.marking_bootc_busy(self.bootc.upgrade_download_only(Some(sink), cancel));
        let forward = async {
            while let Some(update) = updates.recv().await {
                if let Some(fraction) = update.overall() {
                    self.set_progress(fraction);
                }
            }
        };
        let (result, ()) = tokio::join!(run, forward);
        let refreshed = self.after_staging(result.as_ref().ok(), cancel).await;
        let refused = if result.is_ok() && refreshed {
            self.screen_stage(cancel).await
        } else {
            None
        };
        self.set_work(None);
        let reported = result.map_err(|e| from_bootc(&e))?;
        tracing::info!(?reported, "download finished");
        self.refused_stage_result(refused, trigger)
    }

    /// The timer's run did its job, and `DeferredReason` explains the stage. For a user,
    /// `LastError` keeps the reason too: unlike a refusal, the download ran in vain.
    fn refused_stage_result(
        &self,
        refused: Option<String>,
        trigger: Trigger,
    ) -> Result<(), ApexError> {
        let Some(reason) = refused else {
            return Ok(());
        };
        match trigger {
            Trigger::Automatic => {
                tracing::info!(
                    reason,
                    "automatic run: the downloaded update is not offered"
                );
                Ok(())
            }
            Trigger::User => {
                self.lock().last_error.clone_from(&reason);
                Err(ApexError::PolicyDenied(reason))
            }
        }
    }

    /// `reported` is `None` when bootc failed: a stage left from before says nothing about
    /// the tag. Returns whether the state was refreshed.
    async fn after_staging(
        &self,
        reported: Option<&DownloadOutcome>,
        cancel: &CancelToken,
    ) -> bool {
        let host = match self.read_status(cancel).await {
            Ok(host) => host,
            Err(error) => {
                tracing::warn!(error = %error.message(), "refresh after the download failed");
                return false;
            }
        };
        if let Some(reported) = reported {
            self.follow_tag(reported, &host);
        }
        match self.rebuild(host).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(error = %error.message(), "refresh after the download failed");
                false
            }
        }
    }

    /// After a pull the tag points at the running image ("No update available.") or else at
    /// the stage. A later check that finds "no changes" relies on this record.
    fn follow_tag(&self, reported: &DownloadOutcome, host: &Host) {
        let entry = match reported {
            DownloadOutcome::NoUpdate => host.booted(),
            _ => host.staged(),
        };
        let Some(image) = entry.and_then(|entry| entry.image.as_ref()) else {
            return;
        };
        let known = self.lock().last_check.remote_digest.clone();
        if known.as_deref() != Some(image.image_digest.as_str()) {
            tracing::info!(digest = %image.image_digest, "the channel moved since the last check");
            self.record_check(
                LastCheck {
                    at: self.now(),
                    remote_digest: Some(image.image_digest.clone()),
                    remote_version: image.version.clone(),
                    remote_size: None,
                },
                image_timestamp(image),
            );
        }
    }

    /// The tag can move between the check and the pull. An unlocked stage here means bootc
    /// ignored `--download-only`, and nothing armed the counter for it.
    async fn screen_stage(&self, cancel: &CancelToken) -> Option<String> {
        let unlocked = self
            .lock()
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.host.has_unlocked_stage());
        if unlocked {
            self.relock_unexpected(cancel).await;
        }
        let refusal = properties::stage_refusal(self.lock().snapshot.as_ref()?);
        if let Some(reason) = &refusal {
            tracing::warn!(reason, "the downloaded update will not be installed");
        }
        refusal
    }

    async fn relock_unexpected(&self, cancel: &CancelToken) {
        tracing::error!("bootc left the downloaded update set to install; locking it again");
        let relock = self
            .marking_bootc_busy(self.bootc.upgrade_download_only(None, cancel))
            .await;
        match self.read_status(cancel).await {
            Ok(host) => {
                if host
                    .staged()
                    .is_some_and(|staged| !staged.is_download_only())
                {
                    tracing::error!(
                        ?relock,
                        "the downloaded update is still set to install at the next restart"
                    );
                }
                if let Err(error) = self.rebuild(host).await {
                    tracing::warn!(error = %error.message(), "refresh after the re-lock failed");
                }
            }
            Err(error) => {
                tracing::error!(?relock, error = %error.message(), "the re-lock cannot be confirmed");
            }
        }
    }

    async fn download_op(
        &self,
        override_soft_gates: bool,
        cancel: &CancelToken,
    ) -> Result<(), ApexError> {
        self.refresh(cancel).await?;
        let snapshot = self.snapshot()?;
        validate_download(&snapshot, override_soft_gates).map_err(|e| errors::from_download(&e))?;
        self.download(Trigger::User, cancel).await
    }

    async fn write_grubenv(&self, ops: Vec<EnvOp>) -> Result<(), WriteError> {
        let grub = Arc::clone(&self.grub);
        // grub2-editenv blocks for up to 30 s; D-Bus must stay responsive meanwhile.
        tokio::task::spawn_blocking(move || {
            let mut writer = grub.lock().unwrap_or_else(PoisonError::into_inner);
            apply_ops(&mut **writer, &ops)
        })
        .await
        .unwrap_or_else(|join| {
            Err(WriteError::Run {
                message: format!("the grubenv writer stopped: {join}"),
            })
        })
    }

    async fn disarm(&self) {
        if let Err(error) = self.write_grubenv(disarm_ops()).await {
            // Harmless: the boot assessment clears an arming whose target does not boot.
            tracing::warn!(error = %errors::with_sources(&error), "cannot disarm the boot counter");
        }
    }

    fn clear_intent(&self) {
        if let Err(error) = unflushed_is_done(self.store.clear_intent(), "clearing the intent") {
            tracing::warn!(error = %errors::with_sources(&error), "cannot clear the intent");
        }
    }

    /// The error is a sentence fragment, for a larger message.
    async fn boot_vars(&self) -> Result<BootVars, String> {
        let source = Arc::clone(&self.grubenv);
        let read = tokio::task::spawn_blocking(move || source.read());
        let env = match tokio::time::timeout(GRUBENV_READ_WAIT, read).await {
            Err(_) => {
                return Err(format!(
                    "reading {GRUBENV_PATH} took longer than {} seconds",
                    GRUBENV_READ_WAIT.as_secs()
                ));
            }
            Ok(Err(join)) => return Err(format!("the grubenv reader stopped: {join}")),
            Ok(Ok(Err(error))) => return Err(errors::with_sources(&error)),
            Ok(Ok(Ok(env))) => env,
        };
        env.map_or_else(
            || Ok(BootVars::default()),
            |env| BootVars::from_env(&env).map_err(|error| errors::with_sources(&error)),
        )
    }

    /// State that cannot be read refuses too: the precondition cannot be shown to hold.
    async fn confirm_arming(&self, booted: &str, staged: &str) -> Result<(), ApexError> {
        let parse = |digest: &str| {
            apex_boot::Digest::parse(digest).map_err(|error| {
                ApexError::Internal(format!(
                    "bootc reported the image digest {digest:?}, which is not valid: {error}."
                ))
            })
        };
        let booted = parse(booted)?;
        let staged = parse(staged)?;
        let vars = self.boot_vars().await.map_err(|reason| {
            ApexError::Internal(format!(
                "The state of the startup check could not be read, so the update was not installed: {reason}."
            ))
        })?;
        let failed = self
            .store
            .read_health_failure()
            .map_err(|error| from_store(&error))?
            .and_then(|record| apex_boot::Digest::parse(&record.digest).ok());
        check_arming(&vars, &booted, &staged, failed.as_ref()).map_err(|refused| {
            tracing::info!(reason = %refused, "not arming the boot counter");
            arming_refusal(&refused)
        })
    }

    async fn apply(&self, expected: &str, cancel: &CancelToken) -> Result<(), ApexError> {
        let target = apex_boot::Digest::parse(expected).map_err(|e| {
            ApexError::InvalidArgument(format!("{expected:?} is not a valid image digest: {e}."))
        })?;
        self.refresh(cancel).await?;
        let snapshot = self.snapshot()?;
        let host = self.host().ok_or_else(|| self.state_unknown())?;

        // From the staged deployment itself, never from a label.
        let staged_version = match host.staged() {
            Some(entry) if entry.digest() == Some(expected) => self
                .deployment_manifest(entry)
                .await
                .map_err(|e| errors::internal("The downloaded update cannot be read", &e))?
                .and_then(|m| m.parsed_version()),
            _ => None,
        };
        validate_apply(&snapshot, expected, staged_version.as_ref())
            .map_err(|e| errors::from_apply(&e))?;
        arming_precondition(&snapshot.host).map_err(|e| ApexError::PolicyDenied(e.to_string()))?;
        // apex-boot-assess reads grubenv and later clears it; an arming in between is lost.
        if self
            .providers
            .units
            .active_state(ASSESS_UNIT)
            .await
            .as_deref()
            == Some("activating")
        {
            return Err(ApexError::Busy(
                "The system is still checking this startup. Try again in a few minutes.".to_owned(),
            ));
        }

        // Before unlocking, so the stage can never boot unprotected.
        let booted = snapshot.host.booted_digest().unwrap_or_default();
        self.confirm_arming(booted, expected).await?;
        self.write_grubenv(arm_ops(&target))
            .await
            .map_err(|e| errors::internal("The boot counter could not be armed", &e))?;
        let unlocked = self
            .marking_bootc_busy(self.bootc.apply_downloaded(cancel))
            .await;
        // bootc unlocks early, so even a killed or failed bootc may have unlocked the stage.
        let status = self.status_after_change().await;
        let next = status.as_ref().map_or(NextBoot::Unknown, next_boot);
        let outcome = apply_outcome(expected, unlocked.is_ok(), &next);
        match &outcome {
            ApplyOutcome::Applied => {}
            ApplyOutcome::NotApplied => self.disarm().await,
            ApplyOutcome::Other(other) => self.rearm_for(other, host.booted_digest()).await,
            ApplyOutcome::Unconfirmed => {
                tracing::warn!("keeping the boot counter armed: the stage may be unlocked");
            }
        }
        if outcome == ApplyOutcome::Applied {
            let intent = Intent {
                digest: expected.to_owned(),
                created: self.now(),
            };
            if let Err(error) =
                unflushed_is_done(self.store.write_intent(&intent), "saving the intent")
            {
                // Not fatal: the unlocked stage alone holds the automatic run back.
                tracing::error!(error = %errors::with_sources(&error), "cannot save the intent");
            }
        }
        if let Ok(host) = status
            && let Err(error) = self.rebuild(host).await
        {
            tracing::warn!(error = %error.message(), "refresh after the operation failed");
        }
        apply_result(expected, unlocked, &outcome)
    }

    /// With a token of its own: the operation's is cancelled when the service stops, and
    /// this read decides whether the arming stays.
    async fn status_after_change(&self) -> Result<Host, ApexError> {
        let own = CancelToken::new();
        tokio::time::timeout(CONFIRM_WAIT, self.read_status(&own))
            .await
            .unwrap_or_else(|_| {
                Err(ApexError::Busy(
                    "bootc status did not answer in time.".to_owned(),
                ))
            })
    }

    /// So a failure of what actually boots falls back under its own digest. An arming left
    /// for the running image is cleared by the boot assessment.
    async fn rearm_for(&self, other: &str, booted: Option<&str>) {
        let Ok(digest) = apex_boot::Digest::parse(other) else {
            return;
        };
        if booted == Some(other) {
            return;
        }
        if let Err(error) = self.write_grubenv(arm_ops(&digest)).await {
            tracing::error!(error = %errors::with_sources(&error), "cannot arm the boot counter");
        }
    }

    async fn rollback(&self, mark_bad: bool, cancel: &CancelToken) -> Result<(), ApexError> {
        self.refresh(cancel).await?;
        let host = self
            .host()
            .ok_or_else(|| ApexError::Internal("No status.".to_owned()))?;
        if host.rollback().is_none() {
            return Err(ApexError::NothingToDo(
                "There is no previous version to go back to.".to_owned(),
            ));
        }
        if host.rollback_queued() {
            // bootc rollback would swap the order back. The queued one may be older or newer.
            return Err(ApexError::NothingToDo(
                "Another version is already set to start at the next restart.".to_owned(),
            ));
        }
        // It would boot a known-bad image without the boot counter; with panic=10 that can
        // loop until someone uses the GRUB menu.
        if let Some(digest) = host.rollback_digest()
            && let Some(entry) = self.snapshot()?.denylist.get(digest)
        {
            return Err(ApexError::PolicyDenied(format!(
                "The other installed version ({digest}) failed before and is marked as bad, so it is not started again. {}",
                entry.reason
            )));
        }
        if mark_bad && let Some(booted) = host.booted_digest() {
            self.deny(booted, MARK_BAD_REASON)?;
        }
        if let Err(error) = self.marking_bootc_busy(self.bootc.rollback(cancel)).await {
            self.refresh_after_acting(cancel).await;
            return Err(from_bootc(&error));
        }
        // bootc rollback also discards a staged deployment, so no arming can apply.
        self.disarm().await;
        self.clear_intent();
        tracing::info!(
            mark_bad,
            "the previous version will start at the next restart"
        );
        self.refresh_after_acting(cancel).await;
        Ok(())
    }

    fn deny(&self, digest: &str, reason: &str) -> Result<(), ApexError> {
        let mut denylist = self.store.read_denylist().map_err(|e| from_store(&e))?;
        let added = denylist.insert(DenyEntry {
            digest: digest.to_owned(),
            reason: reason.to_owned(),
            at: self.now(),
        });
        if added {
            unflushed_is_done(self.store.write_denylist(&denylist), "saving the denylist")
                .map_err(|e| from_store(&e))?;
            tracing::info!(digest, reason, "digest denied");
        }
        Ok(())
    }

    /// A deferral is a success: the timer did its job, and `DeferredReason` explains it.
    async fn automatic(&self, cancel: &CancelToken) -> Result<(), ApexError> {
        self.refresh(cancel).await?;
        match plan_automatic(&self.snapshot()?) {
            AutomaticPlan::Skip(deferral) => {
                tracing::info!(reason = %deferral, "automatic run skipped");
                return Ok(());
            }
            AutomaticPlan::Check(deferral) => {
                self.check(cancel).await?;
                tracing::info!(reason = %deferral, "automatic download deferred");
                return Ok(());
            }
            AutomaticPlan::CheckThenDownload => self.check(cancel).await?,
        }
        if !self.lock().settings.automatic_downloads {
            tracing::info!("automatic run: checked; automatic downloads are off");
            return Ok(());
        }
        match plan_after_check(&self.snapshot()?) {
            AfterCheck::NothingToDo => {
                tracing::info!("automatic run: nothing to download");
                Ok(())
            }
            AfterCheck::Defer(deferral) => {
                tracing::info!(reason = %deferral, "automatic download deferred");
                Ok(())
            }
            AfterCheck::Download => self.download(Trigger::Automatic, cancel).await,
        }
    }

    async fn fallback(&self, digest: &str, cancel: &CancelToken) -> Result<(), ApexError> {
        self.refresh(cancel).await?;
        let host = self
            .host()
            .ok_or_else(|| ApexError::Internal("No status.".to_owned()))?;
        if host.booted_digest() == Some(digest) {
            return Err(ApexError::InvalidArgument(
                "The reported fallback target is the running deployment.".to_owned(),
            ));
        }
        self.deny(digest, FALLBACK_REASON)?;
        self.record_fallback(digest)?;
        // Only in exactly this state: a repeated report must not swap the order back.
        if host.rollback_queued() && host.rollback_digest() == Some(digest) {
            if let Err(error) = self.marking_bootc_busy(self.bootc.rollback(cancel)).await {
                self.refresh_after_acting(cancel).await;
                return Err(from_bootc(&error));
            }
            tracing::warn!(
                digest,
                "the update failed to start; the previous version is the default again"
            );
        } else {
            tracing::info!(
                digest,
                "fallback reported; the running deployment is already the default"
            );
        }
        // Before the reply too: if it is lost, apex-boot-assess keeps its hedge, and GRUB
        // would go on counting the failed target.
        self.disarm_after_fallback(digest).await;
        if self
            .lock()
            .snapshot
            .as_ref()
            .and_then(|s| s.intent.as_ref())
            .is_some_and(|intent| intent.digest == digest)
        {
            self.clear_intent();
        }
        self.refresh_after_acting(cancel).await;
        Ok(())
    }

    fn record_fallback(&self, digest: &str) -> Result<(), ApexError> {
        let record = FallbackRecord {
            digest: digest.to_owned(),
            at: self.now(),
        };
        let mut state = self.lock();
        unflushed_is_done(
            self.store.write_fallback(&record),
            "saving the last fallback",
        )
        .map_err(|e| from_store(&e))?;
        state.last_fallback = Some(record.digest);
        drop(state);
        self.publish();
        Ok(())
    }

    pub fn set_automatic_downloads(&self, enabled: bool) -> Result<(), ApexError> {
        let settings = UpdateSettings {
            automatic_downloads: enabled,
        };
        let mut state = self.lock();
        unflushed_is_done(self.store.write_settings(&settings), "saving settings.json")
            .map_err(|e| from_store(&e))?;
        state.settings = settings;
        drop(state);
        tracing::info!(enabled, "automatic downloads set");
        self.publish();
        Ok(())
    }

    /// Disarms anyway when grubenv cannot be read: after a fallback no arming protects
    /// anything, and a leftover one could make GRUB count the failed target again.
    async fn disarm_after_fallback(&self, digest: &str) {
        match self.boot_vars().await {
            Ok(vars) => match &vars.apex_target {
                Some(target) if target.as_str() != digest => {
                    tracing::info!(%target, "the boot counter is armed for another update; left alone");
                    return;
                }
                None if vars.apex_counter.is_none() => return,
                _ => {}
            },
            Err(reason) => {
                tracing::warn!(
                    reason,
                    "cannot read grubenv; disarming the boot counter anyway"
                );
            }
        }
        if let Err(error) = self.write_grubenv(disarm_ops()).await {
            tracing::error!(error = %errors::with_sources(&error), "cannot disarm the boot counter after the fallback");
        }
    }

    /// Fails only when bootc itself is unusable, so the boot assessment notices a broken
    /// updater.
    pub async fn ping(&self) -> Result<(), ApexError> {
        self.first_refresh().await;
        match self.lock().refresh_failure.as_ref() {
            Some(failure) if failure.bootc_broken => Err(ApexError::Internal(format!(
                "The update service cannot read the system state: {}",
                failure.message
            ))),
            _ => Ok(()),
        }
    }

    /// While the state is unknown it fails rather than claim that nothing is staged.
    pub async fn get_changes(&self) -> Result<Vec<Change>, ApexError> {
        let host = self.host().ok_or_else(|| self.state_unknown())?;
        let Some(staged) = host.staged().cloned() else {
            return Ok(Vec::new());
        };
        let unreadable = |what: &str| {
            ApexError::Internal(format!(
                "The package list of {what} is not available (/usr/share/apex/manifest.json)."
            ))
        };
        let booted = self
            .running_manifest()
            .await
            .map_err(|e| {
                errors::internal("The package list of the running system cannot be read", &e)
            })?
            .ok_or_else(|| unreadable("the running system"))?;
        let staged = self
            .deployment_manifest(&staged)
            .await
            .map_err(|e| {
                errors::internal(
                    "The package list of the downloaded update cannot be read",
                    &e,
                )
            })?
            .ok_or_else(|| unreadable("the downloaded update"))?;
        Ok(changes_reply(&booted.packages, &staged.packages))
    }

    pub fn acknowledge_health_failure(&self) -> Result<(), ApexError> {
        {
            let mut state = self.lock();
            unflushed_is_done(
                self.store.clear_health_failure(),
                "clearing the health failure",
            )
            .map_err(|e| from_store(&e))?;
            unflushed_is_done(self.store.clear_fallback(), "clearing the last fallback")
                .map_err(|e| from_store(&e))?;
            if let Some(snapshot) = &mut state.snapshot {
                snapshot.health_failure = None;
            }
            state.last_fallback = None;
        }
        tracing::info!("health failure acknowledged");
        self.publish();
        Ok(())
    }

    pub fn report_boot(&self, verdict: BootVerdict, digest: Option<&str>) -> Result<(), ApexError> {
        // Under the state lock, so a concurrent refresh cannot publish records read before.
        match verdict {
            BootVerdict::Green => {
                let mut state = self.lock();
                unflushed_is_done(
                    self.store.clear_health_failure(),
                    "clearing the health failure",
                )
                .map_err(|e| from_store(&e))?;
                let intent = self.store.read_intent().map_err(|e| from_store(&e))?;
                let intent_booted = intent.is_some_and(|i| Some(i.digest.as_str()) == digest);
                if intent_booted {
                    unflushed_is_done(self.store.clear_intent(), "clearing the intent")
                        .map_err(|e| from_store(&e))?;
                }
                if let Some(snapshot) = &mut state.snapshot {
                    snapshot.health_failure = None;
                    if intent_booted {
                        snapshot.intent = None;
                    }
                }
                drop(state);
                tracing::info!(digest, "boot assessed green");
            }
            BootVerdict::Red => {
                let digest = digest.ok_or_else(|| {
                    ApexError::InvalidArgument(
                        "A red report needs the digest of the deployment that failed.".to_owned(),
                    )
                })?;
                let now = self.now();
                let mut state = self.lock();
                let recorded = self
                    .store
                    .read_health_failure()
                    .map_err(|e| from_store(&e))?;
                // apex-boot-assess's own record names the failed checks: keep it.
                if recorded.as_ref().is_none_or(|r| r.digest != digest) {
                    let record = HealthFailureRecord {
                        digest: digest.to_owned(),
                        failed_checks: Vec::new(),
                        at: now,
                    };
                    unflushed_is_done(
                        self.store.write_health_failure(&record),
                        "saving the health failure",
                    )
                    .map_err(|e| from_store(&e))?;
                }
                if let Some(snapshot) = &mut state.snapshot {
                    snapshot.health_failure = Some(digest.to_owned());
                }
                drop(state);
                tracing::warn!(digest, "the new version failed its health checks");
            }
            BootVerdict::RedUnarmed => {
                tracing::warn!(
                    digest,
                    "this boot failed a required health check (no update was being tried)"
                );
            }
            BootVerdict::Fallback => {
                return Err(ApexError::Internal(
                    "A fallback report is handled by the operation queue.".to_owned(),
                ));
            }
        }
        self.publish();
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextBoot {
    NothingApplied,
    /// `""` when the stage has no image.
    Applied(String),
    Unknown,
}

pub fn next_boot(status: &Host) -> NextBoot {
    match status.staged() {
        Some(entry) if !entry.is_download_only() => {
            NextBoot::Applied(entry.digest().unwrap_or_default().to_owned())
        }
        _ => NextBoot::NothingApplied,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    NotApplied,
    Other(String),
    Unconfirmed,
}

pub fn apply_outcome(expected: &str, unlocked: bool, next: &NextBoot) -> ApplyOutcome {
    match next {
        NextBoot::Applied(digest) if digest == expected => ApplyOutcome::Applied,
        NextBoot::Applied(other) => ApplyOutcome::Other(other.clone()),
        NextBoot::NothingApplied => ApplyOutcome::NotApplied,
        NextBoot::Unknown if unlocked => ApplyOutcome::Applied,
        NextBoot::Unknown => ApplyOutcome::Unconfirmed,
    }
}

fn apply_result(
    expected: &str,
    unlocked: Result<(), apex_bootc::BootcError>,
    outcome: &ApplyOutcome,
) -> Result<(), ApexError> {
    match (outcome, unlocked) {
        (ApplyOutcome::Applied, Ok(())) => {
            tracing::info!(
                digest = expected,
                "update set to install at the next restart"
            );
            Ok(())
        }
        (ApplyOutcome::Applied, Err(error)) => {
            tracing::warn!(
                digest = expected,
                error = %error,
                "bootc reported a failure, but the update is set to install at the next restart"
            );
            Ok(())
        }
        (ApplyOutcome::NotApplied, Ok(())) => Err(ApexError::Bootc(
            "bootc reported success, but the update is not set to install at the next restart."
                .to_owned(),
        )),
        (ApplyOutcome::NotApplied, Err(error)) => Err(from_bootc(&error)),
        (ApplyOutcome::Other(other), _) => Err(ApexError::DigestMismatch(format!(
            "Another update ({other}) was downloaded in the meantime and is set to install at the next restart instead of the reviewed one ({expected}). It is protected by the boot check; cancel it if it is not wanted."
        ))),
        (ApplyOutcome::Unconfirmed, Ok(())) => Ok(()),
        (ApplyOutcome::Unconfirmed, Err(error)) => {
            let error = from_bootc(&error);
            let message = error.message();
            let stop = if message.ends_with('.') { "" } else { "." };
            Err(ApexError::new(
                error.kind().unwrap_or(ErrorKind::Internal),
                format!(
                    "{message}{stop} Whether the update is set to install at the next restart could not be confirmed, so the boot check stays armed."
                ),
            ))
        }
    }
}

struct Records {
    intent: Option<Intent>,
    denylist: apex_update::Denylist,
    health: Option<HealthFailureRecord>,
    fallback: Option<FallbackRecord>,
}

fn read_records(store: &dyn Store) -> Result<Records, apex_update::StoreError> {
    Ok(Records {
        intent: store.read_intent()?,
        denylist: store.read_denylist()?,
        health: store.read_health_failure()?,
        fallback: store.read_fallback()?,
    })
}

struct Probes {
    power: apex_update::Power,
    metered: Option<bool>,
    finalize_failed: bool,
    sysroot_free: u64,
    boot_free: u64,
    boot_payload: Option<u64>,
}

fn summarize(host: &Host, manifests: &Manifests<'_>) -> HostSummary {
    HostSummary {
        booted: deployment_dict(host.booted(), manifests.booted, false),
        staged: deployment_dict(host.staged(), manifests.staged, true),
        rollback: deployment_dict(host.rollback(), manifests.rollback, false),
        channel: host
            .booted()
            .and_then(BootEntry::image_reference)
            .map(channel_of_image),
        spec_image: host.spec.image.as_ref().map(|i| i.image.clone()),
    }
}

fn logged<T>(result: Result<Option<T>, FileError>, what: &str) -> Option<T> {
    result.unwrap_or_else(|error| {
        tracing::warn!(error = %errors::with_sources(&error), "cannot read the manifest of {what}");
        None
    })
}

fn build_properties(state: &EngineState) -> Properties {
    let snapshot = state.snapshot.as_ref();
    let work = state.work.unwrap_or(Work::Idle);
    let summary = &state.summary;
    let available = snapshot
        .filter(|s| is_available(s))
        .and_then(|s| s.remote.as_ref())
        .map(|remote| apex_dbus::Deployment {
            image: summary.spec_image.clone(),
            digest: Some(remote.digest.clone()),
            version: state.last_check.remote_version.clone(),
            timestamp: state
                .remote_timestamp
                .as_ref()
                .filter(|(digest, _)| *digest == remote.digest)
                .map(|(_, t)| *t),
            download_size: Some(state.last_check.remote_size.unwrap_or(0)),
            ..apex_dbus::Deployment::default()
        })
        .unwrap_or_default();
    let mut staged = summary.staged.clone();
    if !staged.is_empty() {
        let measured = staged.digest.is_some() && staged.digest == state.last_check.remote_digest;
        let size = measured.then_some(state.last_check.remote_size).flatten();
        staged.download_size = Some(size.unwrap_or(0));
    }
    Properties {
        state: properties::state(snapshot, work, state.refresh_failure.is_some()),
        progress: properties::progress(work),
        booted: summary.booted.clone(),
        staged,
        rollback: summary.rollback.clone(),
        available,
        channel: summary.channel.unwrap_or(Channel::Stable),
        last_check: state.last_check.at,
        deferred_reason: properties::deferred_reason_or_stage(snapshot),
        last_error: state.last_error.clone(),
        health_failure: snapshot
            .and_then(|s| s.health_failure.clone())
            .unwrap_or_default(),
        last_fallback: state.last_fallback.clone().unwrap_or_default(),
        automatic_downloads: state.settings.automatic_downloads,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_update::DeploymentView;

    fn deployment(digest: &str) -> DeploymentView {
        DeploymentView {
            digest: digest.into(),
            version: None,
            download_only: true,
            pinned: false,
        }
    }

    #[test]
    fn arming_needs_a_booted_image_and_a_different_stage() {
        let booted = deployment("sha256:b");
        let staged = deployment("sha256:s");
        let ok = HostView {
            booted: Some(booted.clone()),
            staged: Some(staged.clone()),
            ..HostView::default()
        };
        assert_eq!(arming_precondition(&ok), Ok(()));
        // No rollback deployment is needed: the booted one becomes entry 1.
        assert!(ok.rollback.is_none());
        assert_eq!(
            arming_precondition(&HostView {
                staged: Some(staged),
                ..HostView::default()
            }),
            Err(ArmingError::NoBootedDeployment)
        );
        assert_eq!(
            arming_precondition(&HostView {
                booted: Some(booted.clone()),
                ..HostView::default()
            }),
            Err(ArmingError::NothingStaged)
        );
        assert_eq!(
            arming_precondition(&HostView {
                booted: Some(booted.clone()),
                staged: Some(booted),
                ..HostView::default()
            }),
            Err(ArmingError::StagedIsBooted)
        );
    }

    #[test]
    fn remote_view_from_the_record() {
        assert_eq!(remote_view(&LastCheck::default()), None);
        let view = remote_view(&LastCheck {
            at: 1,
            remote_digest: Some("sha256:r".into()),
            remote_version: Some("not a version".into()),
            remote_size: None,
        })
        .unwrap();
        assert_eq!(view.digest, "sha256:r");
        assert_eq!(view.version, None);
    }

    fn package(name: &str, version: &str, arch: &str) -> apex_update::Package {
        apex_update::Package {
            name: name.into(),
            epoch: 0,
            version: version.into(),
            release: "1.fc45".into(),
            arch: arch.into(),
        }
    }

    fn change(name: &str, arch: &str, kind: &str, from: &str, to: &str) -> Change {
        (
            name.into(),
            arch.into(),
            kind.into(),
            from.into(),
            to.into(),
        )
    }

    #[test]
    fn change_tuples_carry_the_arch_and_empty_evrs() {
        let changes = diff(
            &[
                package("kernel", "6.17.1", "x86_64"),
                package("old", "1.0", "noarch"),
            ],
            &[
                package("kernel", "6.17.2", "x86_64"),
                package("new", "2.0", "x86_64"),
            ],
        );
        assert_eq!(
            changes.iter().map(change_tuple).collect::<Vec<_>>(),
            [
                change(
                    "kernel",
                    "x86_64",
                    "upgraded",
                    "6.17.1-1.fc45",
                    "6.17.2-1.fc45"
                ),
                change("new", "x86_64", "added", "", "2.0-1.fc45"),
                change("old", "noarch", "removed", "1.0-1.fc45", ""),
            ]
        );
    }

    #[test]
    fn changes_are_one_per_arch_sorted_by_name_then_arch() {
        // Deliberately unsorted, with glibc for two architectures.
        let booted = [
            package("glibc", "2.42", "x86_64"),
            package("zlib", "1.3", "x86_64"),
            package("glibc", "2.42", "i686"),
            package("Xorg-free", "1", "noarch"),
        ];
        let staged = [
            package("zlib", "1.3", "x86_64"),
            package("glibc", "2.43", "i686"),
            package("glibc", "2.43", "x86_64"),
            package("apex", "0.1", "noarch"),
        ];
        let reply = changes_reply(&booted, &staged);
        assert_eq!(
            reply,
            [
                // Byte order: upper case sorts before lower case.
                change("Xorg-free", "noarch", "removed", "1-1.fc45", ""),
                change("apex", "noarch", "added", "", "0.1-1.fc45"),
                change("glibc", "i686", "upgraded", "2.42-1.fc45", "2.43-1.fc45"),
                change("glibc", "x86_64", "upgraded", "2.42-1.fc45", "2.43-1.fc45"),
            ]
        );
        assert!(changes_reply(&booted, &booted).is_empty());
    }

    #[test]
    fn arming_refusals_are_policy_denied_in_plain_words() {
        let booted = apex_boot::Digest::parse(
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        for (refused, needle) in [
            (ArmingRefused::StagedIsBooted, "already running"),
            (
                ArmingRefused::BootedUnconfirmed {
                    booted: booted.clone(),
                },
                "has not passed its startup check yet",
            ),
            (
                ArmingRefused::BootedFailed {
                    booted: booted.clone(),
                },
                "failed its startup check",
            ),
        ] {
            let error = arming_refusal(&refused);
            assert_eq!(error.kind(), Some(ErrorKind::PolicyDenied));
            assert!(error.message().contains(needle), "{}", error.message());
            assert!(
                !error.message().contains("sha256:"),
                "plain words, no digest: {}",
                error.message()
            );
        }
    }

    #[test]
    fn the_daemon_writes_grubenv_in_a_private_namespace_with_boot_remounted() {
        let writer = grubenv_writer();
        assert!(writer.remounts_boot());
        let line: Vec<String> = writer
            .command_line("unset", &["apex_target".into()])
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            line[..5],
            [
                "/usr/bin/unshare",
                "--mount",
                "--propagation",
                "private",
                "--"
            ]
        );
        assert!(
            line.iter()
                .any(|arg| arg.contains("mount -n -o remount,rw /boot")),
            "{line:?}"
        );
        assert!(line.iter().any(|arg| arg == "/boot/grub2/grubenv"));
        assert_eq!(line[line.len() - 2..], ["unset", "apex_target"]);
    }
}
