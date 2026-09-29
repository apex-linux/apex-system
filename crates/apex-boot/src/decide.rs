use std::fmt;

use crate::checks::{CheckResult, names};
use crate::digest::Digest;
use crate::vars::{BootVar, BootVars, CounterState, EnvOp, disarm_ops};

/// `apex-updated` may still run `bootc rollback` after a failed fallback report, so the
/// fallback first gives whatever entry 0 then is counted boots, as after `Apply`.
pub const HEDGE_COUNTER: i32 = crate::vars::BOOT_ATTEMPTS;

/// GRUB's entry 0 at the next boot. A staged deployment is not a boot entry until ostree
/// finalizes it at shutdown, so bootc's `rollbackQueued` decides.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DefaultDeployment {
    #[default]
    Unknown,
    Booted,
    Rollback(Option<Digest>),
}

impl DefaultDeployment {
    pub fn from_status(rollback_queued: bool, rollback: Option<Digest>) -> Self {
        if rollback_queued {
            Self::Rollback(rollback)
        } else {
            Self::Booted
        }
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssessInput {
    pub vars: BootVars,
    pub booted_digest: Option<Digest>,
    pub default_deployment: DefaultDeployment,
    pub required_checks: Vec<CheckResult>,
    /// Also set when logind cannot answer: a reboot must never end somebody's session.
    pub user_session_active: bool,
}

/// The `verdict` argument of `ReportBoot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    Green,
    Red,
    RedUnarmed,
    Fallback,
}

impl Verdict {
    pub const ALL: [Verdict; 4] = [
        Verdict::Green,
        Verdict::Red,
        Verdict::RedUnarmed,
        Verdict::Fallback,
    ];

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Green {
        booted: Option<Digest>,
        ops: Vec<EnvOp>,
    },
    RedArmedNoSession {
        target: Digest,
        failed_checks: Vec<String>,
        /// `apex_ok=0`, so the next boot is counted even if something confirmed this one.
        ops: Vec<EnvOp>,
        reboot: bool,
    },
    RedArmedSession {
        target: Digest,
        failed_checks: Vec<String>,
    },
    RedUnarmed {
        booted: Option<Digest>,
        failed_checks: Vec<String>,
    },
    Fallback {
        failed_target: Digest,
        booted: Digest,
        hedge: Vec<EnvOp>,
        ops: Vec<EnvOp>,
    },
    /// The arming no longer matches the boot order (a manual `bootc rollback`, a lost disarm,
    /// a stage never finalized, or no counter): clear it and report an ordinary boot.
    Inconsistent {
        target: Digest,
        booted: Digest,
        failed_checks: Vec<String>,
        ops: Vec<EnvOp>,
    },
    /// Another deployment runs while the target may still be the default (not loadable, or picked
    /// by hand). Clearing would leave a broken default without counter or `fallback`.
    TargetSkipped {
        target: Digest,
        booted: Digest,
        failed_checks: Vec<String>,
        ops: Vec<EnvOp>,
    },
    /// `Apply` armed the counter after GRUB ran: the arming belongs to the next boot.
    ArmedForNextBoot {
        target: Digest,
        booted: Digest,
        failed_checks: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    ApplyEnv(Vec<EnvOp>),
    /// So the failure is known even if the daemon cannot be reached.
    RecordHealthFailure {
        digest: Digest,
        failed_checks: Vec<String>,
    },
    /// An unknown digest is sent as `""`.
    Report {
        verdict: Verdict,
        digest: Option<Digest>,
        must_succeed: bool,
    },
    /// After a failed fallback report the daemon may still have run, or may still run,
    /// `bootc rollback`.
    ReconcileFallback {
        target: Digest,
        booted: Digest,
    },
    /// Right before it, re-check that no user session has appeared; skip it if one has or
    /// logind cannot answer.
    Reboot,
}

impl Step {
    /// Never reboot or ask for a rollback while grubenv cannot follow.
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

/// Unless the rollback certainly happened, the hedge stays. Restoring -1 would be wrong: a
/// daemon still busy with the queued fallback would swap the entries afterwards.
pub fn reconcile_fallback(target: &Digest, booted: &Digest, now: &DefaultDeployment) -> Vec<EnvOp> {
    match now.is_target(target, Some(booted)) {
        Some(false) => disarm_ops(),
        Some(true) | None => Vec::new(),
    }
}

/// The boot order comes before the checks: after a fallback, a green previous deployment must
/// still be made the default before the variables are cleared.
pub fn decide(input: &AssessInput) -> Decision {
    let vars = &input.vars;
    let mut failed_checks: Vec<String> = input
        .required_checks
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
                    // At the last attempt with an unknown order entry 1 may be the target,
                    // so neither keeping 0 nor clearing is safe.
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
            // Only a known booted = target proves that GRUB's default is the target.
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

/// Confirms first: a crash before the clear leaves `apex_ok=1` with the arming, which GRUB
/// treats as confirmed. Unarmed, `apex_ok` means nothing to GRUB and is not written.
fn green_ops(vars: &BootVars) -> Vec<EnvOp> {
    let mut ops = Vec::new();
    if vars.is_armed() {
        ops.push(EnvOp::Set(BootVar::ApexOk, "1".to_owned()));
    }
    ops.extend(clear_ops(vars));
    ops
}

fn hedge_ops() -> Vec<EnvOp> {
    vec![EnvOp::Set(BootVar::ApexCounter, HEDGE_COUNTER.to_string())]
}

impl Decision {
    /// Includes the fallback's disarm, not what [`Step::ReconcileFallback`] may write.
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

    pub fn reboot(&self) -> bool {
        matches!(self, Decision::RedArmedNoSession { reboot: true, .. })
    }

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

    /// The fallback's report is required: `bootc rollback` must make the running deployment
    /// the default before the variables are cleared.
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
            required_checks: checks,
            user_session_active: session,
        }
    }

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
        let decision = decide(&plain(armed(None), Some(target()), green_checks(), false));
        assert_eq!(
            decision.ops(),
            [
                EnvOp::Set(BootVar::ApexOk, "1".into()),
                EnvOp::Unset(BootVar::ApexTarget)
            ]
        );
    }

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
        let stray = BootVars {
            apex_counter: Some(-1),
            ..BootVars::default()
        };
        let decision = decide(&plain(stray, Some(previous()), green_checks(), true));
        assert_eq!(decision.ops(), [EnvOp::Unset(BootVar::ApexCounter)]);
        assert!(matches!(
            decide(&plain(BootVars::default(), None, Vec::new(), false)),
            Decision::Green { .. }
        ));
    }

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
        assert!(Step::Reboot.skipped_after_failure_of(&env));
        assert!(env.skipped_after_failure_of(&env));
        assert!(report(true).skipped_after_failure_of(&env));
        assert!(reconcile.skipped_after_failure_of(&env));
        assert!(!record.skipped_after_failure_of(&env));
        assert!(!report(false).skipped_after_failure_of(&env));
        for next in [&env, &record, &report(false), &Step::Reboot] {
            assert!(next.skipped_after_failure_of(&report(true)));
            assert!(!next.only_after_failure_of(&report(true)));
        }
        assert!(!reconcile.skipped_after_failure_of(&report(true)));
        assert!(reconcile.only_after_failure_of(&report(true)));
        for earlier in [&env, &record, &report(false), &Step::Reboot] {
            assert!(!reconcile.only_after_failure_of(earlier));
        }
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
                        required_checks: checks,
                        user_session_active: session,
                    }
                },
            )
    }

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
            if let Some(i) = steps.iter().position(|s| *s == Step::Reboot) {
                prop_assert_eq!(i, steps.len() - 1);
            }
            let env_ops: Vec<EnvOp> = steps.iter().filter_map(|s| match s {
                Step::ApplyEnv(ops) => Some(ops.clone()),
                _ => None,
            }).flatten().collect();
            prop_assert_eq!(env_ops, decision.ops());
            prop_assert!(!steps.contains(&Step::ApplyEnv(Vec::new())));
            let reconcile = steps.iter().position(|s| matches!(s, Step::ReconcileFallback { .. }));
            prop_assert_eq!(reconcile.is_some(), fallback);
            if let Some(i) = reconcile {
                prop_assert_eq!(i, steps.len() - 1);
                prop_assert!(steps[..i].iter().any(|s| steps[i].only_after_failure_of(s)));
            }
            if fallback {
                prop_assert_eq!(&steps[0], &Step::ApplyEnv(hedge_expected()));
            }
            let recorded = steps.iter().any(|s| matches!(s, Step::RecordHealthFailure { .. }));
            prop_assert_eq!(recorded, decision.verdict() == Verdict::Red);
            prop_assert!(["green", "red", "red-unarmed", "fallback"].contains(&decision.verdict().as_str()));
        }

        #[test]
        fn userspace_never_arms(input in arb_input()) {
            let decision = decide(&input);
            for op in decision.ops() {
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
