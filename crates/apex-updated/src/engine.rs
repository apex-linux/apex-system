//! The engine: executes operations and keeps the published state.
//!
//! Every decision is delegated to the pure `apex-update` crate; this module gathers the
//! [`SystemSnapshot`] it decides on, runs bootc through `apex-bootc`, arms and disarms the
//! boot counter through `apex-boot`, and persists the state files.
//!
//! Queued operations ([`Engine::execute`]) run one at a time on the worker (see
//! [`crate::queue`]). Each one reads bootc status **before** it decides (so a decision
//! never rests on an old snapshot) and again after it acted. The few operations that never
//! run bootc ([`Engine::ping`], [`Engine::get_changes`], [`Engine::report_boot`] for
//! everything but `fallback`, [`Engine::acknowledge_health_failure`]) run directly in the
//! method handler; they only touch state files and the in-memory snapshot, in short
//! synchronous sections that cannot interleave on the single-threaded runtime.
//!
//! # `bootc status` never overlaps the engine's own bootc command
//!
//! Every bootc command holds the ostree sysroot lock while it runs, and `bootc status`
//! takes the same lock: started during this daemon's own download it would wait silently
//! until its 30 s timeout and fail as `Busy`, holding up whoever asked. The queue already
//! runs one operation at a time and covers a refresh while anything runs, and on top of
//! that the engine marks each non-status bootc command in flight ([`Engine::bootc_busy`]):
//! while one runs, a status read is skipped (the refresh answers `Busy` at once and the
//! published state stays as it was). Nothing is lost, because every operation reads the
//! status itself once its bootc command has finished.

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
use apex_bootc::{BootEntry, Bootc, BootcError, CancelToken, CommandRunner, DownloadOutcome, Host};
use apex_dbus::{ApexError, BootVerdict, Change, Channel, ErrorKind};
use apex_update::gates::{
    Operation, check_download_resources, gate_boot_space, gate_bootc_flags, gate_disk_headroom,
    gate_intent, gate_power,
};
use apex_update::state::is_available;
use apex_update::{
    AfterCheck, ApplyError, AutomaticPlan, DenyEntry, HealthFailureRecord, HostView, Intent,
    LastCheck, Manifest, RemoteView, SystemSnapshot, Trigger, Version, diff, intent_is_orphaned,
    plan_after_check, plan_automatic, validate_apply, validate_discard, validate_download,
};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::channel::{channel_of_image, channel_target, is_channel_switch};
use crate::errors::{self, from_bootc, from_deferral, from_store};
use crate::properties::{self, Properties, Work};
use crate::providers::{
    ASSESS_UNIT, BOOT, BOOT_COMPLETE_UNIT, FileError, GrubEnvSource, ManifestSource, Providers,
    SYSROOT,
};
use crate::queue::Op;
use crate::store::{Store, unflushed_is_done};
use crate::view::{self, Manifests, RemoteImage, deployment_dict, image_timestamp};
use crate::{CHECK_RATE_LIMIT_SECS, REFRESH_MAX_AGE};

/// How long after the engine starts `Ping`, property reads and `GetChanges` wait for the
/// first refresh before answering from what is known ([`Engine::first_refresh`]).
pub const PING_WAIT: Duration = Duration::from_secs(20);

/// How long `Apply` waits for the `bootc status` that confirms what boots next after
/// `bootc upgrade --from-downloaded`. Shorter than the 15 s `main` gives a cancelled
/// operation at shutdown, so the arming decision is still made then.
pub const CONFIRM_WAIT: Duration = Duration::from_secs(10);

/// How long reading grubenv may take: a stalled `/boot` must not hold up the queue.
pub const GRUBENV_READ_WAIT: Duration = Duration::from_secs(10);

/// Denylist reason recorded for `ReportBoot("fallback")`.
pub const FALLBACK_REASON: &str =
    "The update did not start correctly in three attempts, so the previous version was restored.";

/// Denylist reason recorded for `Rollback(mark_bad = true)`.
pub const MARK_BAD_REASON: &str = "Marked as bad when going back to the previous version.";

/// Why a refresh was skipped while this engine's own bootc command runs.
pub const STATUS_SKIPPED: &str =
    "The update state is not read again while an update operation is running.";

/// The grubenv writer the daemon uses (M1 contract §5): `grub2-editenv` on
/// `/boot/grub2/grubenv`, started by `unshare --mount --propagation private` after
/// `mount -o remount,rw /boot` inside that private mount namespace, because bootc mounts
/// `/boot` read-only and the host must never see it writable (apex-boot's
/// [`Editenv::system`]).
///
/// It needs root with `CAP_SYS_ADMIN`, the `unshare(CLONE_NEWNS)` and `mount` system calls,
/// and `/usr/bin/{unshare,sh,mount,grub2-editenv}`; `apex-updated.service` keeps all of
/// them (see the comments in the unit).
pub fn grubenv_writer() -> Editenv {
    Editenv::system()
}

/// What the engine is built from.
pub struct EngineParts<R> {
    /// Runs bootc (`TokioRunner` in the daemon, `FakeRunner` in tests).
    pub runner: R,
    /// The state files.
    pub store: Arc<dyn Store>,
    /// Writes grubenv ([`grubenv_writer`] in the daemon).
    pub grub: Box<dyn GrubEnvWriter + Send>,
    /// Reads grubenv (`GrubEnvFile::system` in the daemon).
    pub grubenv: Arc<dyn GrubEnvSource>,
    /// Everything else the engine asks the system.
    pub providers: Providers,
}

impl<R: fmt::Debug> fmt::Debug for EngineParts<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineParts")
            .field("runner", &self.runner)
            .finish_non_exhaustive()
    }
}

/// Why the arming precondition (M1 contract §5) does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ArmingError {
    /// bootc reports no booted image, so nothing would be GRUB entry 1.
    #[error(
        "The running system is not a container image deployment, so the previous version could not be restored if the update failed."
    )]
    NoBootedDeployment,
    /// Nothing is staged.
    #[error("There is no downloaded update to install.")]
    NothingStaged,
    /// The staged image is the running one.
    #[error("The downloaded update is the version that is already running.")]
    StagedIsBooted,
}

/// The arming precondition of `Apply` (M1 contract §5, ADR 0009): the booted deployment
/// comes from an image, and a different image is staged.
///
/// It guarantees that GRUB entry 1, where the snippet falls back to, is a real deployment
/// when the armed one boots, never the firmware-setup entry. Finalizing the staged
/// deployment at shutdown (`_ostree_sysroot_finalize_staged` →
/// `ostree_sysroot_simple_write_deployment` without `NOT_DEFAULT`) makes it entry 0 and
/// always keeps the booted deployment, which becomes entry 1. An older rollback deployment
/// is not needed (it becomes entry 2 or is pruned), so a fresh single-deployment install
/// can arm too.
///
/// `host.booted` and `host.staged` exist only for entries that carry a container image
/// digest ([`view::deployment_view`]), so "comes from an image" is their presence.
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

/// The `PolicyDenied` reply for a refusal of [`check_arming`], the part of the arming
/// precondition that grubenv and `health-failure.json` answer (M1 contract §5): the running
/// deployment must be confirmed, because finalizing the stage keeps only the new deployment
/// and the running one, which becomes the only version to fall back to.
pub fn arming_refusal(refused: &ArmingRefused) -> ApexError {
    let text = match refused {
        ArmingRefused::StagedIsBooted => ArmingError::StagedIsBooted.to_string(),
        ArmingRefused::BootedUnconfirmed { .. } => "The running version has not passed its startup check yet, and installing another update now would remove the last version known to work. Wait until the check has passed, or go back to the previous version.".to_owned(),
        ArmingRefused::BootedFailed { .. } => "The running version failed its startup check. Go back to the previous version first, so that a version known to work stays installed.".to_owned(),
    };
    ApexError::PolicyDenied(text)
}

/// The remote view the engine decides on, from `last-check.json`.
pub fn remote_view(check: &LastCheck) -> Option<RemoteView> {
    check.remote_digest.as_ref().map(|digest| RemoteView {
        digest: digest.clone(),
        version: check.remote_version.as_deref().and_then(Version::parse),
    })
}

/// A package change as the `GetChanges` tuple `(name, arch, kind, from_evr, to_evr)`, with
/// `""` for a missing EVR (D-Bus has no null string).
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

/// The `GetChanges` reply for two package lists: one entry per changed `(name, arch)`,
/// sorted by `(name, arch)` in byte order as the contract requires. `apex_update::diff`
/// already returns that order; sorting again keeps the reply right even if it changes.
pub fn changes_reply(
    booted: &[apex_update::Package],
    staged: &[apex_update::Package],
) -> Vec<Change> {
    let mut changes: Vec<Change> = diff(booted, staged).iter().map(change_tuple).collect();
    changes.sort_unstable_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    changes
}

/// What the published deployment properties are derived from, rebuilt at each refresh.
#[derive(Debug, Clone, Default)]
struct HostSummary {
    booted: apex_dbus::Deployment,
    staged: apex_dbus::Deployment,
    rollback: apex_dbus::Deployment,
    channel: Option<Channel>,
    spec_image: Option<String>,
    build_times: Vec<Option<u64>>,
    booted_version: Option<Version>,
}

/// A failed refresh.
#[derive(Debug, Clone)]
struct RefreshFailure {
    /// For `LastError` and the log.
    message: String,
    /// bootc itself does not work (unparseable status, cannot be started, not a bootc
    /// host): `Ping` fails, so the boot assessment sees a broken updater.
    bootc_broken: bool,
}

/// Manifests read so far. The running image's never changes; deployments are keyed by
/// their ostree commit checksum, which names their content. At most three deployments are
/// relevant at a time, so a `HashMap` pruned at every refresh stays tiny.
#[derive(Debug, Default)]
struct ManifestCache {
    running: RunningManifest,
    deployments: HashMap<String, Option<Arc<Manifest>>>,
}

/// The running image's manifest, read at most once.
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
    /// Build time of the remote image, when the last check in this process learned it.
    remote_timestamp: Option<(String, u64)>,
    manifests: ManifestCache,
    work: Option<Work>,
    last_error: String,
}

/// Executes operations and publishes [`Properties`].
pub struct Engine<R> {
    bootc: Bootc<R>,
    /// bootc commands other than `status` running now (see [`Engine::bootc_busy`]).
    bootc_ops: AtomicUsize,
    store: Arc<dyn Store>,
    grub: Arc<Mutex<Box<dyn GrubEnvWriter + Send>>>,
    grubenv: Arc<dyn GrubEnvSource>,
    providers: Providers,
    state: Mutex<EngineState>,
    published: watch::Sender<Properties>,
    /// `true` once a refresh has determined the state, or recorded why it could not.
    settled: watch::Sender<bool>,
    /// Until when reads wait for the first refresh ([`Engine::first_refresh`]).
    first_refresh_deadline: Instant,
}

/// Marks one bootc command in flight until dropped, also when the command's future is
/// dropped half-way (the runner then kills its process group).
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
    /// An engine that has not refreshed yet (state `unknown`).
    pub fn new(parts: EngineParts<R>) -> Self {
        Self {
            bootc: Bootc::new(parts.runner),
            bootc_ops: AtomicUsize::new(0),
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

    /// Waits until the first refresh has finished, successfully or not, but no longer than
    /// [`PING_WAIT`] after the engine was created.
    ///
    /// Before that the published properties are placeholders (`State` `unknown`, empty
    /// deployment dictionaries) that would read as "no such deployment". D-Bus activation
    /// delivers the call that started the service right after it owns its name, long before
    /// the first `bootc status` has returned, so `Ping`, property reads and `GetChanges`
    /// wait here. The deadline is absolute, so a slow first refresh delays callers by at
    /// most [`PING_WAIT`] in total, not per call.
    pub async fn first_refresh(&self) {
        let mut settled = self.settled.subscribe();
        let _ = tokio::time::timeout_at(
            self.first_refresh_deadline,
            settled.wait_for(|settled| *settled),
        )
        .await;
    }

    /// Records that a refresh has determined the state or its failure.
    fn mark_settled(&self) {
        self.settled
            .send_if_modified(|settled| !std::mem::replace(settled, true));
    }
    /// Follows the published properties.
    pub fn subscribe(&self) -> watch::Receiver<Properties> {
        self.published.subscribe()
    }

    /// The properties as published now.
    pub fn properties(&self) -> Properties {
        self.published.borrow().clone()
    }

    fn lock(&self) -> MutexGuard<'_, EngineState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> u64 {
        self.providers.clock.now()
    }

    /// Whether one of this engine's bootc commands other than `status` is running. bootc
    /// holds the sysroot lock for the whole command, so `bootc status` is not run meanwhile
    /// (see the module documentation).
    pub fn bootc_busy(&self) -> bool {
        self.bootc_ops.load(Ordering::SeqCst) > 0
    }

    /// Runs `command`, a bootc command other than `status`, marked in flight.
    async fn bootc_op<T>(&self, command: impl Future<Output = T>) -> T {
        let _in_flight = InFlight::start(&self.bootc_ops);
        command.await
    }

    /// Whether the last refresh is older than [`REFRESH_MAX_AGE`] (or never happened).
    pub fn refresh_due(&self) -> bool {
        self.lock()
            .refreshed_at
            .is_none_or(|at| at.elapsed() >= REFRESH_MAX_AGE)
    }

    /// Whether the last **successful** check is less than [`CHECK_RATE_LIMIT_SECS`] old
    /// (`LastCheck`, the XML rate limit). A failed check does not count: a retry after a
    /// failure (say, once the network is back) must contact the registry, not report a
    /// success that never happened. A clock that went backwards does not count as recent,
    /// so it cannot block checks for long.
    pub fn check_is_recent(&self) -> bool {
        let now = self.now();
        let last = self.lock().last_check.at;
        last != 0 && now >= last && now - last < CHECK_RATE_LIMIT_SECS
    }

    // ----- publishing ---------------------------------------------------------------

    fn publish(&self) {
        let now = self.now();
        // Text from bootc, the registry and the state files goes on the bus only without
        // control characters (see `crate::text`).
        let properties = build_properties(&self.lock(), now).sanitized();
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

    /// The error for a request that needs the state while no refresh has succeeded yet:
    /// `Internal` with the reason when bootc could not be used, `Busy` while the first
    /// refresh is still under way.
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

    /// Refuses at once an `Apply` that the last refresh already shows cannot succeed:
    /// nothing is staged, another digest is staged, or the stage is already applied.
    ///
    /// Any active-session process may call `Apply` without a prompt, so a burst of calls
    /// with made-up digests must not each queue a full refresh. Only a state refreshed
    /// less than [`REFRESH_MAX_AGE`] ago is trusted; otherwise, and whenever this passes,
    /// the queued operation decides after its own refresh.
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

    // ----- queued operations --------------------------------------------------------

    /// Runs one queued operation. Failures of the work itself (not refusals) are kept in
    /// `LastError`; any success of a user or automatic operation clears it.
    pub async fn execute(&self, op: &Op, cancel: &CancelToken) -> Result<(), ApexError> {
        let result = match op {
            Op::Refresh => return self.refresh(cancel).await,
            Op::Check => self.check(cancel).await,
            Op::Download {
                override_soft_gates,
            } => self.download_op(*override_soft_gates, cancel).await,
            Op::Apply { expected_digest } => self.apply(expected_digest, cancel).await,
            Op::Discard => self.discard(cancel).await,
            Op::Rollback { mark_bad } => self.rollback(*mark_bad, cancel).await,
            Op::SwitchChannel(channel) => self.switch_channel(*channel, cancel).await,
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

    /// Re-reads bootc status and everything derived from it.
    pub async fn refresh(&self, cancel: &CancelToken) -> Result<(), ApexError> {
        let host = self.load(cancel).await?;
        self.rebuild(host).await
    }

    /// A refresh after an operation acted; a failure is only logged, because the operation
    /// itself already succeeded or failed on its own terms.
    async fn after(&self, cancel: &CancelToken) {
        if let Err(error) = self.refresh(cancel).await {
            tracing::warn!(error = %error.message(), "refresh after the operation failed");
        }
    }

    /// `bootc status`, recording a failure so the state and `Ping` reflect it.
    ///
    /// Skipped with `Busy` ([`STATUS_SKIPPED`]) while one of this engine's own bootc
    /// commands runs: the command holds the sysroot lock, so the status would only wait for
    /// its 30 s timeout. The skip records nothing, so the published state stays as it was
    /// until the operation reads the status itself.
    async fn load(&self, cancel: &CancelToken) -> Result<Host, ApexError> {
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

    /// Power, network, space and the finalization state.
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
            // The staged kernel is what finalization copies; the running one is the estimate
            // when the staged tree cannot be read.
            boot_payload: host
                .staged()
                .and_then(|entry| entry.deployment_root().ok())
                .and_then(|root| payload.boot_payload(&root))
                .or_else(|| payload.boot_payload(Path::new("/"))),
        }
    }

    /// Everything else the snapshot needs, around a freshly read `host`.
    ///
    /// The state files are read last, under the state lock and after every await, so a
    /// direct operation (`ReportBoot`, `AcknowledgeHealthFailure`), which changes them under
    /// the same lock, is never overwritten by records read before it.
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
        let summary = summarize(&host, &manifests, &host_view);

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
        state.host = Some(host);
        state.snapshot = Some(snapshot);
        state.summary = summary;
        if state.refresh_failure.take().is_some() {
            // The failure that LastError describes is over.
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

    /// The check record as it stands: the newer of the one this process knows and
    /// `last-check.json` (the file may have been written by an earlier instance, and a
    /// record that could not be saved is still known here).
    fn recorded_check(&self) -> LastCheck {
        let file = self.store.read_last_check().unwrap_or_else(|error| {
            tracing::warn!(error = %errors::with_sources(&error), "ignoring last-check.json");
            LastCheck::default()
        });
        let known = self.lock().last_check.clone();
        if file.at >= known.at { file } else { known }
    }

    /// `Check`: rate-limited, then `bootc upgrade --check` and a status read. What the tag
    /// points at comes from bootc's output and, when it says "no changes", from the check
    /// record (see [`view::remote_after_check`]).
    async fn check(&self, cancel: &CancelToken) -> Result<(), ApexError> {
        if self.check_is_recent() {
            tracing::debug!("check skipped: the last one is less than a minute old");
            return Ok(());
        }
        let now = self.now();
        let outcome = self
            .bootc_op(self.bootc.check(cancel))
            .await
            .map_err(|e| from_bootc(&e))?;
        let host = self.load(cancel).await?;
        let remote = view::remote_after_check(&outcome, &host, &self.recorded_check());
        if remote.is_none() {
            tracing::warn!(?outcome, "the check did not name the channel's image");
        }
        let timestamp = remote.as_ref().and_then(|r| r.timestamp);
        let (remote_digest, remote_version) = remote
            .map(
                |RemoteImage {
                     digest, version, ..
                 }| (Some(digest), version),
            )
            .unwrap_or_default();
        self.record_check(
            LastCheck {
                at: now,
                remote_digest,
                remote_version,
            },
            timestamp,
        );
        tracing::info!(?outcome, "checked for updates");
        self.rebuild(host).await
    }

    /// `bootc upgrade --download-only --progress-fd 3`, then the refresh, then the gates on
    /// what bootc actually staged ([`Self::screen_stage`]). `downloading` and the progress
    /// are published from the start until the refreshed state replaces them, so clients
    /// never see the state fall back to `available` in between.
    async fn download(&self, trigger: Trigger, cancel: &CancelToken) -> Result<(), ApexError> {
        self.set_work(Some(Work::Downloading { progress: None }));
        let (sink, mut updates) = mpsc::channel(32);
        let run = self.bootc_op(self.bootc.upgrade_download_only(Some(sink), cancel));
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

    /// The reply of an operation whose stage [`Self::screen_stage`] refused (`refused` is the
    /// reason). An automatic run succeeds: it did its job, and `DeferredReason` explains the
    /// stage while it exists. A user's download or channel switch fails with
    /// `PolicyDenied`, and `LastError` keeps the reason as well, because unlike a refusal
    /// before the work, the download did run and its result cannot be used.
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

    /// The refresh after a command that pulled the channel's tag (a download or a channel
    /// switch): `reported` is what bootc said it did when it succeeded, `None`
    /// when it failed (a stage left from before is not what the channel points at now, so
    /// the check record stays). On success the check record follows the tag
    /// ([`Self::follow_tag`]). Returns whether the state was refreshed.
    async fn after_staging(
        &self,
        reported: Option<&DownloadOutcome>,
        cancel: &CancelToken,
    ) -> bool {
        let host = match self.load(cancel).await {
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

    /// After bootc pulled the channel's tag: the tag points at the running image when bootc
    /// said `No update available.`, and otherwise at the staged image (a new, re-locked or
    /// unchanged stage; also when bootc's wording was not recognised, or for `bootc
    /// switch`, which reports nothing). That is what a check records, so the check record
    /// is updated if it names another digest (the tag moved since the last check). The
    /// record is also what a later check that finds "no changes" relies on
    /// ([`view::remote_after_check`]): the tag's image is then the one pulled last.
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
                },
                image_timestamp(image),
            );
        }
    }

    /// Runs `Apply`'s denylist and version gates on the update bootc actually staged, after
    /// a download or a channel switch, and returns the reason when it must not be installed
    /// ([`properties::stage_refusal`]). The channel's tag can move between the check (or the
    /// gates the operation ran on the check's digest) and the pull, so the staged digest is
    /// not necessarily the one that passed.
    ///
    /// Such a stage stays locked. It never becomes `Available` (the check record now names
    /// it, and it is staged) nor ready: `Apply` refuses it, because `validate_apply` runs
    /// the same gates, and `DeferredReason` explains it while it is staged.
    ///
    /// A download never leaves a stage unlocked: bootc runs with `--download-only`, and
    /// downloads and switches are refused while a stage is unlocked (`gate_intent`). An
    /// unlocked stage here means bootc ignored the flag, and nothing armed the boot counter
    /// for it, so it is locked again first, whatever the gates say.
    async fn screen_stage(&self, cancel: &CancelToken) -> Option<String> {
        let unlocked = self
            .lock()
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.host.has_unlocked_stage());
        if unlocked {
            self.relock_unexpected(cancel).await;
        }
        let refusal = {
            let state = self.lock();
            let switch = state.host.as_ref().is_some_and(is_channel_switch);
            properties::stage_refusal(state.snapshot.as_ref()?, switch)
        };
        if let Some(reason) = &refusal {
            tracing::warn!(reason, "the downloaded update will not be installed");
        }
        refusal
    }

    /// Locks a stage that a download left unlocked (see [`Self::screen_stage`]) with the
    /// re-lock `DiscardStaged` uses, then refreshes.
    async fn relock_unexpected(&self, cancel: &CancelToken) {
        tracing::error!("bootc left the downloaded update set to install; locking it again");
        let relock = self
            .bootc_op(self.bootc.upgrade_download_only(None, cancel))
            .await;
        match self.load(cancel).await {
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

    async fn grub(&self, ops: Vec<EnvOp>) -> Result<(), WriteError> {
        let grub = Arc::clone(&self.grub);
        // grub2-editenv is a blocking child process (with its own 30 s timeout); keep it off
        // the runtime thread so D-Bus stays responsive.
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
        if let Err(error) = self.grub(disarm_ops()).await {
            // Harmless: the boot assessment treats an arming whose target does not boot as
            // inconsistent and clears it.
            tracing::warn!(error = %errors::with_sources(&error), "cannot disarm the boot counter");
        }
    }

    fn clear_intent(&self) {
        if let Err(error) = unflushed_is_done(self.store.clear_intent(), "clearing the intent") {
            tracing::warn!(error = %errors::with_sources(&error), "cannot clear the intent");
        }
    }

    /// The boot counter's variables as grubenv holds them now, read off the runtime thread
    /// and bounded by [`GRUBENV_READ_WAIT`]. A missing file holds none. The error is a
    /// sentence fragment for a message.
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

    /// The arming precondition's second half (`apex_boot::check_arming`, M1 contract §5):
    /// grubenv is not armed for the running deployment, and `health-failure.json` does not
    /// name it. Both are read now, right before the arming. A refusal is `PolicyDenied`
    /// ([`arming_refusal`]); state that cannot be read refuses too (`Internal`), because the
    /// precondition cannot be shown to hold.
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

    /// `Apply(expected_digest)`: ARCHITECTURE §5.2, steps 1 to 6.
    ///
    /// A stage made by `SwitchChannel` ([`is_channel_switch`]) may hold the build that is
    /// running now or an older one: stable builds are testing builds promoted by digest, so
    /// the two channels are often at the same build, and testing is never older than
    /// stable. The administrator already confirmed following that channel (`switch-channel`
    /// is `auth_admin`), which is the explicit confirmation a downgrade needs (ARCHITECTURE
    /// §5.2), so for such a stage the version check alone is waived; every other check of
    /// `validate_apply` still applies. When the staged image is the running image under the
    /// other channel's reference, nothing new boots: the counter is not armed (the arming
    /// precondition needs a different image, and identical content has nothing to prove).
    ///
    /// The arming precondition (M1 contract §5) is checked in two parts:
    /// [`arming_precondition`] on the status (booted from an image, a different image
    /// staged), then, once the boot assessment of this startup has finished,
    /// [`check_arming`] on grubenv and `health-failure.json` read right before the arming
    /// (the running version is neither armed nor recorded as failed). Each refusal is
    /// `PolicyDenied` with a plain reason, and nothing is armed or unlocked.
    ///
    /// What happens after `bootc upgrade --from-downloaded` depends on a fresh status read
    /// ([`apply_outcome`]), not only on bootc's exit: bootc unlocks the stage in one step
    /// early on, so a bootc that is killed or fails afterwards (a shutdown cancels the
    /// operation, a timeout) has already unlocked it, and the arming must then stay.
    async fn apply(&self, expected: &str, cancel: &CancelToken) -> Result<(), ApexError> {
        let target = apex_boot::Digest::parse(expected).map_err(|e| {
            ApexError::InvalidArgument(format!("{expected:?} is not a valid image digest: {e}."))
        })?;
        self.refresh(cancel).await?;
        let snapshot = self.snapshot()?;
        let host = self.host().ok_or_else(|| self.state_unknown())?;

        // Step 2 reads the version from the staged deployment itself, never from a label.
        let staged_version = match host.staged() {
            Some(entry) if entry.digest() == Some(expected) => self
                .deployment_manifest(entry)
                .await
                .map_err(|e| errors::internal("The downloaded update cannot be read", &e))?
                .and_then(|m| m.parsed_version()),
            _ => None,
        };
        let switch = is_channel_switch(&host);
        // Steps 1 to 3 (digest and download-only, version, /boot space) and the other gates.
        match validate_apply(&snapshot, expected, staged_version.as_ref()) {
            Ok(()) => {}
            Err(ApplyError::Downgrade { booted, staged }) if switch => {
                tracing::info!(
                    %booted,
                    %staged,
                    "installing a channel switch to the same or an older build"
                );
                // What validate_apply checks after the version.
                gate_boot_space(snapshot.boot_free, snapshot.boot_payload)
                    .and_then(|()| gate_disk_headroom(snapshot.sysroot_free))
                    .and_then(|()| gate_power(snapshot.power, Operation::Apply))
                    .map_err(|d| from_deferral(&d))?;
            }
            Err(error) => return Err(errors::from_apply(&error)),
        }
        let origin_only = switch && host.booted_digest() == Some(expected);
        if !origin_only {
            arming_precondition(&snapshot.host)
                .map_err(|e| ApexError::PolicyDenied(e.to_string()))?;
        }
        // apex-boot-assess reads grubenv and later clears it; an arming in between would be
        // lost (see apex_boot::decide).
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

        // Step 4: arm before unlocking, so the stage can never boot unprotected. Only on top
        // of a confirmed running version (read now: the assessment above has finished).
        let armed = if origin_only {
            None
        } else {
            let booted = snapshot.host.booted_digest().unwrap_or_default();
            self.confirm_arming(booted, expected).await?;
            self.grub(arm_ops(&target))
                .await
                .map_err(|e| errors::internal("The boot counter could not be armed", &e))?;
            Some(target)
        };
        // Step 5.
        let unlocked = self.bootc_op(self.bootc.apply_downloaded(cancel)).await;
        let status = self.status_after_change().await;
        let next = status.as_ref().map_or(NextBoot::Unknown, next_boot);
        let outcome = apply_outcome(expected, unlocked.is_ok(), &next);
        match &outcome {
            ApplyOutcome::Applied => {}
            ApplyOutcome::NotApplied => {
                if armed.is_some() {
                    self.disarm().await;
                }
            }
            ApplyOutcome::Other(other) => self.rearm_for(other, host.booted_digest()).await,
            ApplyOutcome::Unconfirmed => {
                tracing::warn!("keeping the boot counter armed: the stage may be unlocked");
            }
        }
        if outcome == ApplyOutcome::Applied {
            // Step 6.
            let intent = Intent {
                digest: expected.to_owned(),
                created: self.now(),
            };
            if let Err(error) =
                unflushed_is_done(self.store.write_intent(&intent), "saving the intent")
            {
                // The update still installs at the next restart; without the record the
                // automatic run is still held back by the unlocked stage (gate_intent).
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

    /// The status read after a command that changed what boots next, with a token of its
    /// own (the operation's may be cancelled because the service is stopping, and this read
    /// decides whether the arming stays) and bounded by [`CONFIRM_WAIT`].
    async fn status_after_change(&self) -> Result<Host, ApexError> {
        let own = CancelToken::new();
        tokio::time::timeout(CONFIRM_WAIT, self.load(&own))
            .await
            .unwrap_or_else(|_| {
                Err(ApexError::Busy(
                    "bootc status did not answer in time.".to_owned(),
                ))
            })
    }

    /// Another digest than the reviewed one boots next: arm the counter for it, so a
    /// failure of what actually boots is counted and falls back under its own digest. Not
    /// for the running image (identical content) or an entry without a digest; the arming
    /// already written then stays, and the boot assessment clears it as inconsistent.
    async fn rearm_for(&self, other: &str, booted: Option<&str>) {
        let Ok(digest) = apex_boot::Digest::parse(other) else {
            return;
        };
        if booted == Some(other) {
            return;
        }
        if let Err(error) = self.grub(arm_ops(&digest)).await {
            tracing::error!(error = %errors::with_sources(&error), "cannot arm the boot counter");
        }
    }

    /// `DiscardStaged`: re-lock the applied stage, then disarm the boot counter and forget
    /// the intent. Never runs `bootc rollback`.
    ///
    /// bootc has no command that locks a stage in place (M1 contract §6 allows none), so the
    /// re-lock is `bootc upgrade --download-only`, which **pulls the channel's tag** first
    /// ([`Bootc::upgrade_download_only`] and its [`DownloadOutcome`]):
    ///
    /// | The tag points at | bootc reports | At the next restart |
    /// |---|---|---|
    /// | the applied image | `Relocked` | nothing is installed |
    /// | a newer image | `Staged`, locked: downloaded and staged in place of the applied one | nothing is installed; `Staged` now names the newer digest, and the check record follows it |
    /// | the running image | `NoUpdate`: nothing changed, exit 0 | the applied image is installed |
    /// | (registry unreachable) | a `Network` failure | the applied image is installed |
    ///
    /// The cancellation counts only when **both** bootc reports a locked stage
    /// ([`DownloadOutcome::stage_locked`]) **and** a fresh status read shows that nothing
    /// unlocked boots next (M1 contract §6, [`discard_outcome`]). Only then are the counter
    /// disarmed and the intent cleared. In every other case the arming and `intent.json`
    /// stay, because they are what protects a restart that may still install the update,
    /// and the call fails with a plain reason ([`discard_result`]). An arming kept for a
    /// stage that is in fact locked is harmless: the next boot assessment clears an arming
    /// whose target is not the default. The one exception is a stage bootc unlocked for
    /// another digest (a bootc that ignored `--download-only`): the counter is armed for
    /// that digest instead, so what boots next is protected.
    ///
    /// Re-locking comes before disarming, so a stage that still boots is never left without
    /// the counter.
    async fn discard(&self, cancel: &CancelToken) -> Result<(), ApexError> {
        self.refresh(cancel).await?;
        let snapshot = self.snapshot()?;
        validate_discard(&snapshot).map_err(|e| errors::from_discard(&e))?;
        // validate_discard found an applied (unlocked) stage; its digest is what is armed.
        let applied = snapshot
            .host
            .staged
            .as_ref()
            .map(|staged| staged.digest.clone())
            .unwrap_or_default();

        let relock = self
            .bootc_op(self.bootc.upgrade_download_only(None, cancel))
            .await;
        // Decides whether the arming goes, so it has a token of its own (the operation's is
        // cancelled when the service stops).
        let status = self.status_after_change().await;
        let bootc_locked = relock.as_ref().is_ok_and(DownloadOutcome::stage_locked);
        let next = status.as_ref().map_or(NextBoot::Unknown, next_boot);
        let outcome = discard_outcome(&applied, bootc_locked, &next);
        match &outcome {
            DiscardOutcome::Cancelled => {
                self.disarm().await;
                self.clear_intent();
            }
            DiscardOutcome::Kept => {
                tracing::warn!(
                    ?relock,
                    ?next,
                    "keeping the boot counter armed and the intent: the cancellation is not confirmed"
                );
            }
            DiscardOutcome::Other(other) => {
                self.rearm_for(other, snapshot.host.booted_digest()).await;
            }
        }
        let status = match status {
            Ok(host) => {
                if let Ok(reported) = &relock
                    && *reported != DownloadOutcome::Unknown
                {
                    // bootc pulled the tag and said what it points at.
                    self.follow_tag(reported, &host);
                }
                if let Err(error) = self.rebuild(host).await {
                    tracing::warn!(error = %error.message(), "refresh after the re-lock failed");
                }
                None
            }
            Err(error) => Some(error),
        };
        discard_result(relock, &next, status, &outcome)
    }

    /// `Rollback(mark_bad)`: `bootc rollback`, optionally denying the booted digest first.
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
            // bootc rollback would swap the order back. The queued deployment may be older
            // (a rollback) or newer (the failed update after a fallback, or undoing a
            // rollback), so the text names neither.
            return Err(ApexError::NothingToDo(
                "Another version is already set to start at the next restart.".to_owned(),
            ));
        }
        // After a fallback (or a rollback with mark_bad) the failed deployment is still
        // installed, as the rollback deployment. Making it the default again would boot a
        // known-bad image without the boot counter, which is exactly what the denylist
        // prevents; with panic=10 that can be a boot loop no one can leave without the
        // GRUB menu.
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
        if let Err(error) = self.bootc_op(self.bootc.rollback(cancel)).await {
            self.after(cancel).await;
            return Err(from_bootc(&error));
        }
        // bootc rollback also discards a staged deployment, so no arming can apply.
        self.disarm().await;
        self.clear_intent();
        tracing::info!(
            mark_bad,
            "the previous version will start at the next restart"
        );
        self.after(cancel).await;
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

    /// The Fedora release of the running system: its manifest, else os-release.
    async fn fedora_release(&self) -> Option<u32> {
        let from_manifest = logged(self.running_manifest().await, "the running image")
            .map(|m| m.fedora)
            .filter(|&fedora| fedora != 0);
        from_manifest.or_else(|| self.providers.files.fedora_release())
    }

    /// `SwitchChannel(channel)`: `bootc switch --download-only` to the compile-time
    /// reference of `channel` for the running Fedora release.
    ///
    /// bootc stages the channel's image even when it is the running one (only the origin
    /// changes), and the other channel may be at an older build. `Apply` installs such a
    /// stage anyway ([`is_channel_switch`]), so the switch always completes with an
    /// `Apply` and a restart; until then the booted deployment, and `Channel`, still follow
    /// the old channel, and a locked stage that is not applied is dropped at the next
    /// restart.
    async fn switch_channel(
        &self,
        channel: Channel,
        cancel: &CancelToken,
    ) -> Result<(), ApexError> {
        self.refresh(cancel).await?;
        let snapshot = self.snapshot()?;
        let fedora = self.fedora_release().await.ok_or_else(|| {
            ApexError::PolicyDenied(
                "The Fedora release of this system is unknown, so the channel cannot be changed."
                    .to_owned(),
            )
        })?;
        let target = channel_target(channel, fedora).ok_or_else(|| {
            ApexError::PolicyDenied(format!(
                "Changing the channel is not available on Fedora {fedora} with this version of Apex."
            ))
        })?;
        let following = self.host().and_then(|h| h.spec.image.map(|i| i.image));
        if following.as_deref() == Some(target.to_string().as_str()) {
            return Err(ApexError::NothingToDo(format!(
                "This system already follows the {channel} channel."
            )));
        }
        // Hard gates only: the call is admin-authenticated and explicit, and the contract
        // gives it no override argument for the soft ones.
        gate_bootc_flags(&snapshot.host).map_err(|d| from_deferral(&d))?;
        gate_intent(snapshot.intent.as_ref(), &snapshot.host, Trigger::User)
            .map_err(|d| from_deferral(&d))?;
        check_download_resources(&snapshot, true).map_err(|d| from_deferral(&d))?;

        self.set_work(Some(Work::Downloading { progress: None }));
        let result = self
            .bootc_op(self.bootc.switch_download_only(target.clone(), cancel))
            .await;
        // The switch resolved the new channel's tag, which is what a check records; bootc
        // switch reports nothing, so the staged image is taken.
        let reported = DownloadOutcome::Unknown;
        let refreshed = self
            .after_staging(result.as_ref().ok().map(|()| &reported), cancel)
            .await;
        let refused = if result.is_ok() && refreshed {
            self.screen_stage(cancel).await
        } else {
            None
        };
        self.set_work(None);
        result.map_err(|e| from_bootc(&e))?;
        tracing::info!(%target, "switched channel; the new image is downloaded");
        self.refused_stage_result(refused, Trigger::User)
    }

    /// `Automatic`: plan, check, then download when every gate passes (ARCHITECTURE §5.2).
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

    /// `ReportBoot("fallback", digest)`: GRUB fell back from `digest`. Deny it, then make
    /// the running deployment the default with `bootc rollback`, disarm the boot counter,
    /// and forget the intent, all before the reply.
    ///
    /// After a fallback bootc reports `rollbackQueued` with the failed target as the
    /// rollback deployment (it is still first in the boot order). `bootc rollback` runs
    /// only in exactly that situation: a repeated report must not swap the order back.
    ///
    /// Once the running deployment is the default (after the rollback, or already), the
    /// arming for `digest` is removed here ([`Self::disarm_after_fallback`]), before the
    /// reply. `apex-boot-assess` wrote the hedge `apex_counter=3` before reporting and
    /// disarms after a successful reply; if the reply is lost, it keeps the hedge unless it
    /// sees the rollback. Disarming here too means a lost reply can no longer leave GRUB
    /// counting (and, after three quick shutdowns, choosing) the failed target. Both
    /// disarms are the same `unset`, so their order does not matter.
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
        if host.rollback_queued() && host.rollback_digest() == Some(digest) {
            if let Err(error) = self.bootc_op(self.bootc.rollback(cancel)).await {
                self.after(cancel).await;
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
        self.after(cancel).await;
        Ok(())
    }

    /// Removes the arming for `digest`, the target GRUB fell back from, once the running
    /// deployment is the default again. An arming for another digest is left alone (it
    /// belongs to a later `Apply`), and nothing is written when nothing is armed. When
    /// grubenv cannot be read, the counter is disarmed anyway: after a fallback no arming
    /// can protect anything, and a leftover one could only make GRUB count the failed
    /// target again. A failed write is logged; `apex-boot-assess` disarms as well.
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
        if let Err(error) = self.grub(disarm_ops()).await {
            tracing::error!(error = %errors::with_sources(&error), "cannot disarm the boot counter after the fallback");
        }
    }

    // ----- direct operations --------------------------------------------------------

    /// `Ping`: waits for the first refresh ([`Engine::first_refresh`]), then fails only if
    /// bootc itself is unusable (its status cannot be read or parsed), so the boot
    /// assessment notices an image whose updater is broken. Transient conditions answer
    /// success.
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

    /// `GetChanges`: the package diff between the running image and the staged one, from
    /// their manifests; empty when nothing is staged. While the state is not known (no
    /// refresh has succeeded), it fails rather than claim that nothing is staged: `Busy`
    /// while the first refresh is under way, `Internal` when bootc cannot be used.
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

    /// `AcknowledgeHealthFailure`: forget the health failure. Idempotent.
    pub fn acknowledge_health_failure(&self) -> Result<(), ApexError> {
        {
            let mut state = self.lock();
            unflushed_is_done(
                self.store.clear_health_failure(),
                "clearing the health failure",
            )
            .map_err(|e| from_store(&e))?;
            if let Some(snapshot) = &mut state.snapshot {
                snapshot.health_failure = None;
            }
        }
        tracing::info!("health failure acknowledged");
        self.publish();
        Ok(())
    }

    /// `ReportBoot` for `green`, `red` and `red-unarmed` (`fallback` is queued, see
    /// [`Op::Fallback`]).
    ///
    /// - `green`: forget the health failure, and the intent if it is for the booted
    ///   `digest` (an intent for another digest belongs to a later apply).
    /// - `red`: record the health failure for `digest` unless `apex-boot-assess` already
    ///   did (its record keeps the failed check names).
    /// - `red-unarmed`: only logged.
    pub fn report_boot(&self, verdict: BootVerdict, digest: Option<&str>) -> Result<(), ApexError> {
        // The store changes and the snapshot update happen under the state lock, so a
        // refresh running at the same time cannot publish records read before them.
        match verdict {
            BootVerdict::Green => {
                let mut state = self.lock();
                unflushed_is_done(
                    self.store.clear_health_failure(),
                    "clearing the health failure",
                )
                .map_err(|e| from_store(&e))?;
                let intent = self.store.read_intent().map_err(|e| from_store(&e))?;
                let done = intent.is_some_and(|i| Some(i.digest.as_str()) == digest);
                if done {
                    unflushed_is_done(self.store.clear_intent(), "clearing the intent")
                        .map_err(|e| from_store(&e))?;
                }
                if let Some(snapshot) = &mut state.snapshot {
                    snapshot.health_failure = None;
                    if done {
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

/// What boots at the next restart, from the status read after `Apply`'s unlock or
/// `DiscardStaged`'s re-lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextBoot {
    /// No unlocked stage: nothing new is installed at the next restart.
    NothingApplied,
    /// A stage with this digest (`""` if it has no image) is unlocked and installed at the
    /// next restart.
    Applied(String),
    /// The status could not be read.
    Unknown,
}

/// What `status` says boots at the next restart.
pub fn next_boot(status: &Host) -> NextBoot {
    match status.staged() {
        Some(entry) if !entry.is_download_only() => {
            NextBoot::Applied(entry.digest().unwrap_or_default().to_owned())
        }
        _ => NextBoot::NothingApplied,
    }
}

/// What `DiscardStaged` concludes once `bootc upgrade --download-only` has tried to re-lock
/// the applied stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardOutcome {
    /// bootc reported a locked stage and the status confirms that nothing unlocked boots
    /// next: disarm the counter, forget the intent, succeed.
    Cancelled,
    /// Not confirmed: the applied update may still install at the next restart, so the
    /// arming and the intent stay, and the call fails.
    Kept,
    /// Another digest is unlocked and boots next (a bootc that ignored `--download-only`):
    /// arm the counter for it, and fail.
    Other(String),
}

/// [`DiscardOutcome`] for the applied digest `applied`, from bootc's report (`bootc_locked`:
/// [`DownloadOutcome::stage_locked`]) and what the status read afterwards says boots next.
/// Success needs both (M1 contract §6): `No update available.` leaves an applied stage
/// applied while bootc exits 0, and a status alone may be stale or unreadable.
pub fn discard_outcome(applied: &str, bootc_locked: bool, next: &NextBoot) -> DiscardOutcome {
    match next {
        NextBoot::NothingApplied if bootc_locked => DiscardOutcome::Cancelled,
        NextBoot::Applied(other) if other != applied && !other.is_empty() => {
            DiscardOutcome::Other(other.clone())
        }
        NextBoot::NothingApplied | NextBoot::Applied(_) | NextBoot::Unknown => DiscardOutcome::Kept,
    }
}

/// The reply of `DiscardStaged`: success only for [`DiscardOutcome::Cancelled`], otherwise
/// why the update was not cancelled and what happens at the next restart, in plain words.
/// `status_error` is the failure of the status read afterwards, if it failed.
pub fn discard_result(
    relock: Result<DownloadOutcome, BootcError>,
    next: &NextBoot,
    status_error: Option<ApexError>,
    outcome: &DiscardOutcome,
) -> Result<(), ApexError> {
    let still = match next {
        NextBoot::Applied(_) => "The update is still set to install at the next restart.",
        NextBoot::NothingApplied | NextBoot::Unknown => {
            "It may still be installed at the next restart, so the startup check stays armed."
        }
    };
    match (outcome, relock) {
        (DiscardOutcome::Cancelled, relock) => {
            tracing::info!(
                ?relock,
                "the update will not be installed at the next restart"
            );
            Ok(())
        }
        (DiscardOutcome::Other(other), _) => Err(ApexError::Bootc(format!(
            "bootc set another update ({other}) to install at the next restart instead of cancelling. It is protected by the startup check; cancel again to stop it."
        ))),
        (DiscardOutcome::Kept, Err(error)) => {
            let mapped = from_bootc(&error);
            let kind = mapped.kind().unwrap_or(ErrorKind::Internal);
            let reason = if kind == ErrorKind::Network {
                tracing::warn!(error = %error, "cancelling failed: no connection to the registry");
                "Cancelling needs a network connection.".to_owned()
            } else {
                sentence(&mapped.message())
            };
            Err(ApexError::new(kind, format!("{reason} {still}")))
        }
        (DiscardOutcome::Kept, Ok(DownloadOutcome::NoUpdate)) => Err(ApexError::Bootc(format!(
            "The update channel now points at the installed version, so the update could not be cancelled. {still}"
        ))),
        (DiscardOutcome::Kept, Ok(reported)) if reported.stage_locked() => match status_error {
            Some(error) => Err(ApexError::new(
                error.kind().unwrap_or(ErrorKind::Internal),
                format!(
                    "bootc reported the update as cancelled, but this could not be confirmed: {} {still}",
                    sentence(&error.message())
                ),
            )),
            None => Err(ApexError::Bootc(
                "bootc reported the update as cancelled, but it is still set to install at the next restart. The startup check stays armed."
                    .to_owned(),
            )),
        },
        (DiscardOutcome::Kept, Ok(_)) => Err(ApexError::Bootc(format!(
            "bootc did not report the update as cancelled. {still}"
        ))),
    }
}

/// `text` as a sentence: it ends with a full stop.
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with(['.', '!', '?']) {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

/// What `Apply` concludes once `bootc upgrade --from-downloaded` has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The reviewed digest installs at the next restart, or bootc reported success and the
    /// status could not be read: keep the arming, write the intent, succeed.
    Applied,
    /// Nothing installs at the next restart: disarm and fail.
    NotApplied,
    /// Another digest installs at the next restart (another bootc run staged it in the
    /// meantime): arm for it and fail with `DigestMismatch`.
    Other(String),
    /// bootc failed and the status could not be read, so the stage may be unlocked: keep
    /// the arming (an arming for a stage that does not boot is harmless: the next boot
    /// assessment clears it as inconsistent) and fail.
    Unconfirmed,
}

/// [`ApplyOutcome`] from bootc's report (`unlocked`: it exited successfully) and what the
/// status read afterwards says boots next.
pub fn apply_outcome(expected: &str, unlocked: bool, next: &NextBoot) -> ApplyOutcome {
    match next {
        NextBoot::Applied(digest) if digest == expected => ApplyOutcome::Applied,
        NextBoot::Applied(other) => ApplyOutcome::Other(other.clone()),
        NextBoot::NothingApplied => ApplyOutcome::NotApplied,
        NextBoot::Unknown if unlocked => ApplyOutcome::Applied,
        NextBoot::Unknown => ApplyOutcome::Unconfirmed,
    }
}

/// The reply of `Apply` for `outcome`, given bootc's own result.
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

/// The persisted records a snapshot needs.
struct Records {
    intent: Option<Intent>,
    denylist: apex_update::Denylist,
    health: Option<HealthFailureRecord>,
}

fn read_records(store: &dyn Store) -> Result<Records, apex_update::StoreError> {
    Ok(Records {
        intent: store.read_intent()?,
        denylist: store.read_denylist()?,
        health: store.read_health_failure()?,
    })
}

/// What the system reports besides bootc.
struct Probes {
    power: apex_update::Power,
    metered: Option<bool>,
    finalize_failed: bool,
    sysroot_free: u64,
    boot_free: u64,
    boot_payload: Option<u64>,
}

fn summarize(host: &Host, manifests: &Manifests<'_>, view: &HostView) -> HostSummary {
    HostSummary {
        booted: deployment_dict(host.booted(), manifests.booted, false),
        staged: deployment_dict(host.staged(), manifests.staged, true),
        rollback: deployment_dict(host.rollback(), manifests.rollback, false),
        channel: host
            .booted()
            .and_then(BootEntry::image_reference)
            .map(channel_of_image),
        spec_image: host.spec.image.as_ref().map(|i| i.image.clone()),
        build_times: [host.booted(), host.staged()]
            .into_iter()
            .flatten()
            .map(|e| e.image.as_ref().and_then(image_timestamp))
            .collect(),
        booted_version: view.booted_version().cloned(),
    }
}

fn logged<T>(result: Result<Option<T>, FileError>, what: &str) -> Option<T> {
    result.unwrap_or_else(|error| {
        tracing::warn!(error = %errors::with_sources(&error), "cannot read the manifest of {what}");
        None
    })
}

fn build_properties(state: &EngineState, now: u64) -> Properties {
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
            ..apex_dbus::Deployment::default()
        })
        .unwrap_or_default();
    let remote_time = state.remote_timestamp.as_ref().map(|(_, t)| *t);
    let newest = properties::newest_build_time(
        summary.build_times.iter().copied().chain([remote_time]),
        [
            summary.booted_version.clone(),
            state
                .last_check
                .remote_version
                .as_deref()
                .and_then(Version::parse),
        ],
    );
    Properties {
        state: properties::state(snapshot, work, state.refresh_failure.is_some()),
        progress: properties::progress(work),
        booted: summary.booted.clone(),
        staged: summary.staged.clone(),
        rollback: summary.rollback.clone(),
        available,
        channel: summary.channel.unwrap_or(Channel::Stable),
        last_check: state.last_check.at,
        deferred_reason: properties::deferred_reason_or_stage(
            snapshot,
            state.host.as_ref().is_some_and(is_channel_switch),
        ),
        last_error: state.last_error.clone(),
        stale: properties::stale(state.last_check.at, newest, now),
        health_failure: snapshot
            .and_then(|s| s.health_failure.clone())
            .unwrap_or_default(),
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
        // Multilib: glibc exists for two architectures and changes in both. The manifests
        // are deliberately unsorted; the reply must not be.
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
    fn discard_succeeds_only_when_bootc_and_the_status_agree_the_stage_is_locked() {
        let applied = "sha256:s";
        // bootc reported a locked stage (locked again, or a newer image staged locked), and
        // nothing unlocked boots next.
        assert_eq!(
            discard_outcome(applied, true, &NextBoot::NothingApplied),
            DiscardOutcome::Cancelled
        );
        // The status alone is not enough: "No update available." leaves an applied stage
        // applied, and output bootc no longer words as expected proves nothing.
        assert_eq!(
            discard_outcome(applied, false, &NextBoot::NothingApplied),
            DiscardOutcome::Kept
        );
        // Neither is bootc's report alone.
        for locked in [true, false] {
            assert_eq!(
                discard_outcome(applied, locked, &NextBoot::Applied(applied.into())),
                DiscardOutcome::Kept
            );
            assert_eq!(
                discard_outcome(applied, locked, &NextBoot::Unknown),
                DiscardOutcome::Kept
            );
            // Another digest boots next: the counter follows it.
            assert_eq!(
                discard_outcome(applied, locked, &NextBoot::Applied("sha256:n".into())),
                DiscardOutcome::Other("sha256:n".into())
            );
            // An unlocked stage without a digest cannot be armed for; keep what there is.
            assert_eq!(
                discard_outcome(applied, locked, &NextBoot::Applied(String::new())),
                DiscardOutcome::Kept
            );
        }
    }

    fn network_error() -> BootcError {
        BootcError::Network {
            detail: "dial tcp: connect: network is unreachable".into(),
        }
    }

    #[test]
    fn a_discard_that_did_not_happen_says_why_in_plain_words() {
        let still = NextBoot::Applied("sha256:s".into());
        let kept = DiscardOutcome::Kept;
        let error = discard_result(Ok(DownloadOutcome::NoUpdate), &still, None, &kept)
            .expect_err("not cancelled");
        assert_eq!(error.kind(), Some(ErrorKind::Bootc));
        assert_eq!(
            error.message(),
            "The update channel now points at the installed version, so the update could not be cancelled. The update is still set to install at the next restart."
        );

        let error = discard_result(Err(network_error()), &still, None, &kept).expect_err("offline");
        assert_eq!(error.kind(), Some(ErrorKind::Network));
        assert_eq!(
            error.message(),
            "Cancelling needs a network connection. The update is still set to install at the next restart."
        );

        // bootc claims the lock, the status cannot be read.
        let busy = ApexError::Busy("The sysroot is locked by another process".into());
        let error = discard_result(
            Ok(DownloadOutcome::Relocked),
            &NextBoot::Unknown,
            Some(busy),
            &kept,
        )
        .expect_err("not confirmed");
        assert_eq!(error.kind(), Some(ErrorKind::Busy));
        assert!(
            error.message().contains("could not be confirmed: The sysroot is locked by another process. It may still be installed"),
            "{}",
            error.message()
        );

        // bootc claims the lock, the status says otherwise.
        let error = discard_result(Ok(DownloadOutcome::Relocked), &still, None, &kept)
            .expect_err("still applied");
        assert_eq!(error.kind(), Some(ErrorKind::Bootc));
        assert!(
            error.message().contains("still set to install"),
            "{}",
            error.message()
        );

        // Output bootc no longer words as expected.
        let error = discard_result(
            Ok(DownloadOutcome::Unknown),
            &NextBoot::NothingApplied,
            None,
            &kept,
        )
        .expect_err("not reported");
        assert_eq!(error.kind(), Some(ErrorKind::Bootc));
        assert!(
            error
                .message()
                .starts_with("bootc did not report the update as cancelled.")
        );

        let error = discard_result(
            Ok(DownloadOutcome::Unknown),
            &NextBoot::Applied("sha256:n".into()),
            None,
            &DiscardOutcome::Other("sha256:n".into()),
        )
        .expect_err("another update");
        assert!(error.message().contains("sha256:n"), "{}", error.message());

        discard_result(
            Ok(DownloadOutcome::Relocked),
            &NextBoot::NothingApplied,
            None,
            &DiscardOutcome::Cancelled,
        )
        .expect("cancelled");
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
