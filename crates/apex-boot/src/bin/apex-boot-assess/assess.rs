//! The glue: gather what [`apex_boot::decide()`] needs, then carry out the decision's steps.
//!
//! Everything here is synchronous and talks to the world only through [`crate::ports`],
//! the library's [`Check`]s and a [`GrubEnvWriter`], so the tests drive every decision path
//! with fakes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use apex_boot::checks::force_red::{self, HookFile};
use apex_boot::grubenv::{self, ReadError};
use apex_boot::{
    AssessInput, BootVars, BootVarsError, Check, CheckResult, DefaultDeployment, Digest,
    GrubEnvWriter, HealthFailure, SESSION_WAIT, Step, apply_ops, reconcile_fallback, run_checks,
};

use crate::ports::{BootedImage, Clock, FailureStore, HostStatus, Rebooter, Sessions, Updater};
use crate::{ping, reported, session};

/// The ports shared with the check threads.
#[derive(Clone)]
pub(crate) struct Ports {
    /// logind.
    pub(crate) sessions: Arc<dyn Sessions>,
    /// `apex-updated`.
    pub(crate) updater: Arc<dyn Updater>,
    /// `bootc status`.
    pub(crate) host: Arc<dyn HostStatus>,
    /// Time.
    pub(crate) clock: Arc<dyn Clock>,
}

/// The checks that need neither logind nor D-Bus: the library's unit and grub.cfg checks,
/// and the file-based reported checks.
pub(crate) struct LocalChecks {
    /// Required: [`CORE_UNITS_ACTIVE`](apex_boot::checks::names::CORE_UNITS_ACTIVE).
    pub(crate) core_units: Box<dyn Check>,
    /// Required: [`FAILED_UNITS`](apex_boot::checks::names::FAILED_UNITS).
    pub(crate) failed_units: Box<dyn Check>,
    /// Required: [`GRUB_CFG_MARKER`](apex_boot::checks::names::GRUB_CFG_MARKER).
    pub(crate) grub_cfg: Box<dyn Check>,
    /// Reported (see [`reported::file_checks`]).
    pub(crate) reported: Vec<Box<dyn Check>>,
    /// The qualification hook file ([`force_red::FORCE_RED_PATH`] below `--root`).
    pub(crate) force_red: PathBuf,
}

/// Where the booted digest came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootedSource {
    /// `bootc status`.
    Bootc,
    /// The daemon's `Booted` property, because `bootc status` gave none.
    Daemon,
    /// Neither: the decision treats the digest as unknown.
    Unknown,
}

impl BootedSource {
    /// A short name for logs and `--dry-run`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BootedSource::Bootc => "bootc",
            BootedSource::Daemon => "apex-updated",
            BootedSource::Unknown => "unknown",
        }
    }
}

/// Everything known before the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Gathered {
    /// The decision's input (its `checks` are the required checks, in [`REQUIRED`](apex_boot::checks::names::REQUIRED)
    /// order).
    pub(crate) input: AssessInput,
    /// The reported checks, in [`REPORTED`](apex_boot::checks::names::REPORTED) order.
    pub(crate) reported: Vec<CheckResult>,
    /// Where `input.booted_digest` came from.
    pub(crate) booted_source: BootedSource,
}

/// Why the assessment cannot decide. Nothing has been changed when this is returned.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AssessError {
    /// grubenv cannot be read or is not an environment block.
    #[error(transparent)]
    Grubenv(#[from] ReadError),
    /// grubenv holds Apex variables that cannot be interpreted.
    #[error("{path}: {source}")]
    Vars {
        /// The grubenv file.
        path: String,
        /// What is wrong.
        #[source]
        source: BootVarsError,
    },
}

/// Reads the Apex variables from `path`.
pub(crate) fn read_vars(path: &Path) -> Result<BootVars, AssessError> {
    let env = grubenv::read_file(path)?;
    BootVars::from_env(&env).map_err(|source| AssessError::Vars {
        path: path.display().to_string(),
        source,
    })
}

/// Runs the checks and collects the decision's input.
///
/// 1. Reads grubenv once up front, so a block that cannot be acted on fails at once instead
///    of after minutes of checks.
/// 2. Waits, bounded by [`SESSION_WAIT`] from the start, for a session that proves the
///    desktop.
/// 3. Runs the required checks and the file-based reported checks in one concurrent batch,
///    with `bootc status` alongside, then resolves the booted digest (falling back to the
///    daemon) and the boot order ([`default_deployment`]).
/// 4. Adds the failed required check `forced-red` if the qualification hook names the
///    booted digest ([`force_red`]).
/// 5. Asks logind whether a user may be logged in; if logind cannot answer, assumes one
///    may be (never reboot on a guess).
/// 6. Reads grubenv again: this read is the one the decision uses, as late as possible.
///
/// Every bounded wait here and in [`perform`] is counted in
/// [`apex_boot::ASSESS_TIME_BUDGET`]; a new one must be added there.
pub(crate) fn gather(
    ports: &Ports,
    local: LocalChecks,
    grubenv_path: &Path,
) -> Result<Gathered, AssessError> {
    let start = ports.clock.now();
    read_vars(grubenv_path)?;

    match session::poll_for_desktop(&*ports.sessions, &*ports.clock, start + SESSION_WAIT) {
        Ok(found) => tracing::info!(
            "session {} ({}) is up after {} s",
            found.id,
            found.class,
            ports.clock.now().saturating_duration_since(start).as_secs()
        ),
        Err(reason) => tracing::warn!("{reason}; running the checks anyway"),
    }

    let LocalChecks {
        core_units,
        failed_units,
        grub_cfg,
        reported: reported_checks,
        force_red: force_red_path,
    } = local;
    // The order of names::REQUIRED, then the reported checks.
    let mut batch: Vec<Box<dyn Check>> = vec![
        session::check(Arc::clone(&ports.sessions), Arc::clone(&ports.clock)),
        core_units,
        ping::check(Arc::clone(&ports.updater), Arc::clone(&ports.clock)),
        failed_units,
        grub_cfg,
    ];
    let required_count = batch.len();
    batch.extend(reported_checks);
    let host = &ports.host;
    let (mut results, booted) = thread::scope(|scope| {
        let status = scope.spawn(|| host.booted());
        let results = run_checks(batch);
        let booted = status
            .join()
            .unwrap_or_else(|_| Err("bootc status panicked".to_owned()));
        (results, booted)
    });
    let mut reported = results.split_off(required_count);
    reported.push(reported::image_source(&booted));
    let mut required = results;
    for result in required.iter().chain(&reported) {
        if result.outcome.is_pass() {
            tracing::info!("check {}: pass", result.name);
        } else {
            tracing::warn!("check {}: {}", result.name, result.outcome);
        }
    }

    let (booted_digest, booted_source) = resolve_booted(&booted, &*ports.updater);
    let default_deployment = default_deployment(&booted);
    tracing::info!("entry 0 at the next boot: {default_deployment}");
    if let Some(forced) = forced_red(&force_red_path, booted_digest.as_ref()) {
        tracing::warn!("check {}: {}", forced.name, forced.outcome);
        required.push(forced);
    }

    let user_session_active = match ports.sessions.list() {
        Ok(sessions) => session::user_logged_in(&sessions),
        Err(err) => {
            // Fail safe: rebooting a user's session is what ADR 0009 forbids. If logind is
            // really broken nobody can log in through GDM; the user power-cycles, and GRUB
            // counts that boot anyway.
            tracing::warn!(
                "cannot list logind sessions ({err}); assuming a user may be logged in, so \
                 this boot is not rebooted automatically"
            );
            true
        }
    };

    let vars = read_vars(grubenv_path)?;
    Ok(Gathered {
        input: AssessInput {
            vars,
            booted_digest,
            default_deployment,
            checks: required,
            user_session_active,
        },
        reported,
        booted_source,
    })
}

/// The failed check the qualification hook adds for this boot, if its file names the
/// booted digest. An absent file is the normal case and says nothing; an ignored one is
/// logged and changes nothing either.
pub(crate) fn forced_red(path: &Path, booted: Option<&Digest>) -> Option<CheckResult> {
    let hook = force_red::read(path);
    match &hook {
        HookFile::Absent => {}
        HookFile::Ignored(reason) => {
            tracing::warn!("qualification hook ignored: {reason}");
        }
        HookFile::Names(named) => {
            if force_red::check(&hook, booted).is_none() {
                tracing::info!(
                    "qualification hook {} names {named:?}, not this boot; ignored",
                    path.display()
                );
            }
        }
    }
    force_red::check(&hook, booted)
}

/// The booted digest: `bootc status` first; if it gives none, the daemon's `Booted`.
///
/// The fallback matters for armed boots: with the digest unknown the decision cannot confirm
/// the target and judges the boot red, so a `bootc status` that fails in this process (for
/// example because of the unit's sandbox) would otherwise fail every update.
pub(crate) fn resolve_booted(
    booted: &Result<BootedImage, String>,
    updater: &dyn Updater,
) -> (Option<Digest>, BootedSource) {
    let parse = |digest: Option<String>, from: &str| {
        let digest = digest?;
        Digest::parse(&digest)
            .map_err(|err| {
                tracing::warn!("{from} reports an invalid booted digest {digest:?}: {err}");
            })
            .ok()
    };
    match booted {
        Ok(image) => {
            if let Some(digest) = parse(image.digest.clone(), "bootc") {
                return (Some(digest), BootedSource::Bootc);
            }
            tracing::warn!("bootc status names no booted digest; asking apex-updated");
        }
        Err(err) => tracing::warn!("bootc status failed ({err}); asking apex-updated"),
    }
    match updater.booted_digest() {
        Ok(digest) => match parse(digest, "apex-updated") {
            Some(digest) => (Some(digest), BootedSource::Daemon),
            None => (None, BootedSource::Unknown),
        },
        Err(err) => {
            tracing::warn!("apex-updated cannot name the booted digest either: {err}");
            (None, BootedSource::Unknown)
        }
    }
}

/// Which deployment is entry 0 at the next boot, from `bootc status`: unknown when the
/// status could not be read, or when it names a rollback digest that is not a valid one.
pub(crate) fn default_deployment(booted: &Result<BootedImage, String>) -> DefaultDeployment {
    let Ok(image) = booted else {
        return DefaultDeployment::Unknown;
    };
    let rollback = match image.rollback_digest.as_deref().map(Digest::parse) {
        None => None,
        Some(Ok(digest)) => Some(digest),
        Some(Err(err)) => {
            tracing::warn!("bootc reports an invalid rollback digest: {err}");
            return DefaultDeployment::Unknown;
        }
    };
    DefaultDeployment::from_status(image.rollback_queued, rollback)
}

/// How one step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    /// Performed.
    Done,
    /// Performed and failed.
    Failed(String),
    /// Not performed.
    Skipped(Skip),
}

/// Why a step was not performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// The step with this index failed, and [`Step::skipped_after_failure_of`] says so.
    AfterFailure(usize),
    /// The step runs only after a failure ([`Step::only_after_failure_of`]) that did not
    /// happen.
    NotNeeded,
    /// A user logged in before the reboot.
    UserLoggedIn,
    /// logind could not say whether a user logged in before the reboot.
    SessionUnknown,
}

/// What the steps act on besides the shared ports.
pub(crate) struct Effects<'a, W: GrubEnvWriter + ?Sized> {
    /// grubenv.
    pub(crate) writer: &'a mut W,
    /// `health-failure.json`.
    pub(crate) store: &'a dyn FailureStore,
    /// `systemctl reboot`.
    pub(crate) rebooter: &'a dyn Rebooter,
}

/// Performs `steps` in order with the library's failure policy: a failed step cancels the
/// later steps [`Step::skipped_after_failure_of`] names; the others still run; a step that
/// runs only after a failure ([`Step::only_after_failure_of`]) runs only if that failure
/// happened. Returns one outcome per step.
pub(crate) fn perform<W: GrubEnvWriter + ?Sized>(
    steps: &[Step],
    ports: &Ports,
    effects: &mut Effects<'_, W>,
) -> Vec<StepOutcome> {
    let mut outcomes: Vec<StepOutcome> = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let failed = |outcome: &StepOutcome| matches!(outcome, StepOutcome::Failed(_));
        let cancelled_by = steps.iter().zip(&outcomes).position(|(earlier, outcome)| {
            failed(outcome) && step.skipped_after_failure_of(earlier)
        });
        let mut conditions = steps
            .iter()
            .zip(&outcomes)
            .filter(|(earlier, _)| step.only_after_failure_of(earlier))
            .peekable();
        let needed = conditions.peek().is_none() || conditions.any(|(_, outcome)| failed(outcome));
        let outcome = match cancelled_by {
            Some(failed) => StepOutcome::Skipped(Skip::AfterFailure(failed)),
            None if !needed => StepOutcome::Skipped(Skip::NotNeeded),
            None => perform_one(step, ports, effects),
        };
        log_step(index, step, &outcome);
        outcomes.push(outcome);
    }
    outcomes
}

fn perform_one<W: GrubEnvWriter + ?Sized>(
    step: &Step,
    ports: &Ports,
    effects: &mut Effects<'_, W>,
) -> StepOutcome {
    let result = match step {
        Step::ApplyEnv(ops) => apply_ops(effects.writer, ops).map_err(|err| err.to_string()),
        Step::RecordHealthFailure {
            digest,
            failed_checks,
        } => effects.store.record(&HealthFailure::new(
            digest.clone(),
            failed_checks.clone(),
            ports.clock.unix_now(),
        )),
        Step::Report {
            verdict, digest, ..
        } => ports.updater.report_boot(*verdict, digest.as_ref()),
        Step::ReconcileFallback { target, booted } => reconcile(target, booted, ports, effects),
        Step::Reboot => {
            // Never reboot into a session that appeared since the decision, nor when logind
            // cannot say (the same fail-safe rule as the decision's input).
            match ports.sessions.list() {
                Ok(sessions) if session::user_logged_in(&sessions) => {
                    return StepOutcome::Skipped(Skip::UserLoggedIn);
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!("cannot re-check logind sessions ({err}); not rebooting");
                    return StepOutcome::Skipped(Skip::SessionUnknown);
                }
            }
            effects.rebooter.reboot()
        }
    };
    match result {
        Ok(()) => StepOutcome::Done,
        Err(err) => StepOutcome::Failed(err),
    }
}

/// [`Step::ReconcileFallback`]: the fallback report failed, but the daemon may have run
/// `bootc rollback` anyway. Read the boot order again and disarm if the target is no longer
/// the default; otherwise the hedge (`apex_counter=3`) stays.
fn reconcile<W: GrubEnvWriter + ?Sized>(
    target: &Digest,
    booted: &Digest,
    ports: &Ports,
    effects: &mut Effects<'_, W>,
) -> Result<(), String> {
    let status = ports.host.booted();
    let now = default_deployment(&status);
    if now == DefaultDeployment::Unknown {
        let why = status
            .err()
            .unwrap_or_else(|| "invalid rollback digest".to_owned());
        return Err(format!(
            "cannot read the boot order again ({why}); the hedge stays, so the next boots \
             are counted boots of entry 0"
        ));
    }
    let ops = reconcile_fallback(target, booted, &now);
    if ops.is_empty() {
        tracing::warn!(
            "{target} is still the default ({now}); the hedge stays: if apex-updated still \
             rolls back, the next boot starts {booted} and clears the arming, otherwise \
             {target} gets its attempts again"
        );
        return Ok(());
    }
    tracing::info!("the rollback happened after all ({now}); clearing the arming");
    apply_ops(effects.writer, &ops).map_err(|err| err.to_string())
}

fn log_step(index: usize, step: &Step, outcome: &StepOutcome) {
    let what = describe_step(step);
    match outcome {
        StepOutcome::Done => tracing::info!("step {index} ({what}): done"),
        StepOutcome::Failed(err) => tracing::error!("step {index} ({what}) failed: {err}"),
        StepOutcome::Skipped(Skip::AfterFailure(failed)) => {
            tracing::warn!("step {index} ({what}) skipped because step {failed} failed");
        }
        StepOutcome::Skipped(Skip::NotNeeded) => {
            tracing::debug!("step {index} ({what}) not needed");
        }
        StepOutcome::Skipped(Skip::UserLoggedIn) => tracing::warn!(
            "step {index} ({what}) skipped: a user has logged in; the failure waits for them"
        ),
        StepOutcome::Skipped(Skip::SessionUnknown) => tracing::warn!(
            "step {index} ({what}) skipped: logind cannot say whether a user is logged in; \
             the failure waits for the user"
        ),
    }
}

/// One line for a step.
pub(crate) fn describe_step(step: &Step) -> String {
    match step {
        Step::ApplyEnv(ops) => {
            let ops: Vec<String> = ops.iter().map(ToString::to_string).collect();
            format!("grubenv: {}", ops.join(", "))
        }
        Step::RecordHealthFailure { digest, .. } => format!("record health failure of {digest}"),
        Step::Report {
            verdict, digest, ..
        } => format!(
            "ReportBoot({verdict}, {})",
            digest.as_ref().map_or("\"\"", Digest::as_str)
        ),
        Step::ReconcileFallback { target, .. } => {
            format!("reconcile grubenv with the boot order (fallback from {target})")
        }
        Step::Reboot => "systemctl reboot".to_owned(),
    }
}

/// Whether any step failed (the process then exits with failure).
pub(crate) fn any_failed(outcomes: &[StepOutcome]) -> bool {
    outcomes.iter().any(|o| matches!(o, StepOutcome::Failed(_)))
}

#[cfg(test)]
mod tests;
