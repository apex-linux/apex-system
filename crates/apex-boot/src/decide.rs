//! The ADR 0009 decision table: a pure function from what is known after boot to what
//! `apex-boot-assess` must do.
//!
//! | Situation | Decision |
//! |---|---|
//! | Armed, booted ≠ target, counter exhausted (GRUB fell back) | [`Decision::Fallback`] |
//! | Armed, booted ≠ target, counter not yet counted (`3`) | [`Decision::ArmedForNextBoot`] |
//! | Armed, booted ≠ target, counted (`2`, `1`, `0`), the target may still be the default | [`Decision::TargetSkipped`] |
//! | Armed, booted ≠ target, counted but the target is certainly not the default, or no counter | [`Decision::Inconsistent`] |
//! | Armed, booted = target, but another deployment is the default | [`Decision::Inconsistent`] |
//! | All required checks green (booted = target, or not armed) | [`Decision::Green`] |
//! | Red, armed, booted = target (or unknown), user session active | [`Decision::RedArmedSession`] |
//! | Red, armed, booted = target (or unknown), no user session | [`Decision::RedArmedNoSession`] |
//! | Red, not armed | [`Decision::RedUnarmed`] |
//!
//! The rows are tried in this order. The rows about which deployment runs and which one
//! is the default come first, whatever the checks say: a green previous deployment after a
//! fallback must still be made the default before the variables are cleared, or the next
//! boot would start the failed target again, unprotected.
//!
//! Resolutions of cases the ADR table leaves open (reported with this crate):
//! - **Counter state.** "Exhausted" means GRUB selected entry 1 (it stores -1); see
//!   [`CounterState`]. After a red boot without a session, the decision asks for a reboot
//!   only if GRUB demonstrably counted this boot ([`CounterState::reboot_makes_progress`])
//!   and the booted digest is the target. Otherwise a reboot might not move the counter (the
//!   snippet is missing, `save_env` fails, or GRUB already fell back and the target still
//!   booted) and could repeat forever.
//! - **Arming newer than this boot.** Booted ≠ target with the counter still at 3 means
//!   `Apply` armed the counter after GRUB ran, during this boot. That arming belongs to the
//!   next boot, so it is left alone ([`Decision::ArmedForNextBoot`]) instead of being
//!   cleared as inconsistent.
//! - **Where the target is in the boot order** ([`DefaultDeployment`], from `bootc
//!   status`). GRUB picks entries by position: entry 0 is the default deployment, entry 1
//!   the other one. The ADR row "booted ≠ target, counter not exhausted → inconsistent,
//!   clear" is only safe when the target is not entry 0. GRUB also boots entry 1 while the
//!   target is still entry 0 when it cannot load the target (`fallback=1`) or when the
//!   previous entry is picked by hand; clearing the arming then would leave a broken
//!   target as the default without a counter or `fallback`. So a counted arming is kept
//!   ([`Decision::TargetSkipped`]) unless the target is certainly not the default, and GRUB
//!   goes on counting until it falls back for good.
//! - **Fallback without trusting the reply.** `apex-updated` finishes a queued fallback
//!   (`bootc rollback`, which swaps entries 0 and 1) even when the caller has given up, so a
//!   failed `ReportBoot("fallback")` does not mean nothing happened. Leaving `apex_counter`
//!   at -1 after the swap would make GRUB boot the failed target (now entry 1) on every
//!   boot. The fallback therefore first writes `apex_counter=3` (the *hedge*,
//!   [`HEDGE_COUNTER`]): whatever the default is after this boot gets counted boots with
//!   `fallback=1`, as after an `Apply`. If the rollback happened, that is the previous
//!   deployment and the next assessment that completes clears the arming
//!   ([`Decision::Inconsistent`]); if it did not, the target gets its attempts again and GRUB
//!   falls back again. Then the report, then the disarm. After a failed report
//!   [`Step::ReconcileFallback`] reads the boot order again and disarms at once when the
//!   rollback did happen ([`reconcile_fallback`]). If the hedge cannot be written, the
//!   report is not sent: a rollback must never be requested while grubenv cannot follow it.
//!   What remains: after a rollback whose disarm was lost, GRUB comes back to the target
//!   only if three boots of the previous deployment in a row end before their assessment
//!   writes grubenv (for example three quick shutdowns, or three stops in emergency mode,
//!   which the early-boot watchdog [`crate::emergency`] reboots). `apex-updated` disarming
//!   grubenv itself right after `bootc rollback` would close that too.
//! - **Unknown boot order at the last attempt.** Booted ≠ target, the counter at 0 and the
//!   boot order unknown: entry 1 at the next boot may be the target (after a rollback whose
//!   disarm was lost) or the previous deployment. Neither keeping the counter at 0 (GRUB
//!   would jump to entry 1) nor clearing it (the target may be the default) is safe, so the
//!   hedge is written again: entry 0 gets counted boots.
//! - **Unknown booted digest.** When armed but the booted digest is unknown, the
//!   deployment cannot be confirmed: a synthetic failed check [`names::BOOTED_DIGEST`] makes
//!   it red. That never clears the arming and never reboots: only a known booted = target
//!   proves that GRUB's default entry is the target, so a reboot could otherwise walk GRUB
//!   onto an unrelated entry 1.
//! - **Only what changes is written.** Green and inconsistent boots unset only the
//!   variables that are present, and confirm (`apex_ok=1`) only while armed, so an ordinary
//!   boot never writes grubenv at all.
//!
//! The caller must not let `Apply` arm the counter between reading grubenv and running
//! the decision's steps: every decision that clears the variables would clear the fresh
//! arming too. `apex-updated` must refuse `Apply` until this boot's assessment has
//! finished.

use std::fmt;

use crate::checks::{CheckResult, names};
use crate::digest::Digest;
use crate::vars::{BootVar, BootVars, CounterState, EnvOp, disarm_ops};

/// The value the fallback's hedge writes to `apex_counter`: counted boots of whatever entry
/// 0 is, as many as a fresh arming gets.
pub const HEDGE_COUNTER: i32 = crate::vars::BOOT_ATTEMPTS;

/// Which deployment is GRUB's entry 0 (the default) at the next boot, from `bootc status`
/// read during the assessment.
///
/// ostree writes one boot entry per deployment in boot order. bootc's `rollback` is the
/// first deployment other than the booted and the staged one, and `rollbackQueued` says it
/// comes before the booted one. A staged deployment is not among the boot entries until
/// ostree finalizes it at shutdown, and `Apply` during this boot is ruled out while the
/// assessment runs (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DefaultDeployment {
    /// `bootc status` could not be read.
    #[default]
    Unknown,
    /// The booted deployment is entry 0 (`rollbackQueued` is false).
    Booted,
    /// The rollback deployment is entry 0 (`rollbackQueued`); its image digest, if it has
    /// one.
    Rollback(Option<Digest>),
}

impl DefaultDeployment {
    /// From `bootc status`: `rollbackQueued` and the rollback deployment's digest.
    pub fn from_status(rollback_queued: bool, rollback: Option<Digest>) -> Self {
        if rollback_queued {
            Self::Rollback(rollback)
        } else {
            Self::Booted
        }
    }

    /// Whether `target` is entry 0, when the booted deployment is `booted`: `None` when
    /// that cannot be known.
    pub fn is_target(&self, target: &Digest, booted: Option<&Digest>) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::Booted => booted.map(|booted| booted == target),
            Self::Rollback(rollback) => Some(rollback.as_ref() == Some(target)),
        }
    }
}

impl fmt::Display for DefaultDeployment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown"),
            Self::Booted => f.write_str("booted"),
            Self::Rollback(Some(digest)) => write!(f, "rollback {digest}"),
            Self::Rollback(None) => f.write_str("rollback (no image)"),
        }
    }
}

/// Everything the decision depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssessInput {
    /// The Apex grubenv variables as read after boot.
    pub vars: BootVars,
    /// Manifest digest of the booted deployment, if it could be determined.
    pub booted_digest: Option<Digest>,
    /// Which deployment is entry 0 at the next boot.
    pub default_deployment: DefaultDeployment,
    /// Results of the required checks only. Reported checks never influence the decision.
    pub checks: Vec<CheckResult>,
    /// Whether a user (not only the greeter) may be logged in. The binary sets it when
    /// logind cannot answer, too: a reboot must never end somebody's session.
    pub user_session_active: bool,
}

/// The `verdict` argument of `ReportBoot` (D-Bus interface `io.github.sekirokenjii.Apex.Update1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// All required checks passed; digest = the booted deployment.
    Green,
    /// An armed boot failed; digest = the target. The daemon publishes `HealthFailure` and
    /// waits for the user.
    Red,
    /// A boot that was not armed failed a required check; report only.
    RedUnarmed,
    /// GRUB fell back to the previous deployment; digest = the failed target. The daemon
    /// runs `bootc rollback` and denylists the digest.
    Fallback,
}

impl Verdict {
    /// Every verdict.
    pub const ALL: [Verdict; 4] = [
        Verdict::Green,
        Verdict::Red,
        Verdict::RedUnarmed,
        Verdict::Fallback,
    ];

    /// The string sent over D-Bus.
    pub const fn as_str(self) -> &'static str {
        match self {
            Verdict::Green => "green",
            Verdict::Red => "red",
            Verdict::RedUnarmed => "red-unarmed",
            Verdict::Fallback => "fallback",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to do about this boot. Each variant is one row of the table in the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// All required checks passed: confirm the boot and clear the arming.
    Green {
        /// The booted deployment, if known.
        booted: Option<Digest>,
        /// While armed: `apex_ok=1`, then unset `apex_target` and `apex_counter` (those
        /// present). Not armed: unset a stray `apex_counter` if there is one; usually
        /// nothing.
        ops: Vec<EnvOp>,
    },
    /// The armed target is running, failed, and nobody is logged in.
    RedArmedNoSession {
        /// The target (also the booted deployment, unless that is unknown).
        target: Digest,
        /// Names of the failed required checks.
        failed_checks: Vec<String>,
        /// `apex_ok=0`, so the next boot is counted even if something confirmed this
        /// one.
        ops: Vec<EnvOp>,
        /// Reboot so the counter moves. False when a reboot would not make progress or the
        /// booted digest is unknown; the failure then waits for the user like
        /// [`Decision::RedArmedSession`].
        reboot: bool,
    },
    /// The armed target is running, failed, and a user may be logged in: never reboot.
    /// Publish the failure and let the user choose "Roll back".
    RedArmedSession {
        /// The target.
        target: Digest,
        /// Names of the failed required checks.
        failed_checks: Vec<String>,
    },
    /// An ordinary boot failed a required check: report only.
    RedUnarmed {
        /// The booted deployment, if known.
        booted: Option<Digest>,
        /// Names of the failed required checks.
        failed_checks: Vec<String>,
    },
    /// GRUB fell back to the previous deployment because the target used up its attempts.
    /// The daemon makes the running deployment the default (`bootc rollback`) and
    /// denylists the target; then the variables are cleared. See the module docs for why
    /// the hedge comes first.
    Fallback {
        /// The target that failed.
        failed_target: Digest,
        /// The running (previous) deployment.
        booted: Digest,
        /// `apex_counter=3` ([`HEDGE_COUNTER`]), written before the report.
        hedge: Vec<EnvOp>,
        /// Unset `apex_target` and `apex_counter`, only after the report succeeded.
        ops: Vec<EnvOp>,
    },
    /// Armed, but the arming no longer matches the boot order: another deployment runs and
    /// the target is certainly not the default (a manual `bootc rollback`, a lost disarm
    /// after a fallback, a stage that was never finalized) or GRUB does not count (no
    /// `apex_counter`); or the target runs but another deployment is the default. Clear the
    /// variables and report this boot as an ordinary one.
    Inconsistent {
        /// The target the counter was armed for.
        target: Digest,
        /// The running deployment.
        booted: Digest,
        /// Names of the failed required checks of this boot.
        failed_checks: Vec<String>,
        /// Clear the arming; when the checks passed, also `apex_ok=1` first.
        ops: Vec<EnvOp>,
    },
    /// Armed and counted, but another deployment runs while the target may still be the
    /// default: GRUB could not load the target and used `fallback=1`, or the previous entry
    /// was picked by hand. Keep the arming, so GRUB goes on counting the target's attempts
    /// and falls back for good once they are used up; report this boot as an ordinary one.
    TargetSkipped {
        /// The target the counter is armed for.
        target: Digest,
        /// The running deployment.
        booted: Digest,
        /// Names of the failed required checks of this boot.
        failed_checks: Vec<String>,
        /// Usually nothing. The hedge (`apex_counter=3`) when the counter is at 0 and the
        /// boot order is unknown (see the module docs).
        ops: Vec<EnvOp>,
    },
    /// Armed during this boot (GRUB has not counted it yet): the arming is for the next
    /// boot. Change nothing; report this boot as an ordinary one.
    ArmedForNextBoot {
        /// The target of the new arming.
        target: Digest,
        /// The running deployment.
        booted: Digest,
        /// Names of the failed required checks of this boot.
        failed_checks: Vec<String>,
    },
}

/// One action of a decision, in the order it must be performed.
///
/// When a step fails, the binary logs it, skips the later steps that
/// [`Step::skipped_after_failure_of`] names, carries on with the rest, and exits with
/// failure at the end. In short: a failed grubenv write cancels later grubenv writes, the
/// reboot and a required report (never reboot, and never ask for a rollback, when grubenv
/// cannot be written), but the failure is still recorded and best-effort reports are still
/// sent; a failed `must_succeed` report cancels everything after it except
/// [`Step::ReconcileFallback`], which runs only then ([`Step::only_after_failure_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Apply these grubenv operations (see [`crate::writer::apply_ops`]).
    ApplyEnv(Vec<EnvOp>),
    /// Write `/var/lib/apex/health-failure.json` (see [`crate::health::HealthFailure`]),
    /// so the failure is known even if the daemon cannot be reached.
    RecordHealthFailure {
        /// The failed digest.
        digest: Digest,
        /// Names of the failed required checks.
        failed_checks: Vec<String>,
    },
    /// Call `ReportBoot(verdict, digest)`; an unknown digest is sent as `""`.
    Report {
        /// The verdict.
        verdict: Verdict,
        /// The digest argument.
        digest: Option<Digest>,
        /// If the call fails, stop: skip every later step (but
        /// [`Step::ReconcileFallback`]) and exit with failure, so the next boot tries again.
        /// Otherwise log the failure and go on.
        must_succeed: bool,
    },
    /// Only after the fallback report failed: the daemon may still have run, or may still
    /// run, `bootc rollback`. Read the boot order again (`bootc status`) and apply
    /// [`reconcile_fallback`].
    ReconcileFallback {
        /// The failed target.
        target: Digest,
        /// The running deployment.
        booted: Digest,
    },
    /// `systemctl reboot`. Always the last step. Re-check right before it that no user
    /// session has appeared in the meantime; if one has, or logind cannot answer, skip it.
    Reboot,
}

impl Step {
    /// Whether this step must be skipped because the earlier step `failed` failed.
    pub fn skipped_after_failure_of(&self, failed: &Step) -> bool {
        match failed {
            Step::ApplyEnv(_) => matches!(
                self,
                Step::ApplyEnv(_)
                    | Step::Reboot
                    | Step::Report {
                        must_succeed: true,
                        ..
                    }
                    | Step::ReconcileFallback { .. }
            ),
            Step::Report {
                must_succeed: true, ..
            } => !matches!(self, Step::ReconcileFallback { .. }),
            Step::Report { .. }
            | Step::RecordHealthFailure { .. }
            | Step::ReconcileFallback { .. }
            | Step::Reboot => false,
        }
    }

    /// Whether this step runs only if the earlier step `earlier` was performed and failed:
    /// [`Step::ReconcileFallback`] after the required fallback report.
    pub fn only_after_failure_of(&self, earlier: &Step) -> bool {
        matches!(
            (self, earlier),
            (
                Step::ReconcileFallback { .. },
                Step::Report {
                    must_succeed: true,
                    ..
                }
            )
        )
    }
}

/// What [`Step::ReconcileFallback`] writes, given the boot order `now` read after the
/// fallback report failed:
///
/// - the target is certainly not the default any more (the daemon's `bootc rollback` ran):
///   unset `apex_target` and `apex_counter`, so the next boot starts the running deployment
///   unarmed;
/// - otherwise nothing. The hedge written before the report stays: if the daemon still
///   rolls back later, entry 0 is the running deployment and the next assessment clears
///   the arming; if it never does, the target gets its attempts again. Restoring -1
///   here would be wrong, because a daemon that is still busy with the queued fallback
///   would swap the entries afterwards.
pub fn reconcile_fallback(target: &Digest, booted: &Digest, now: &DefaultDeployment) -> Vec<EnvOp> {
    match now.is_target(target, Some(booted)) {
        Some(false) => disarm_ops(),
        Some(true) | None => Vec::new(),
    }
}

/// The decision table. Pure: see the module docs for the rows and their order.
pub fn decide(input: &AssessInput) -> Decision {
    let vars = &input.vars;
    let mut failed_checks: Vec<String> = input
        .checks
        .iter()
        .filter(|c| !c.outcome.is_pass())
        .map(|c| c.name.clone())
        .collect();

    let Some(target) = vars.apex_target.clone() else {
        return if failed_checks.is_empty() {
            Decision::Green {
                booted: input.booted_digest.clone(),
                ops: green_ops(vars),
            }
        } else {
            Decision::RedUnarmed {
                booted: input.booted_digest.clone(),
                failed_checks,
            }
        };
    };

    let counter = vars.counter_state();
    let target_is_default = input
        .default_deployment
        .is_target(&target, input.booted_digest.as_ref());
    match &input.booted_digest {
        Some(booted) if *booted != target => {
            let booted = booted.clone();
            match counter {
                CounterState::Exhausted => Decision::Fallback {
                    failed_target: target,
                    booted,
                    hedge: hedge_ops(),
                    ops: disarm_ops(),
                },
                CounterState::NotYetCounted => Decision::ArmedForNextBoot {
                    target,
                    booted,
                    failed_checks,
                },
                CounterState::Counted { remaining } if target_is_default != Some(false) => {
                    let ops = if target_is_default.is_none() && remaining == 0 {
                        hedge_ops()
                    } else {
                        Vec::new()
                    };
                    Decision::TargetSkipped {
                        target,
                        booted,
                        failed_checks,
                        ops,
                    }
                }
                CounterState::Counted { .. } | CounterState::Unset => {
                    inconsistent(target, booted, failed_checks, vars)
                }
            }
        }
        Some(booted) if target_is_default == Some(false) => {
            inconsistent(target, booted.clone(), failed_checks, vars)
        }
        booted => {
            if booted.is_none() {
                failed_checks.push(names::BOOTED_DIGEST.to_owned());
            }
            if failed_checks.is_empty() {
                Decision::Green {
                    booted: booted.clone(),
                    ops: green_ops(vars),
                }
            } else if input.user_session_active {
                Decision::RedArmedSession {
                    target,
                    failed_checks,
                }
            } else {
                Decision::RedArmedNoSession {
                    target,
                    failed_checks,
                    ops: vec![EnvOp::Set(BootVar::ApexOk, "0".to_owned())],
                    reboot: booted.is_some() && counter.reboot_makes_progress(),
                }
            }
        }
    }
}

fn inconsistent(
    target: Digest,
    booted: Digest,
    failed_checks: Vec<String>,
    vars: &BootVars,
) -> Decision {
    let ops = if failed_checks.is_empty() {
        green_ops(vars)
    } else {
        clear_ops(vars)
    };
    Decision::Inconsistent {
        target,
        booted,
        failed_checks,
        ops,
    }
}

/// Unset whichever of `apex_target` and `apex_counter` is present (one `grub2-editenv
/// unset` call, or none).
fn clear_ops(vars: &BootVars) -> Vec<EnvOp> {
    let mut ops = Vec::new();
    if vars.apex_target.is_some() {
        ops.push(EnvOp::Unset(BootVar::ApexTarget));
    }
    if vars.apex_counter.is_some() {
        ops.push(EnvOp::Unset(BootVar::ApexCounter));
    }
    ops
}

/// Confirm the boot, then clear the arming: while armed `apex_ok=1`, then unset the
/// variables that are present. Two `grub2-editenv` calls; a crash between them leaves
/// `apex_ok=1` with the arming intact, which GRUB treats as confirmed and the next
/// assessment completes. Not armed: `apex_ok` means nothing to GRUB, so only a stray
/// `apex_counter` is removed, and an ordinary boot writes nothing.
fn green_ops(vars: &BootVars) -> Vec<EnvOp> {
    let mut ops = Vec::new();
    if vars.is_armed() {
        ops.push(EnvOp::Set(BootVar::ApexOk, "1".to_owned()));
    }
    ops.extend(clear_ops(vars));
    ops
}

/// `apex_counter=3` (see [`HEDGE_COUNTER`] and the module docs).
fn hedge_ops() -> Vec<EnvOp> {
    vec![EnvOp::Set(BootVar::ApexCounter, HEDGE_COUNTER.to_string())]
}

impl Decision {
    /// The grubenv operations of this decision's [`Step::ApplyEnv`] steps, in order (empty
    /// when it changes nothing). The fallback's disarm is among them although it runs only
    /// after a successful report; what [`Step::ReconcileFallback`] may write is not.
    pub fn ops(&self) -> Vec<EnvOp> {
        match self {
            Decision::Green { ops, .. }
            | Decision::RedArmedNoSession { ops, .. }
            | Decision::Inconsistent { ops, .. }
            | Decision::TargetSkipped { ops, .. } => ops.clone(),
            Decision::Fallback { hedge, ops, .. } => hedge.iter().chain(ops).cloned().collect(),
            Decision::RedArmedSession { .. }
            | Decision::RedUnarmed { .. }
            | Decision::ArmedForNextBoot { .. } => Vec::new(),
        }
    }

    /// The verdict to report with `ReportBoot`.
    pub fn verdict(&self) -> Verdict {
        match self {
            Decision::Green { .. } => Verdict::Green,
            Decision::RedArmedNoSession { .. } | Decision::RedArmedSession { .. } => Verdict::Red,
            Decision::RedUnarmed { .. } => Verdict::RedUnarmed,
            Decision::Fallback { .. } => Verdict::Fallback,
            Decision::Inconsistent { failed_checks, .. }
            | Decision::TargetSkipped { failed_checks, .. }
            | Decision::ArmedForNextBoot { failed_checks, .. } => {
                if failed_checks.is_empty() {
                    Verdict::Green
                } else {
                    Verdict::RedUnarmed
                }
            }
        }
    }

    /// The digest to report with the verdict: the target for `red` and `fallback`, the
    /// booted deployment otherwise.
    pub fn report_digest(&self) -> Option<&Digest> {
        match self {
            Decision::Green { booted, .. } | Decision::RedUnarmed { booted, .. } => booted.as_ref(),
            Decision::RedArmedNoSession { target, .. }
            | Decision::RedArmedSession { target, .. } => Some(target),
            Decision::Fallback { failed_target, .. } => Some(failed_target),
            Decision::Inconsistent { booted, .. }
            | Decision::TargetSkipped { booted, .. }
            | Decision::ArmedForNextBoot { booted, .. } => Some(booted),
        }
    }

    /// Whether this decision reboots the machine.
    pub fn reboot(&self) -> bool {
        matches!(self, Decision::RedArmedNoSession { reboot: true, .. })
    }

    /// Names of the failed required checks (empty for green and fallback decisions).
    pub fn failed_checks(&self) -> &[String] {
        match self {
            Decision::RedArmedNoSession { failed_checks, .. }
            | Decision::RedArmedSession { failed_checks, .. }
            | Decision::RedUnarmed { failed_checks, .. }
            | Decision::Inconsistent { failed_checks, .. }
            | Decision::TargetSkipped { failed_checks, .. }
            | Decision::ArmedForNextBoot { failed_checks, .. } => failed_checks,
            Decision::Green { .. } | Decision::Fallback { .. } => &[],
        }
    }

    /// The steps to perform, in order.
    ///
    /// - Green, Inconsistent, target skipped: write grubenv if there is anything to write,
    ///   then report (best effort).
    /// - Red armed: record `health-failure.json` and report (best effort); without a
    ///   session, first `apex_ok=0`, and reboot last if the counter makes progress.
    /// - Red unarmed, armed for next boot: report only.
    /// - Fallback: the hedge (`apex_counter=3`), the report (required: the daemon's `bootc
    ///   rollback` must make the running deployment the default before the variables are
    ///   cleared), the disarm, and after a failed report the reconciliation. See the
    ///   module docs.
    pub fn steps(&self) -> Vec<Step> {
        let report = |must_succeed: bool| Step::Report {
            verdict: self.verdict(),
            digest: self.report_digest().cloned(),
            must_succeed,
        };
        let write_then_report = |ops: &Vec<EnvOp>| {
            let mut steps = Vec::new();
            if !ops.is_empty() {
                steps.push(Step::ApplyEnv(ops.clone()));
            }
            steps.push(report(false));
            steps
        };
        match self {
            Decision::Green { ops, .. }
            | Decision::Inconsistent { ops, .. }
            | Decision::TargetSkipped { ops, .. } => write_then_report(ops),
            Decision::RedArmedNoSession {
                target,
                failed_checks,
                ops,
                reboot,
            } => {
                let mut steps = vec![
                    Step::ApplyEnv(ops.clone()),
                    Step::RecordHealthFailure {
                        digest: target.clone(),
                        failed_checks: failed_checks.clone(),
                    },
                    report(false),
                ];
                if *reboot {
                    steps.push(Step::Reboot);
                }
                steps
            }
            Decision::RedArmedSession {
                target,
                failed_checks,
            } => vec![
                Step::RecordHealthFailure {
                    digest: target.clone(),
                    failed_checks: failed_checks.clone(),
                },
                report(false),
            ],
            Decision::RedUnarmed { .. } | Decision::ArmedForNextBoot { .. } => vec![report(false)],
            Decision::Fallback {
                failed_target,
                booted,
                hedge,
                ops,
            } => vec![
                Step::ApplyEnv(hedge.clone()),
                report(true),
                Step::ApplyEnv(ops.clone()),
                Step::ReconcileFallback {
                    target: failed_target.clone(),
                    booted: booted.clone(),
                },
            ],
        }
    }

    /// A short name for the row, for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Decision::Green { .. } => "green",
            Decision::RedArmedNoSession { .. } => "red-armed-no-session",
            Decision::RedArmedSession { .. } => "red-armed-session",
            Decision::RedUnarmed { .. } => "red-unarmed",
            Decision::Fallback { .. } => "fallback",
            Decision::Inconsistent { .. } => "inconsistent",
            Decision::TargetSkipped { .. } => "target-skipped",
            Decision::ArmedForNextBoot { .. } => "armed-for-next-boot",
        }
    }
}

impl fmt::Display for Decision {
    /// One line for the journal: the row, the verdict, the operations and the failed
    /// checks.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (report {}", self.kind(), self.verdict())?;
        if let Some(digest) = self.report_digest() {
            write!(f, " {digest}")?;
        }
        f.write_str(")")?;
        let ops = self.ops();
        if !ops.is_empty() {
            let ops: Vec<String> = ops.iter().map(ToString::to_string).collect();
            write!(f, "; grubenv: {}", ops.join(", "))?;
        }
        if !self.failed_checks().is_empty() {
            write!(f, "; failed: {}", self.failed_checks().join(", "))?;
        }
        if self.reboot() {
            f.write_str("; reboot")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckOutcome;
    use proptest::prelude::*;

    fn digest(n: u8) -> Digest {
        Digest::parse(&format!(
            "sha256:{}",
            format!("{n:x}").repeat(64).get(..64).unwrap()
        ))
        .unwrap()
    }

    fn target() -> Digest {
        digest(0xa)
    }

    fn previous() -> Digest {
        digest(0xb)
    }

    fn green_checks() -> Vec<CheckResult> {
        names::REQUIRED
            .iter()
            .map(|n| CheckResult::pass(*n))
            .collect()
    }

    fn red_checks() -> Vec<CheckResult> {
        let mut checks = green_checks();
        checks[3] = CheckResult::fail(names::FAILED_UNITS, "failed units: gdm.service");
        checks[0] = CheckResult::timeout(names::SESSION);
        checks
    }

    fn armed(counter: Option<i32>) -> BootVars {
        BootVars {
            apex_target: Some(target()),
            apex_counter: counter,
            apex_ok: Some(false),
        }
    }

    /// The boot order as `bootc status` reports it when `first` is entry 0 and `booted`
    /// runs.
    fn order(first: &Digest, booted: &Digest) -> DefaultDeployment {
        if first == booted {
            DefaultDeployment::Booted
        } else {
            DefaultDeployment::Rollback(Some(first.clone()))
        }
    }

    fn input(
        vars: BootVars,
        booted: Option<Digest>,
        default_deployment: DefaultDeployment,
        checks: Vec<CheckResult>,
        session: bool,
    ) -> AssessInput {
        AssessInput {
            vars,
            booted_digest: booted,
            default_deployment,
            checks,
            user_session_active: session,
        }
    }

    /// The booted deployment is entry 0: the usual situation.
    fn plain(
        vars: BootVars,
        booted: Option<Digest>,
        checks: Vec<CheckResult>,
        session: bool,
    ) -> AssessInput {
        input(vars, booted, DefaultDeployment::Booted, checks, session)
    }

    fn green_ops_expected() -> Vec<EnvOp> {
        vec![
            EnvOp::Set(BootVar::ApexOk, "1".into()),
            EnvOp::Unset(BootVar::ApexTarget),
            EnvOp::Unset(BootVar::ApexCounter),
        ]
    }

    fn clear_ops_expected() -> Vec<EnvOp> {
        vec![
            EnvOp::Unset(BootVar::ApexTarget),
            EnvOp::Unset(BootVar::ApexCounter),
        ]
    }

    fn hedge_expected() -> Vec<EnvOp> {
        vec![EnvOp::Set(BootVar::ApexCounter, "3".into())]
    }

    fn report(verdict: Verdict, digest: Digest, must_succeed: bool) -> Step {
        Step::Report {
            verdict,
            digest: Some(digest),
            must_succeed,
        }
    }

    #[test]
    fn verdict_strings_match_the_dbus_contract() {
        let xml = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../apex-dbus/data/io.github.sekirokenjii.Apex.Update1.xml"
        ))
        .unwrap();
        // The verdicts are documented in the comment right before the method.
        let method = xml.find("<method name=\"ReportBoot\">").unwrap();
        let doc = xml[..method].rfind("verdict:").unwrap();
        let report_boot = &xml[doc..method];
        for verdict in Verdict::ALL {
            assert!(
                report_boot.contains(&format!("\"{}\"", verdict.as_str())),
                "{verdict} not documented next to ReportBoot"
            );
        }
    }

    #[test]
    fn the_boot_order_names_entry_zero() {
        assert_eq!(
            DefaultDeployment::from_status(false, Some(previous())),
            DefaultDeployment::Booted
        );
        assert_eq!(
            DefaultDeployment::from_status(true, Some(target())),
            DefaultDeployment::Rollback(Some(target()))
        );
        let t = target();
        let p = previous();
        assert_eq!(
            DefaultDeployment::Booted.is_target(&t, Some(&t)),
            Some(true)
        );
        assert_eq!(
            DefaultDeployment::Booted.is_target(&t, Some(&p)),
            Some(false)
        );
        assert_eq!(DefaultDeployment::Booted.is_target(&t, None), None);
        assert_eq!(order(&t, &p).is_target(&t, Some(&p)), Some(true));
        assert_eq!(order(&p, &t).is_target(&t, Some(&t)), Some(false));
        assert_eq!(
            DefaultDeployment::Rollback(None).is_target(&t, Some(&p)),
            Some(false)
        );
        assert_eq!(DefaultDeployment::Unknown.is_target(&t, Some(&p)), None);
    }

    // Row: all required checks green.
    #[test]
    fn green_armed_target() {
        for default in [DefaultDeployment::Booted, DefaultDeployment::Unknown] {
            for counter in [Some(3), Some(2), Some(1), Some(0), Some(-1)] {
                let decision = decide(&input(
                    armed(counter),
                    Some(target()),
                    default.clone(),
                    green_checks(),
                    false,
                ));
                assert_eq!(
                    decision,
                    Decision::Green {
                        booted: Some(target()),
                        ops: green_ops_expected()
                    },
                    "{counter:?}"
                );
                assert_eq!(decision.verdict().as_str(), "green");
                assert_eq!(decision.report_digest(), Some(&target()));
            }
        }
        // Only what is there is unset.
        let decision = decide(&plain(armed(None), Some(target()), green_checks(), false));
        assert_eq!(
            decision.ops(),
            [
                EnvOp::Set(BootVar::ApexOk, "1".into()),
                EnvOp::Unset(BootVar::ApexTarget)
            ]
        );
    }

    /// Finding: every ordinary boot rewrote grubenv twice for nothing.
    #[test]
    fn an_ordinary_green_boot_writes_nothing() {
        for booted in [Some(previous()), None] {
            for vars in [
                BootVars::default(),
                BootVars {
                    apex_ok: Some(false),
                    ..BootVars::default()
                },
                BootVars {
                    apex_ok: Some(true),
                    ..BootVars::default()
                },
            ] {
                let decision = decide(&plain(vars, booted.clone(), green_checks(), true));
                assert_eq!(
                    decision,
                    Decision::Green {
                        booted: booted.clone(),
                        ops: vec![]
                    }
                );
                assert_eq!(
                    decision.steps(),
                    vec![Step::Report {
                        verdict: Verdict::Green,
                        digest: booted.clone(),
                        must_succeed: false
                    }]
                );
            }
        }
        // A stray counter without a target is removed, and nothing is confirmed.
        let stray = BootVars {
            apex_counter: Some(-1),
            ..BootVars::default()
        };
        let decision = decide(&plain(stray, Some(previous()), green_checks(), true));
        assert_eq!(decision.ops(), [EnvOp::Unset(BootVar::ApexCounter)]);
        // No required checks at all is vacuously green; the binary always runs them.
        assert!(matches!(
            decide(&plain(BootVars::default(), None, Vec::new(), false)),
            Decision::Green { .. }
        ));
    }

    // Row: red, armed, booted = target, no user session.
    #[test]
    fn red_armed_no_session_reboots_while_the_counter_moves() {
        for remaining in [2, 1, 0] {
            let decision = decide(&plain(
                armed(Some(remaining)),
                Some(target()),
                red_checks(),
                false,
            ));
            assert_eq!(
                decision,
                Decision::RedArmedNoSession {
                    target: target(),
                    failed_checks: vec![names::SESSION.into(), names::FAILED_UNITS.into()],
                    ops: vec![EnvOp::Set(BootVar::ApexOk, "0".into())],
                    reboot: true,
                }
            );
            assert!(decision.reboot());
            assert_eq!(decision.verdict(), Verdict::Red);
            assert_eq!(decision.steps().last(), Some(&Step::Reboot));
        }
    }

    #[test]
    fn red_armed_no_session_does_not_reboot_when_it_cannot_help() {
        // 3: GRUB did not count this boot. -1: GRUB fell back yet the target runs.
        // None: GRUB never counts. Rebooting could loop forever in each case.
        for counter in [Some(3), Some(-1), Some(9), None] {
            let decision = decide(&plain(armed(counter), Some(target()), red_checks(), false));
            assert!(
                matches!(decision, Decision::RedArmedNoSession { reboot: false, .. }),
                "{counter:?}: {decision:?}"
            );
            assert!(!decision.steps().contains(&Step::Reboot));
            assert_eq!(decision.verdict(), Verdict::Red);
        }
    }

    // Row: red, armed, booted = target, user session active.
    #[test]
    fn red_armed_session_never_reboots() {
        for counter in [Some(2), Some(1), Some(0), Some(3), Some(-1), None] {
            let decision = decide(&plain(armed(counter), Some(target()), red_checks(), true));
            assert_eq!(
                decision,
                Decision::RedArmedSession {
                    target: target(),
                    failed_checks: vec![names::SESSION.into(), names::FAILED_UNITS.into()],
                }
            );
            assert!(decision.ops().is_empty());
            assert_eq!(
                decision.steps(),
                vec![
                    Step::RecordHealthFailure {
                        digest: target(),
                        failed_checks: vec![names::SESSION.into(), names::FAILED_UNITS.into()],
                    },
                    report(Verdict::Red, target(), false),
                ]
            );
        }
    }

    // Row: red, not armed.
    #[test]
    fn red_unarmed_reports_only() {
        for session in [false, true] {
            for counter in [None, Some(0), Some(3)] {
                let vars = BootVars {
                    apex_counter: counter,
                    ..BootVars::default()
                };
                let decision = decide(&plain(vars, Some(previous()), red_checks(), session));
                assert_eq!(
                    decision,
                    Decision::RedUnarmed {
                        booted: Some(previous()),
                        failed_checks: vec![names::SESSION.into(), names::FAILED_UNITS.into()],
                    }
                );
                assert_eq!(
                    decision.steps(),
                    vec![report(Verdict::RedUnarmed, previous(), false)]
                );
            }
        }
    }

    // Row: armed, booted ≠ target, counter exhausted.
    #[test]
    fn fallback_hedges_then_reports_then_clears() {
        for checks in [green_checks(), red_checks()] {
            for session in [false, true] {
                for default in [
                    order(&target(), &previous()),
                    DefaultDeployment::Booted,
                    DefaultDeployment::Unknown,
                ] {
                    let decision = decide(&input(
                        armed(Some(-1)),
                        Some(previous()),
                        default,
                        checks.clone(),
                        session,
                    ));
                    assert_eq!(
                        decision,
                        Decision::Fallback {
                            failed_target: target(),
                            booted: previous(),
                            hedge: hedge_expected(),
                            ops: clear_ops_expected(),
                        }
                    );
                    assert_eq!(
                        decision.steps(),
                        vec![
                            Step::ApplyEnv(hedge_expected()),
                            report(Verdict::Fallback, target(), true),
                            Step::ApplyEnv(clear_ops_expected()),
                            Step::ReconcileFallback {
                                target: target(),
                                booted: previous()
                            },
                        ]
                    );
                }
            }
        }
    }

    /// After a failed fallback report: disarm only once the target is certainly not the
    /// default any more; otherwise keep the hedge (never back to -1).
    #[test]
    fn reconciliation_after_a_failed_fallback_report() {
        let (t, p) = (target(), previous());
        assert_eq!(
            reconcile_fallback(&t, &p, &DefaultDeployment::Booted),
            clear_ops_expected()
        );
        assert_eq!(
            reconcile_fallback(&t, &p, &DefaultDeployment::Rollback(Some(digest(0xc)))),
            clear_ops_expected()
        );
        for still in [order(&t, &p), DefaultDeployment::Unknown] {
            assert!(reconcile_fallback(&t, &p, &still).is_empty(), "{still}");
        }
    }

    // Row: armed, booted ≠ target, counted, target certainly not the default; or no
    // counter.
    #[test]
    fn inconsistent_clears_and_reports_the_boot() {
        let cases = [
            (Some(2), DefaultDeployment::Booted),
            (Some(1), DefaultDeployment::Booted),
            (Some(0), DefaultDeployment::Booted),
            (Some(0), DefaultDeployment::Rollback(Some(digest(0xc)))),
            (None, DefaultDeployment::Booted),
            (None, order(&target(), &previous())),
            (None, DefaultDeployment::Unknown),
        ];
        for (counter, default) in cases {
            let green = decide(&input(
                armed(counter),
                Some(previous()),
                default.clone(),
                green_checks(),
                false,
            ));
            let mut expected = green_ops_expected();
            if counter.is_none() {
                expected.pop();
            }
            assert_eq!(
                green,
                Decision::Inconsistent {
                    target: target(),
                    booted: previous(),
                    failed_checks: vec![],
                    ops: expected.clone(),
                },
                "{counter:?} {default}"
            );
            assert_eq!(green.verdict(), Verdict::Green);
            assert_eq!(green.report_digest(), Some(&previous()));

            let red = decide(&input(
                armed(counter),
                Some(previous()),
                default,
                red_checks(),
                false,
            ));
            assert_eq!(
                red,
                Decision::Inconsistent {
                    target: target(),
                    booted: previous(),
                    failed_checks: vec![names::SESSION.into(), names::FAILED_UNITS.into()],
                    ops: expected[1..].to_vec(),
                }
            );
            assert_eq!(red.verdict(), Verdict::RedUnarmed);
            assert!(!red.reboot());
        }
    }

    /// Finding: GRUB booted the previous entry (`fallback=1` after a load failure, or a
    /// manual pick) while the target is still the default. The arming must stay, or the
    /// broken target boots unarmed from then on.
    #[test]
    fn the_arming_stays_while_the_target_may_be_the_default() {
        for counter in [2, 1, 0] {
            for checks in [green_checks(), red_checks()] {
                let decision = decide(&input(
                    armed(Some(counter)),
                    Some(previous()),
                    order(&target(), &previous()),
                    checks.clone(),
                    false,
                ));
                let failed = if checks == green_checks() {
                    vec![]
                } else {
                    vec![names::SESSION.to_owned(), names::FAILED_UNITS.to_owned()]
                };
                assert_eq!(
                    decision,
                    Decision::TargetSkipped {
                        target: target(),
                        booted: previous(),
                        failed_checks: failed.clone(),
                        ops: vec![],
                    }
                );
                assert_eq!(
                    decision.steps(),
                    vec![report(decision.verdict(), previous(), false)]
                );
                assert_eq!(
                    decision.verdict(),
                    if failed.is_empty() {
                        Verdict::Green
                    } else {
                        Verdict::RedUnarmed
                    }
                );
            }
        }
        // Unknown boot order: keep while attempts remain; at 0 hedge, because entry 1 may
        // be the target.
        for (counter, ops) in [(2, vec![]), (1, vec![]), (0, hedge_expected())] {
            let decision = decide(&input(
                armed(Some(counter)),
                Some(previous()),
                DefaultDeployment::Unknown,
                green_checks(),
                true,
            ));
            assert_eq!(
                decision,
                Decision::TargetSkipped {
                    target: target(),
                    booted: previous(),
                    failed_checks: vec![],
                    ops,
                }
            );
        }
    }

    /// The target runs, but a rollback to another deployment is queued (for example a
    /// fallback whose disarm was lost): clear, so the next boot starts the default instead
    /// of entry 1.
    #[test]
    fn a_target_that_is_no_longer_the_default_is_disarmed() {
        for counter in [Some(2), Some(0), Some(-1), Some(3)] {
            for session in [false, true] {
                let decision = decide(&input(
                    armed(counter),
                    Some(target()),
                    order(&previous(), &target()),
                    red_checks(),
                    session,
                ));
                assert_eq!(
                    decision,
                    Decision::Inconsistent {
                        target: target(),
                        booted: target(),
                        failed_checks: vec![names::SESSION.into(), names::FAILED_UNITS.into()],
                        ops: clear_ops_expected(),
                    }
                );
                assert!(!decision.reboot());
            }
            let green = decide(&input(
                armed(counter),
                Some(target()),
                order(&previous(), &target()),
                green_checks(),
                false,
            ));
            assert_eq!(green.kind(), "inconsistent");
            assert_eq!(green.ops(), green_ops_expected());
        }
    }

    #[test]
    fn a_fresh_arming_is_left_for_the_next_boot() {
        let decision = decide(&plain(
            armed(Some(3)),
            Some(previous()),
            green_checks(),
            false,
        ));
        assert_eq!(
            decision,
            Decision::ArmedForNextBoot {
                target: target(),
                booted: previous(),
                failed_checks: vec![],
            }
        );
        assert!(decision.ops().is_empty());
        assert_eq!(decision.verdict(), Verdict::Green);
        let red = decide(&plain(
            armed(Some(3)),
            Some(previous()),
            red_checks(),
            false,
        ));
        assert_eq!(red.verdict(), Verdict::RedUnarmed);
        assert!(red.ops().is_empty());
        assert!(!red.reboot());
    }

    /// Finding: an unknown booted digest must never reboot; only a known booted = target
    /// proves what GRUB's default entry is.
    #[test]
    fn unknown_booted_digest_while_armed_is_red_and_never_reboots() {
        for counter in [Some(2), Some(1), Some(0), Some(-1), Some(3), None] {
            for default in [DefaultDeployment::Unknown, DefaultDeployment::Booted] {
                let decision = decide(&input(armed(counter), None, default, green_checks(), false));
                assert_eq!(
                    decision,
                    Decision::RedArmedNoSession {
                        target: target(),
                        failed_checks: vec![names::BOOTED_DIGEST.into()],
                        ops: vec![EnvOp::Set(BootVar::ApexOk, "0".into())],
                        reboot: false,
                    }
                );
                assert!(!decision.steps().contains(&Step::Reboot));
            }
        }
        let decision = decide(&plain(armed(Some(-1)), None, green_checks(), true));
        assert_eq!(
            decision,
            Decision::RedArmedSession {
                target: target(),
                failed_checks: vec![names::BOOTED_DIGEST.into()],
            }
        );
    }

    #[test]
    fn failure_policy() {
        let env = Step::ApplyEnv(vec![]);
        let record = Step::RecordHealthFailure {
            digest: target(),
            failed_checks: vec![],
        };
        let report = |must_succeed| Step::Report {
            verdict: Verdict::Red,
            digest: None,
            must_succeed,
        };
        let reconcile = Step::ReconcileFallback {
            target: target(),
            booted: previous(),
        };
        // A failed grubenv write: no more writes, no reboot, no required report (never ask
        // for a rollback grubenv cannot follow), but record and best-effort reports.
        assert!(Step::Reboot.skipped_after_failure_of(&env));
        assert!(env.skipped_after_failure_of(&env));
        assert!(report(true).skipped_after_failure_of(&env));
        assert!(reconcile.skipped_after_failure_of(&env));
        assert!(!record.skipped_after_failure_of(&env));
        assert!(!report(false).skipped_after_failure_of(&env));
        // A failed required report stops everything but the reconciliation, which runs
        // only then.
        for next in [&env, &record, &report(false), &Step::Reboot] {
            assert!(next.skipped_after_failure_of(&report(true)));
            assert!(!next.only_after_failure_of(&report(true)));
        }
        assert!(!reconcile.skipped_after_failure_of(&report(true)));
        assert!(reconcile.only_after_failure_of(&report(true)));
        for earlier in [&env, &record, &report(false), &Step::Reboot] {
            assert!(!reconcile.only_after_failure_of(earlier));
        }
        // Best-effort steps never stop the others; in particular a failed report does not
        // cancel the reboot that moves the counter.
        for failed in [&record, &report(false), &reconcile] {
            for next in [&env, &record, &report(false), &Step::Reboot, &reconcile] {
                assert!(!next.skipped_after_failure_of(failed));
            }
        }
    }

    #[test]
    fn display_is_one_line() {
        let decision = decide(&plain(armed(Some(0)), Some(target()), red_checks(), false));
        let line = decision.to_string();
        assert!(!line.contains('\n'));
        assert!(line.starts_with("red-armed-no-session (report red sha256:aaaa"));
        assert!(line.ends_with("; grubenv: set apex_ok=0; failed: session, failed-units; reboot"));
        assert_eq!(DefaultDeployment::Unknown.to_string(), "unknown");
        assert_eq!(
            DefaultDeployment::Rollback(None).to_string(),
            "rollback (no image)"
        );
    }

    /// The whole table on a grid of inputs, as a golden file reviewers can read.
    /// `entry0` is the deployment `bootc status` names as the default ("?" when it could
    /// not be read; always so when the booted digest is unknown).
    #[test]
    fn decision_table_snapshot() {
        use std::fmt::Write as _;
        let counters = [None, Some(3), Some(2), Some(0), Some(-1)];
        let places = [
            ("target", "target"),
            ("target", "previous"),
            ("target", "?"),
            ("previous", "target"),
            ("previous", "previous"),
            ("previous", "?"),
            ("unknown", "?"),
        ];
        let named = |name: &str| match name {
            "target" => Some(target()),
            "previous" => Some(previous()),
            _ => None,
        };
        let mut table =
            String::from("armed | counter | booted | entry0 | checks | session => decision\n");
        for armed_target in [false, true] {
            for counter in counters {
                for (booted, entry0) in places {
                    for green in [true, false] {
                        for session in [false, true] {
                            let vars = BootVars {
                                apex_target: armed_target.then(target),
                                apex_counter: counter,
                                apex_ok: Some(false),
                            };
                            let booted_digest = named(booted);
                            let default = match (named(entry0), &booted_digest) {
                                (Some(first), Some(running)) => order(&first, running),
                                _ => DefaultDeployment::Unknown,
                            };
                            let checks = if green { green_checks() } else { red_checks() };
                            let decision =
                                decide(&input(vars, booted_digest, default, checks, session));
                            writeln!(
                                table,
                                "{armed_target} | {counter:?} | {booted} | {entry0} | {} | {session} => {}",
                                if green { "green" } else { "red" },
                                decision
                                    .to_string()
                                    .replace(&target().to_string(), "T")
                                    .replace(&previous().to_string(), "P"),
                            )
                            .unwrap();
                        }
                    }
                }
            }
        }
        insta::assert_snapshot!(table);
    }

    fn arb_digest() -> impl Strategy<Value = Digest> {
        prop_oneof![Just(target()), Just(previous()), Just(digest(0xc))]
    }

    fn arb_default() -> impl Strategy<Value = DefaultDeployment> {
        prop_oneof![
            Just(DefaultDeployment::Unknown),
            Just(DefaultDeployment::Booted),
            proptest::option::of(arb_digest()).prop_map(DefaultDeployment::Rollback),
        ]
    }

    fn arb_input() -> impl Strategy<Value = AssessInput> {
        let counter = prop_oneof![
            Just(None),
            (-3i32..=5).prop_map(Some),
            any::<i32>().prop_map(Some),
        ];
        let outcome = prop_oneof![
            Just(CheckOutcome::Pass),
            Just(CheckOutcome::Timeout),
            "[a-z ]{0,10}".prop_map(CheckOutcome::Fail),
        ];
        let checks = proptest::collection::vec(("[a-z-]{1,12}", outcome), 0..6).prop_map(|v| {
            v.into_iter()
                .map(|(name, outcome)| CheckResult { name, outcome })
                .collect()
        });
        (
            proptest::option::of(arb_digest()),
            counter,
            proptest::option::of(any::<bool>()),
            proptest::option::of(arb_digest()),
            arb_default(),
            checks,
            any::<bool>(),
        )
            .prop_map(
                |(apex_target, apex_counter, apex_ok, booted, default, checks, session)| {
                    AssessInput {
                        vars: BootVars {
                            apex_target,
                            apex_counter,
                            apex_ok,
                        },
                        booted_digest: booted,
                        default_deployment: default,
                        checks,
                        user_session_active: session,
                    }
                },
            )
    }

    /// What a green boot writes for these variables.
    fn expected_green_ops(vars: &BootVars) -> Vec<EnvOp> {
        let mut ops = Vec::new();
        if vars.apex_target.is_some() {
            ops.push(EnvOp::Set(BootVar::ApexOk, "1".into()));
            ops.push(EnvOp::Unset(BootVar::ApexTarget));
        }
        if vars.apex_counter.is_some() {
            ops.push(EnvOp::Unset(BootVar::ApexCounter));
        }
        ops
    }

    proptest! {
        #[test]
        fn never_reboots_with_a_user_session(input in arb_input()) {
            let decision = decide(&input);
            if input.user_session_active {
                prop_assert!(!decision.reboot());
                prop_assert!(!decision.steps().contains(&Step::Reboot));
            }
        }

        #[test]
        fn unarmed_never_rolls_back_or_reboots(input in arb_input()) {
            let decision = decide(&input);
            if input.vars.apex_target.is_none() {
                prop_assert!(
                    matches!(decision, Decision::Green { .. } | Decision::RedUnarmed { .. }),
                    "{:?}", decision
                );
                prop_assert_ne!(decision.verdict(), Verdict::Fallback);
                prop_assert_ne!(decision.verdict(), Verdict::Red);
                prop_assert!(!decision.reboot());
                // Nothing but a stray counter is ever touched.
                for op in decision.ops() {
                    prop_assert_eq!(op, EnvOp::Unset(BootVar::ApexCounter));
                }
            }
        }

        #[test]
        fn green_confirms_and_clears_only_what_is_there(input in arb_input()) {
            if let Decision::Green { ops, .. } = decide(&input) {
                prop_assert_eq!(ops, expected_green_ops(&input.vars));
            }
        }

        #[test]
        fn a_green_verdict_that_clears_confirms_first(input in arb_input()) {
            let decision = decide(&input);
            if decision.verdict() == Verdict::Green
                && matches!(decision, Decision::Green { .. } | Decision::Inconsistent { .. })
            {
                prop_assert_eq!(decision.ops(), expected_green_ops(&input.vars));
            }
        }

        #[test]
        fn steps_are_consistent(input in arb_input()) {
            let decision = decide(&input);
            let steps = decision.steps();
            let fallback = matches!(decision, Decision::Fallback { .. });
            // Exactly one report, with the decision's verdict and digest.
            let reports: Vec<_> = steps.iter().filter(|s| matches!(s, Step::Report { .. })).collect();
            prop_assert_eq!(reports.len(), 1);
            prop_assert_eq!(
                reports[0],
                &Step::Report {
                    verdict: decision.verdict(),
                    digest: decision.report_digest().cloned(),
                    must_succeed: fallback,
                }
            );
            // A reboot is only ever the last step.
            if let Some(i) = steps.iter().position(|s| *s == Step::Reboot) {
                prop_assert_eq!(i, steps.len() - 1);
            }
            // The operations in the steps are the decision's operations, and no step
            // writes nothing.
            let env_ops: Vec<EnvOp> = steps.iter().filter_map(|s| match s {
                Step::ApplyEnv(ops) => Some(ops.clone()),
                _ => None,
            }).flatten().collect();
            prop_assert_eq!(env_ops, decision.ops());
            prop_assert!(!steps.contains(&Step::ApplyEnv(Vec::new())));
            // Only a fallback reconciles, last, and only after its required report.
            let reconcile = steps.iter().position(|s| matches!(s, Step::ReconcileFallback { .. }));
            prop_assert_eq!(reconcile.is_some(), fallback);
            if let Some(i) = reconcile {
                prop_assert_eq!(i, steps.len() - 1);
                prop_assert!(steps[..i].iter().any(|s| steps[i].only_after_failure_of(s)));
            }
            // A fallback writes the hedge before it asks for the rollback.
            if fallback {
                prop_assert_eq!(&steps[0], &Step::ApplyEnv(hedge_expected()));
            }
            // A health failure is recorded exactly for red verdicts, for the target.
            let recorded = steps.iter().any(|s| matches!(s, Step::RecordHealthFailure { .. }));
            prop_assert_eq!(recorded, decision.verdict() == Verdict::Red);
            // The verdict string is one the D-Bus contract knows.
            prop_assert!(["green", "red", "red-unarmed", "fallback"].contains(&decision.verdict().as_str()));
        }

        #[test]
        fn userspace_never_arms(input in arb_input()) {
            let decision = decide(&input);
            for op in decision.ops() {
                // apex_target is only ever removed; apex_counter is set only to the hedge,
                // by the rows that must keep GRUB off an entry 1 that may be the target.
                prop_assert!(!matches!(op, EnvOp::Set(BootVar::ApexTarget, _)));
                if let EnvOp::Set(BootVar::ApexCounter, value) = &op {
                    prop_assert_eq!(value.as_str(), HEDGE_COUNTER.to_string());
                    prop_assert!(
                        matches!(decision, Decision::Fallback { .. } | Decision::TargetSkipped { .. }),
                        "{:?}", decision
                    );
                }
            }
            let clears = decision.ops().contains(&EnvOp::Unset(BootVar::ApexTarget));
            if clears {
                prop_assert!(
                    matches!(
                        decision,
                        Decision::Green { .. } | Decision::Fallback { .. } | Decision::Inconsistent { .. }
                    ),
                    "{:?}", decision
                );
            }
        }

        /// Finding: never clear a counted arming while the target may still be GRUB's
        /// default entry.
        #[test]
        fn a_counted_arming_survives_while_the_target_may_be_the_default(input in arb_input()) {
            let Some(target) = input.vars.apex_target.as_ref() else { return Ok(()); };
            let counted = matches!(input.vars.counter_state(), CounterState::Counted { .. });
            let other_runs = input.booted_digest.as_ref().is_some_and(|b| b != target);
            let may_be_default = input
                .default_deployment
                .is_target(target, input.booted_digest.as_ref())
                != Some(false);
            if counted && other_runs && may_be_default {
                let decision = decide(&input);
                prop_assert!(matches!(decision, Decision::TargetSkipped { .. }), "{:?}", decision);
                prop_assert!(!decision.ops().iter().any(|op| matches!(op, EnvOp::Unset(_))));
            }
        }

        #[test]
        fn reboot_only_for_a_known_red_armed_target_whose_counter_moves(input in arb_input()) {
            let decision = decide(&input);
            if decision.reboot() {
                let target = input.vars.apex_target.as_ref();
                prop_assert!(target.is_some());
                prop_assert_eq!(input.booted_digest.as_ref(), target);
                prop_assert!(input.vars.counter_state().reboot_makes_progress());
                prop_assert!(!input.user_session_active);
                prop_assert!(!decision.failed_checks().is_empty());
                prop_assert_ne!(
                    input.default_deployment.is_target(target.unwrap(), input.booted_digest.as_ref()),
                    Some(false)
                );
            }
        }

        #[test]
        fn fallback_only_when_grub_fell_back(input in arb_input()) {
            let decision = decide(&input);
            if matches!(decision, Decision::Fallback { .. }) {
                prop_assert!(input.vars.apex_target.is_some());
                prop_assert!(input.booted_digest.is_some());
                prop_assert_ne!(input.booted_digest.as_ref(), input.vars.apex_target.as_ref());
                prop_assert!(input.vars.counter_state().is_exhausted());
            }
        }

        #[test]
        fn a_fresh_arming_is_never_cleared(input in arb_input()) {
            let fresh = input.vars.apex_target.is_some()
                && input.vars.apex_counter == Some(crate::vars::BOOT_ATTEMPTS)
                && input.booted_digest.is_some()
                && input.booted_digest != input.vars.apex_target;
            if fresh {
                prop_assert!(decide(&input).ops().is_empty());
            }
        }
    }
}
