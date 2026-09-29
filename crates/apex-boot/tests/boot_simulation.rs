//! Whole boot sequences under injected faults: GRUB's snippet model, the assessment with the
//! binary's failure policy, and a fake `apex-updated` swapping entries as `bootc rollback` does.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers panic on broken fixtures"
)]

use apex_boot::checks::names;
use apex_boot::emergency;
use apex_boot::snippet::{BootEntry, grub_boot};
use apex_boot::{
    AssessInput, BOOT_ATTEMPTS, BootVars, CheckResult, Decision, DefaultDeployment, Digest, EnvOp,
    GrubEnv, HEDGE_COUNTER, RecordingWriter, Step, Verdict, apply_ops, arm_ops, decide,
    reconcile_fallback,
};
use proptest::prelude::*;

fn digest(c: char) -> Digest {
    Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Green,
    Red,
    /// `panic=10` reboots before userspace.
    Panic,
    /// GRUB boots its `fallback` entry, if one is set, or stops with an error.
    Unloadable,
    /// The assessment never runs; the drop-in's watchdog decides.
    Emergency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportFault {
    None,
    /// The daemon does its work, but the caller's 30 s timeout loses the reply.
    LostAfterEffect,
    /// The daemon does its work later in this boot, after the assessment has finished.
    EffectLater,
    Refused,
}

#[derive(Debug, Clone, Copy)]
struct BootConditions {
    target_health: Health,
    user_session: bool,
    /// Every `grub2-editenv` call, the report, reading the boot order and the reboot is an
    /// action.
    crash_after_actions: Option<usize>,
    env_fails_at: Option<usize>,
    report_fault: ReportFault,
    status_unknown: bool,
    manual_pick: Option<BootEntry>,
    /// A cause both deployments share, such as a bad fstab entry or a failing disk.
    previous_emergency: bool,
}

impl BootConditions {
    fn plain(target_health: Health) -> Self {
        Self {
            target_health,
            user_session: false,
            crash_after_actions: None,
            env_fails_at: None,
            report_fault: ReportFault::None,
            status_unknown: false,
            manual_pick: None,
            previous_emergency: false,
        }
    }
}

#[derive(Debug)]
enum BootEnd {
    Panicked,
    AssessmentRebooted(Decision),
    Settled(Decision),
    PowerLoss,
    /// GRUB had no fallback: it waits until someone picks an entry or power-cycles.
    StuckInGrub,
    WatchdogRebooted,
    StuckInEmergency(emergency::Stay),
}

impl BootEnd {
    fn automatic_reboot(&self) -> bool {
        matches!(
            self,
            BootEnd::Panicked | BootEnd::AssessmentRebooted(_) | BootEnd::WatchdogRebooted
        )
    }
}

struct Run {
    writer: RecordingWriter,
    actions: usize,
    power_loss_after_actions: usize,
    daemon_fallback_after_assessment: Option<Digest>,
    conditions: BootConditions,
}

impl Run {
    fn act(&mut self) -> Result<(), BootEnd> {
        if self.actions == self.power_loss_after_actions {
            return Err(BootEnd::PowerLoss);
        }
        self.actions += 1;
        Ok(())
    }

    fn apply(&mut self, ops: &[EnvOp]) -> Result<Outcome, BootEnd> {
        for call in ops.chunk_by(|a, b| matches!(a, EnvOp::Set(..)) == matches!(b, EnvOp::Set(..)))
        {
            self.act()?;
            if apply_ops(&mut self.writer, call).is_err() {
                return Ok(Outcome::Failed);
            }
        }
        Ok(Outcome::Done)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Skipped,
}

struct Machine {
    /// In menu order: 0 is the default deployment, 1 the rollback.
    entries: [Digest; 2],
    target: Digest,
    env: GrubEnv,
    denylist: Vec<Digest>,
    /// `None` when GRUB stopped.
    booted: Option<Digest>,
    /// On the last boot, and not by a manual pick.
    grub_chose_target: bool,
    rolled_back: bool,
    confirmed: bool,
}

impl Machine {
    /// The shutdown after `Apply` finalized `target`, so it is the armed default entry.
    fn applied(target: Digest, previous: Digest) -> Self {
        let mut writer = RecordingWriter::new(GrubEnv::new());
        apply_ops(&mut writer, &arm_ops(&target)).unwrap();
        Self {
            entries: [target.clone(), previous],
            target,
            env: writer.env().clone(),
            denylist: Vec::new(),
            booted: None,
            grub_chose_target: false,
            rolled_back: false,
            confirmed: false,
        }
    }

    fn vars(&self) -> BootVars {
        BootVars::from_env(&self.env).unwrap()
    }

    fn default_deployment(&self, booted: &Digest) -> DefaultDeployment {
        if self.entries[0] == *booted {
            DefaultDeployment::Booted
        } else {
            DefaultDeployment::Rollback(Some(self.entries[0].clone()))
        }
    }

    /// Like the engine's `fallback`: `bootc rollback` runs only when the digest is the
    /// queued rollback deployment.
    fn daemon_fallback(&mut self, failed: &Digest, booted: &Digest) {
        assert_ne!(
            failed, booted,
            "the daemon refuses a fallback of the running one"
        );
        if !self.denylist.contains(failed) {
            self.denylist.push(failed.clone());
        }
        let rollback_queued = self.entries[0] != *booted;
        if rollback_queued && self.entries[0] == *failed {
            self.entries.swap(0, 1);
            if *failed == self.target {
                self.rolled_back = true;
            }
        }
    }

    fn boot(&mut self, conditions: BootConditions) -> BootEnd {
        let choice = grub_boot(&mut self.env).unwrap();
        let (entry, fallback) = match conditions.manual_pick {
            // GRUB has no fallback for a manual pick.
            Some(picked) => (picked, None),
            None => (choice.entry, choice.fallback),
        };
        self.grub_chose_target =
            conditions.manual_pick.is_none() && self.entries[entry.index()] == self.target;
        let mut booted = self.entries[entry.index()].clone();
        if booted == self.target && conditions.target_health == Health::Unloadable {
            // GRUB tries its `fallback` entry within the same boot.
            match fallback {
                Some(entry) if self.entries[entry.index()] != self.target => {
                    booted = self.entries[entry.index()].clone();
                }
                _ => {
                    self.booted = None;
                    return BootEnd::StuckInGrub;
                }
            }
        }
        self.booted = Some(booted.clone());

        let health = if booted == self.target {
            conditions.target_health
        } else if conditions.previous_emergency {
            Health::Emergency
        } else {
            Health::Green
        };
        if health == Health::Panic {
            return BootEnd::Panicked;
        }
        if health == Health::Emergency {
            return match emergency::action_for(&self.vars()) {
                emergency::Action::Reboot { .. } => BootEnd::WatchdogRebooted,
                emergency::Action::Stay(reason) => BootEnd::StuckInEmergency(reason),
            };
        }
        let checks: Vec<CheckResult> = names::REQUIRED
            .iter()
            .map(|name| match health {
                Health::Red if *name == names::SESSION => {
                    CheckResult::fail(*name, "gnome-shell crashed")
                }
                _ => CheckResult::pass(*name),
            })
            .collect();
        let default_deployment = if conditions.status_unknown {
            DefaultDeployment::Unknown
        } else {
            self.default_deployment(&booted)
        };
        let decision = decide(&AssessInput {
            vars: self.vars(),
            booted_digest: Some(booted.clone()),
            default_deployment,
            required_checks: checks,
            user_session_active: conditions.user_session,
        });
        let end = self.perform(&decision, &booted, conditions);
        // Once its green boot is confirmed in grubenv GRUB stops counting the update, even
        // if clearing the arming did not follow.
        if matches!(decision, Decision::Green { .. })
            && booted == self.target
            && self.vars().apex_ok == Some(true)
        {
            self.confirmed = true;
        }
        end
    }

    fn perform(
        &mut self,
        decision: &Decision,
        booted: &Digest,
        conditions: BootConditions,
    ) -> BootEnd {
        let steps = decision.steps();
        let mut outcomes: Vec<Outcome> = Vec::new();
        let mut writer = RecordingWriter::new(self.env.clone());
        if let Some(call) = conditions.env_fails_at {
            writer = writer.failing_at(call);
        }
        let mut run = Run {
            writer,
            actions: 0,
            power_loss_after_actions: conditions.crash_after_actions.unwrap_or(usize::MAX),
            daemon_fallback_after_assessment: None,
            conditions,
        };
        let mut end = None;
        for step in &steps {
            let cancelled = steps.iter().zip(&outcomes).any(|(earlier, outcome)| {
                *outcome == Outcome::Failed && step.skipped_after_failure_of(earlier)
            });
            let conditions_on: Vec<Outcome> = steps
                .iter()
                .zip(&outcomes)
                .filter(|(earlier, _)| step.only_after_failure_of(earlier))
                .map(|(_, outcome)| *outcome)
                .collect();
            let needed = conditions_on.is_empty() || conditions_on.contains(&Outcome::Failed);
            if cancelled || !needed {
                outcomes.push(Outcome::Skipped);
                continue;
            }
            match self.perform_step(step, decision, booted, &mut run) {
                Ok(outcome) => outcomes.push(outcome),
                Err(stop) => {
                    end = Some(stop);
                    break;
                }
            }
        }
        self.env = run.writer.env().clone();
        // ostree swaps the boot entries atomically, so a queued fallback the caller gave up
        // on still runs even if power is lost right after.
        if let Some(failed) = run.daemon_fallback_after_assessment {
            self.daemon_fallback(&failed, booted);
        }
        end.unwrap_or_else(|| BootEnd::Settled(decision.clone()))
    }

    /// `Err` ends the boot.
    fn perform_step(
        &mut self,
        step: &Step,
        decision: &Decision,
        booted: &Digest,
        run: &mut Run,
    ) -> Result<Outcome, BootEnd> {
        match step {
            Step::ApplyEnv(ops) => run.apply(ops),
            Step::RecordHealthFailure { .. } => Ok(Outcome::Done),
            Step::Report {
                verdict, digest, ..
            } => {
                run.act()?;
                let fallback = (*verdict == Verdict::Fallback).then(|| digest.clone().unwrap());
                match run.conditions.report_fault {
                    ReportFault::None | ReportFault::LostAfterEffect => {
                        if let Some(failed) = &fallback {
                            self.daemon_fallback(failed, booted);
                        }
                    }
                    ReportFault::EffectLater => run.daemon_fallback_after_assessment = fallback,
                    ReportFault::Refused => {}
                }
                Ok(if run.conditions.report_fault == ReportFault::None {
                    Outcome::Done
                } else {
                    Outcome::Failed
                })
            }
            Step::ReconcileFallback { target, booted } => {
                run.act()?;
                if run.conditions.status_unknown {
                    return Ok(Outcome::Failed);
                }
                let now = self.default_deployment(booted);
                run.apply(&reconcile_fallback(target, booted, &now))
            }
            Step::Reboot => {
                if run.conditions.user_session {
                    return Ok(Outcome::Skipped);
                }
                run.act()?;
                Err(BootEnd::AssessmentRebooted(decision.clone()))
            }
        }
    }
}

fn ok_boot(target_health: Health) -> BootConditions {
    BootConditions::plain(target_health)
}

#[test]
fn a_healthy_update_is_confirmed_on_its_first_boot() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p);
    let end = machine.boot(ok_boot(Health::Green));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Green { .. })),
        "{end:?}"
    );
    assert_eq!(machine.booted, Some(t.clone()));
    assert_eq!(
        machine.vars(),
        BootVars {
            apex_target: None,
            apex_counter: None,
            apex_ok: Some(true),
        }
    );
    let env = machine.env.clone();
    let end = machine.boot(ok_boot(Health::Green));
    assert!(matches!(end, BootEnd::Settled(Decision::Green { ref ops, .. }) if ops.is_empty()));
    assert_eq!(machine.booted, Some(t));
    let mut after_grub = env;
    grub_boot(&mut after_grub).unwrap();
    assert_eq!(
        machine.env, after_grub,
        "only GRUB's own save_env changed it"
    );
}

#[test]
fn a_broken_update_without_a_user_rolls_back_after_three_boots() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    let mut log = Vec::new();
    loop {
        match machine.boot(ok_boot(Health::Red)) {
            BootEnd::AssessmentRebooted(d) => log.push(format!(
                "{} on {:?}",
                d.kind(),
                machine.booted.as_ref() == Some(&t)
            )),
            BootEnd::Settled(d) => {
                log.push(format!(
                    "{} on {:?}",
                    d.kind(),
                    machine.booted.as_ref() == Some(&t)
                ));
                break;
            }
            other => unreachable!("{other:?}"),
        }
        assert!(log.len() < 10, "no progress: {log:?}");
    }
    assert_eq!(
        log,
        [
            "red-armed-no-session on true",
            "red-armed-no-session on true",
            "red-armed-no-session on true",
            "fallback on false",
        ]
    );
    assert_eq!(machine.denylist, std::slice::from_ref(&t));
    assert_eq!(machine.entries, [p.clone(), t]);
    assert!(!machine.vars().is_armed());
    assert_eq!(machine.vars().apex_counter, None);
    let end = machine.boot(ok_boot(Health::Red));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Green { .. })),
        "{end:?}"
    );
    assert_eq!(machine.booted, Some(p));
}

#[test]
fn a_panicking_update_rolls_back_without_userspace() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    for _ in 0..3 {
        assert!(matches!(
            machine.boot(ok_boot(Health::Panic)),
            BootEnd::Panicked
        ));
    }
    let end = machine.boot(ok_boot(Health::Panic));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Fallback { .. })),
        "{end:?}"
    );
    assert_eq!(machine.booted, Some(p.clone()));
    assert_eq!(machine.entries[0], p);
    assert_eq!(machine.denylist, [t]);
    assert!(!machine.vars().is_armed());
}

#[test]
fn an_update_that_stops_in_emergency_mode_rolls_back_by_itself() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    for _ in 0..BOOT_ATTEMPTS {
        let end = machine.boot(ok_boot(Health::Emergency));
        assert!(matches!(end, BootEnd::WatchdogRebooted), "{end:?}");
        assert_eq!(machine.booted, Some(t.clone()));
    }
    let end = machine.boot(ok_boot(Health::Emergency));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Fallback { .. })),
        "{end:?}"
    );
    assert_eq!(machine.booted, Some(p.clone()));
    assert_eq!(machine.entries, [p, t.clone()]);
    assert_eq!(machine.denylist, [t]);
    assert!(!machine.vars().is_armed());
}

#[test]
fn when_everything_stops_in_emergency_mode_the_reboots_stop_after_three() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    let everything_broken = BootConditions {
        previous_emergency: true,
        ..ok_boot(Health::Emergency)
    };
    let mut ends = Vec::new();
    for _ in 0..8 {
        ends.push(format!("{:?}", machine.boot(everything_broken)));
    }
    assert_eq!(
        ends,
        [
            "WatchdogRebooted",
            "WatchdogRebooted",
            "WatchdogRebooted",
            "StuckInEmergency(FellBack)",
            "StuckInEmergency(FellBack)",
            "StuckInEmergency(FellBack)",
            "StuckInEmergency(FellBack)",
            "StuckInEmergency(FellBack)",
        ]
    );
    assert_eq!(machine.booted, Some(p));
    let end = machine.boot(ok_boot(Health::Emergency));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Fallback { .. })),
        "{end:?}"
    );
    assert_eq!(machine.denylist, [t]);
}

#[test]
fn every_kind_of_early_failure_counts_against_the_same_attempts() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    assert!(matches!(
        machine.boot(ok_boot(Health::Emergency)),
        BootEnd::WatchdogRebooted
    ));
    assert!(matches!(
        machine.boot(ok_boot(Health::Panic)),
        BootEnd::Panicked
    ));
    assert!(matches!(
        machine.boot(ok_boot(Health::Red)),
        BootEnd::AssessmentRebooted(_)
    ));
    let end = machine.boot(ok_boot(Health::Emergency));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Fallback { .. })),
        "{end:?}"
    );
    assert_eq!(machine.booted, Some(p));
    assert_eq!(machine.denylist, [t]);
}

#[test]
fn an_ordinary_emergency_shell_is_left_alone() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p);
    let end = machine.boot(ok_boot(Health::Green));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Green { .. })),
        "{end:?}"
    );
    for _ in 0..3 {
        let end = machine.boot(ok_boot(Health::Emergency));
        assert!(
            matches!(end, BootEnd::StuckInEmergency(emergency::Stay::NotArmed)),
            "{end:?}"
        );
        assert_eq!(machine.booted, Some(t.clone()));
    }
    let mut env = GrubEnv::new();
    grub_boot(&mut env).unwrap();
    assert_eq!(
        emergency::action_for(&BootVars::from_env(&env).unwrap()),
        emergency::Action::Stay(emergency::Stay::NotArmed)
    );
}

#[test]
fn a_rescue_shell_after_an_apply_in_the_same_boot_is_left_alone() {
    let n = digest('c');
    let mut env = GrubEnv::new();
    grub_boot(&mut env).unwrap();
    let mut writer = RecordingWriter::new(env);
    apply_ops(&mut writer, &arm_ops(&n)).unwrap();
    assert_eq!(
        emergency::action_for(&BootVars::from_env(writer.env()).unwrap()),
        emergency::Action::Stay(emergency::Stay::NotCounted)
    );
}

#[test]
fn an_unloadable_update_is_counted_and_rolled_back() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    let mut kinds = Vec::new();
    for _ in 0..5 {
        let BootEnd::Settled(decision) = machine.boot(ok_boot(Health::Unloadable)) else {
            panic!("GRUB did not fall through to the previous deployment");
        };
        assert_eq!(machine.booted, Some(p.clone()));
        kinds.push(decision.kind());
    }
    assert_eq!(
        kinds,
        [
            "target-skipped",
            "target-skipped",
            "target-skipped",
            "fallback",
            "green"
        ]
    );
    assert_eq!(machine.entries, [p, t.clone()]);
    assert_eq!(machine.denylist, [t]);
    assert!(!machine.vars().is_armed());
}

#[test]
fn a_manual_pick_keeps_the_arming_and_the_panicking_target_rolls_back() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    assert!(matches!(
        machine.boot(ok_boot(Health::Panic)),
        BootEnd::Panicked
    ));
    let picked = machine.boot(BootConditions {
        manual_pick: Some(BootEntry::Previous),
        ..ok_boot(Health::Panic)
    });
    assert!(
        matches!(picked, BootEnd::Settled(Decision::TargetSkipped { .. })),
        "{picked:?}"
    );
    assert!(machine.vars().is_armed());
    assert!(matches!(
        machine.boot(ok_boot(Health::Panic)),
        BootEnd::Panicked
    ));
    let end = machine.boot(ok_boot(Health::Panic));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Fallback { .. })),
        "{end:?}"
    );
    assert_eq!(machine.entries, [p.clone(), t]);
    for _ in 0..5 {
        machine.boot(ok_boot(Health::Panic));
        assert_eq!(machine.booted, Some(p.clone()));
    }
}

#[test]
fn a_lost_fallback_reply_never_boots_the_target_again() {
    let faults = [
        BootConditions {
            report_fault: ReportFault::LostAfterEffect,
            ..ok_boot(Health::Panic)
        },
        BootConditions {
            report_fault: ReportFault::EffectLater,
            ..ok_boot(Health::Panic)
        },
        BootConditions {
            report_fault: ReportFault::LostAfterEffect,
            status_unknown: true,
            ..ok_boot(Health::Panic)
        },
        BootConditions {
            // The hedge is call 0; the disarm after the successful report is call 1.
            env_fails_at: Some(1),
            ..ok_boot(Health::Panic)
        },
        BootConditions {
            // The hedge, the report, then power is lost before the disarm.
            crash_after_actions: Some(2),
            ..ok_boot(Health::Panic)
        },
    ];
    for fault in faults {
        let (t, p) = (digest('a'), digest('b'));
        let mut machine = Machine::applied(t.clone(), p.clone());
        for _ in 0..3 {
            assert!(matches!(
                machine.boot(ok_boot(Health::Panic)),
                BootEnd::Panicked
            ));
        }
        machine.boot(fault);
        assert!(machine.rolled_back, "{fault:?}");
        assert_eq!(machine.entries, [p.clone(), t.clone()]);
        for boot in 0..6 {
            let end = machine.boot(BootConditions {
                status_unknown: boot < 2 && fault.status_unknown,
                ..ok_boot(Health::Panic)
            });
            assert!(
                !machine.grub_chose_target,
                "{fault:?}: boot {boot}: {end:?}"
            );
            assert_eq!(machine.booted, Some(p.clone()));
        }
        assert!(!machine.vars().is_armed(), "{fault:?}");
    }
}

#[test]
fn a_lost_fallback_reply_survives_quick_shutdowns() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    for _ in 0..3 {
        machine.boot(ok_boot(Health::Panic));
    }
    machine.boot(BootConditions {
        report_fault: ReportFault::EffectLater,
        ..ok_boot(Health::Panic)
    });
    assert!(machine.rolled_back);
    let quick = usize::try_from(HEDGE_COUNTER).unwrap() - 1;
    for _ in 0..quick {
        assert!(matches!(
            machine.boot(BootConditions {
                crash_after_actions: Some(0),
                ..ok_boot(Health::Panic)
            }),
            BootEnd::PowerLoss
        ));
        assert!(!machine.grub_chose_target);
    }
    machine.boot(ok_boot(Health::Panic));
    assert!(!machine.grub_chose_target);
    assert_eq!(machine.booted, Some(p));
    assert!(!machine.vars().is_armed());
}

#[test]
fn a_refused_fallback_report_gives_the_target_its_attempts_again() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t.clone(), p.clone());
    for _ in 0..3 {
        machine.boot(ok_boot(Health::Panic));
    }
    let end = machine.boot(BootConditions {
        report_fault: ReportFault::Refused,
        ..ok_boot(Health::Panic)
    });
    assert!(matches!(end, BootEnd::Settled(Decision::Fallback { .. })));
    assert_eq!(machine.vars().apex_counter, Some(HEDGE_COUNTER));
    for _ in 0..HEDGE_COUNTER {
        assert!(matches!(
            machine.boot(ok_boot(Health::Panic)),
            BootEnd::Panicked
        ));
    }
    let end = machine.boot(ok_boot(Health::Panic));
    assert!(matches!(end, BootEnd::Settled(Decision::Fallback { .. })));
    assert_eq!(machine.entries, [p, t]);
    assert!(!machine.vars().is_armed());
}

#[test]
fn a_broken_update_with_a_user_logged_in_never_reboots_by_itself() {
    let (t, p) = (digest('a'), digest('b'));
    let mut machine = Machine::applied(t, p);
    let end = machine.boot(BootConditions {
        user_session: true,
        ..ok_boot(Health::Red)
    });
    let BootEnd::Settled(decision) = end else {
        panic!("rebooted: {end:?}");
    };
    assert_eq!(decision.kind(), "red-armed-session");
    assert_eq!(decision.verdict(), Verdict::Red);
    assert!(machine.vars().is_armed());
    assert_eq!(machine.vars().apex_counter, Some(2));
}

#[test]
fn an_apply_during_the_assessment_survives_it() {
    let (p, n) = (digest('b'), digest('c'));
    // p boots unarmed. While its assessment waits for the session, the user applies n, so
    // the assessment reads a counter GRUB has not seen yet.
    let mut env = GrubEnv::new();
    grub_boot(&mut env).unwrap();
    let mut writer = RecordingWriter::new(env);
    apply_ops(&mut writer, &arm_ops(&n)).unwrap();
    let decision = decide(&AssessInput {
        vars: BootVars::from_env(writer.env()).unwrap(),
        booted_digest: Some(p),
        default_deployment: DefaultDeployment::Booted,
        required_checks: vec![CheckResult::pass(names::SESSION)],
        user_session_active: true,
    });
    assert_eq!(decision.kind(), "armed-for-next-boot");
    assert!(decision.ops().is_empty());
    let mut env = writer.env().clone();
    grub_boot(&mut env).unwrap();
    assert_eq!(BootVars::from_env(&env).unwrap().apex_counter, Some(2));
}

/// The boot's assessment may not have written grubenv, or GRUB's choice was overridden
/// by hand.
fn faulty(conditions: &BootConditions) -> bool {
    conditions.crash_after_actions.is_some()
        || conditions.env_fails_at.is_some()
        || conditions.manual_pick.is_some()
        || conditions.previous_emergency
}

fn arb_health() -> impl Strategy<Value = Health> {
    prop_oneof![
        3 => Just(Health::Green),
        3 => Just(Health::Red),
        3 => Just(Health::Panic),
        1 => Just(Health::Unloadable),
        2 => Just(Health::Emergency),
    ]
}

fn arb_conditions() -> impl Strategy<Value = BootConditions> {
    (
        arb_health(),
        any::<bool>(),
        prop_oneof![3 => Just(None), 1 => (0usize..6).prop_map(Some)],
        prop_oneof![3 => Just(None), 1 => (0usize..3).prop_map(Some)],
        prop_oneof![
            4 => Just(ReportFault::None),
            1 => Just(ReportFault::LostAfterEffect),
            1 => Just(ReportFault::EffectLater),
            1 => Just(ReportFault::Refused),
        ],
        prop_oneof![3 => Just(false), 1 => Just(true)],
        prop_oneof![
            8 => Just(None),
            1 => Just(Some(BootEntry::Default)),
            1 => Just(Some(BootEntry::Previous)),
        ],
        prop_oneof![3 => Just(false), 1 => Just(true)],
    )
        .prop_map(
            |(
                target_health,
                user_session,
                crash_after_actions,
                env_fails_at,
                report_fault,
                status_unknown,
                manual_pick,
                previous_emergency,
            )| BootConditions {
                target_health,
                user_session,
                crash_after_actions,
                env_fails_at,
                report_fault,
                status_unknown,
                manual_pick,
                previous_emergency,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// Stops at the first `faulty` boot after the rollback: from there `HEDGE_COUNTER` such boots
    /// can use up the hedge; `a_lost_fallback_reply_survives_quick_shutdowns` shows the margin.
    #[test]
    fn nothing_loops_and_a_rolled_back_target_stays_away(
        boots in prop::collection::vec(arb_conditions(), 1..20),
    ) {
        let (t, p) = (digest('a'), digest('b'));
        let mut machine = Machine::applied(t, p);
        let mut automatic_in_a_row = 0;
        for conditions in boots {
            if machine.confirmed || (machine.rolled_back && faulty(&conditions)) {
                break;
            }
            let rolled_back_before = machine.rolled_back;
            let end = machine.boot(conditions);
            if let BootEnd::AssessmentRebooted(decision) = &end {
                prop_assert!(!conditions.user_session, "rebooted with a session: {:?}", decision);
            }
            if conditions.manual_pick.is_some() {
                automatic_in_a_row = 0;
                continue;
            }
            if end.automatic_reboot() {
                automatic_in_a_row += 1;
            } else {
                automatic_in_a_row = 0;
            }
            prop_assert!(
                automatic_in_a_row <= BOOT_ATTEMPTS,
                "{} automatic reboots in a row, last {:?}", automatic_in_a_row, end
            );
            if !machine.confirmed {
                prop_assert!(!matches!(end, BootEnd::StuckInGrub), "stuck in GRUB");
            }
            if rolled_back_before {
                prop_assert!(!machine.grub_chose_target, "GRUB chose the rolled-back target: {:?}", end);
            }
        }
    }

    #[test]
    fn a_target_that_stays_broken_is_rolled_back_after_any_faults(
        boots in prop::collection::vec(arb_conditions(), 0..10),
        health in prop_oneof![
            Just(Health::Red),
            Just(Health::Panic),
            Just(Health::Unloadable),
            Just(Health::Emergency),
        ],
    ) {
        let (t, p) = (digest('a'), digest('b'));
        let mut machine = Machine::applied(t.clone(), p.clone());
        for conditions in boots {
            if machine.rolled_back && faulty(&conditions) {
                // The residual case the first property stops at.
                return Ok(());
            }
            machine.boot(BootConditions { target_health: health, ..conditions });
        }
        if machine.confirmed {
            return Ok(());
        }
        for _ in 0..6 {
            machine.boot(ok_boot(health));
        }
        prop_assert_eq!(&machine.entries[0], &p);
        prop_assert!(machine.denylist.contains(&t));
        prop_assert!(!machine.vars().is_armed(), "{:?}", machine.vars());
    }

    #[test]
    fn a_target_that_recovers_is_kept(failures in 0usize..3) {
        let (t, p) = (digest('a'), digest('b'));
        let mut machine = Machine::applied(t.clone(), p);
        for _ in 0..failures {
            let end = machine.boot(ok_boot(Health::Red));
            prop_assert!(matches!(end, BootEnd::AssessmentRebooted(_)), "{:?}", end);
        }
        let end = machine.boot(ok_boot(Health::Green));
        prop_assert!(matches!(end, BootEnd::Settled(Decision::Green { .. })), "{:?}", end);
        prop_assert_eq!(&machine.entries[0], &t);
        prop_assert!(machine.denylist.is_empty());
        prop_assert!(!machine.vars().is_armed());
    }
}
