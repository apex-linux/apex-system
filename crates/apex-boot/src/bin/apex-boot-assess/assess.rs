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

#[derive(Clone)]
pub(crate) struct Ports {
    pub(crate) sessions: Arc<dyn Sessions>,
    pub(crate) updater: Arc<dyn Updater>,
    pub(crate) host: Arc<dyn HostStatus>,
    pub(crate) clock: Arc<dyn Clock>,
}

/// The checks that need neither logind nor D-Bus.
pub(crate) struct LocalChecks {
    pub(crate) core_units: Box<dyn Check>,
    pub(crate) failed_units: Box<dyn Check>,
    pub(crate) grub_cfg: Box<dyn Check>,
    pub(crate) reported: Vec<Box<dyn Check>>,
    pub(crate) force_red_path: PathBuf,
    /// The booted manifest's `fedora`, which names the channel tags.
    pub(crate) fedora_major: Result<u32, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootedSource {
    Bootc,
    Daemon,
    Unknown,
}

impl BootedSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BootedSource::Bootc => "bootc",
            BootedSource::Daemon => "apex-updated",
            BootedSource::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Gathered {
    pub(crate) input: AssessInput,
    pub(crate) reported: Vec<CheckResult>,
    pub(crate) booted_source: BootedSource,
}

/// Nothing has been changed when this is returned.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AssessError {
    #[error(transparent)]
    Grubenv(#[from] ReadError),
    #[error("{path}: {source}")]
    Vars {
        path: String,
        #[source]
        source: BootVarsError,
    },
}

pub(crate) fn read_vars(path: &Path) -> Result<BootVars, AssessError> {
    let env = grubenv::read_file(path)?;
    BootVars::from_env(&env).map_err(|source| AssessError::Vars {
        path: path.display().to_string(),
        source,
    })
}

/// Every bounded wait here and in [`perform`] must be counted in
/// [`apex_boot::ASSESS_TIME_BUDGET`].
pub(crate) fn gather(
    ports: &Ports,
    local: LocalChecks,
    grubenv_path: &Path,
) -> Result<Gathered, AssessError> {
    let start = ports.clock.now();
    // Fail now, not after minutes of checks, when grubenv cannot be acted on.
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
        force_red_path,
        fedora_major,
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
    reported.push(reported::image_source(&booted, &fedora_major));
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

    let user_session_active = user_may_be_logged_in(&*ports.sessions);

    // As late as possible: this is the read the decision acts on.
    let vars = read_vars(grubenv_path)?;
    Ok(Gathered {
        input: AssessInput {
            vars,
            booted_digest,
            default_deployment,
            required_checks: required,
            user_session_active,
        },
        reported,
        booted_source,
    })
}

/// Fails safe: a reboot must never end a user's session. If logind is really broken nobody
/// can log in through GDM, and GRUB counts the boot the user then power-cycles.
fn user_may_be_logged_in(sessions: &dyn Sessions) -> bool {
    match sessions.list() {
        Ok(sessions) => session::user_logged_in(&sessions),
        Err(err) => {
            tracing::warn!(
                "cannot list logind sessions ({err}); assuming a user may be logged in, so \
                 this boot is not rebooted automatically"
            );
            true
        }
    }
}

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

/// Falls back to the daemon's `Booted`: with the digest unknown an armed boot is judged
/// red, so a `bootc status` that fails in this process would otherwise fail every update.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    Done,
    Failed(String),
    Skipped(Skip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    AfterFailure { failed_step: usize },
    NotNeeded,
    UserLoggedIn,
    SessionUnknown,
}

pub(crate) struct Effects<'a, W: GrubEnvWriter + ?Sized> {
    pub(crate) writer: &'a mut W,
    pub(crate) store: &'a dyn FailureStore,
    pub(crate) rebooter: &'a dyn Rebooter,
}

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
            Some(failed_step) => StepOutcome::Skipped(Skip::AfterFailure { failed_step }),
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
            // A session may have appeared since the decision.
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
        StepOutcome::Skipped(Skip::AfterFailure { failed_step }) => {
            tracing::warn!("step {index} ({what}) skipped because step {failed_step} failed");
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

pub(crate) fn any_failed(outcomes: &[StepOutcome]) -> bool {
    outcomes.iter().any(|o| matches!(o, StepOutcome::Failed(_)))
}

#[cfg(test)]
mod tests;
