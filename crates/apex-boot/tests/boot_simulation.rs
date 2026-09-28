//! Simulates whole boot sequences: GRUB running the snippet model, the assessment deciding,
//! its steps carried out with the library's failure policy (as the binary does) against the
//! recording writer, and a fake `apex-updated` whose fallback runs `bootc rollback`
//! (swapping the default and rollback entries) exactly as the daemon does.
//!
//! Faults injected on any boot: power loss between any two actions, a grubenv write that
//! fails, `bootc status` that cannot be read, a menu entry picked by hand, and a
//! `ReportBoot` that fails with or without its effect (the daemon may finish a queued
//! fallback after the caller gave up, now or later in the boot). A target may also stop in
//! emergency mode before the assessment runs; the early-boot watchdog
//! ([`apex_boot::emergency`]) then decides from grubenv alone.
//!
//! Checked end to end: a broken update rolls back on its own; every reboot counts, the
//! kernel's panic reboots and the watchdog's included, so nothing reboots in a loop; GRUB never stops at an
//! error because an armed target lost its `fallback`; and once the daemon has rolled the
//! target back, lost replies and an unreadable boot order never bring it back. What can:
//! boots of the previous deployment that end before their assessment writes grubenv (power
//! loss, a failed write, a stop in emergency mode) or a manual pick, `HEDGE_COUNTER` of them
//! before the arming is cleared. `a_lost_fallback_reply_survives_quick_shutdowns` shows the
//! margin.

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

/// How the target deployment behaves on one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Green,
    Red,
    /// Kernel panic or initramfs failure: `panic=10` reboots before userspace.
    Panic,
    /// GRUB cannot load the kernel or initramfs (for example a signature failure). GRUB
    /// then boots its `fallback` entry, if one is set, or stops with an error.
    Unloadable,
    /// The real root stops in emergency mode (for example a failed mount): the assessment
    /// never runs, and the drop-in's watchdog decides whether to reboot.
    Emergency,
}

/// What happens to `ReportBoot` on one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportFault {
    /// The daemon does its work and replies.
    None,
    /// The daemon does its work, but the reply is lost (the caller's 30 s timeout).
    LostAfterEffect,
    /// The call times out while the queued operation waits; the daemon does its work later
    /// in this boot, after the assessment has finished.
    EffectLater,
    /// The call fails and the daemon does nothing (not reachable).
    Refused,
}

/// One boot's circumstances.
#[derive(Debug, Clone, Copy)]
struct BootConditions {
    target_health: Health,
    user_session: bool,
    /// Power loss after this many of the assessment's actions (each `grub2-editenv`
    /// call, the report, reading the boot order, the reboot).
    crash_after_actions: Option<usize>,
    /// The `grub2-editenv` call with this index (in this boot) fails.
    env_fails_at: Option<usize>,
    report_fault: ReportFault,
    /// `bootc status` cannot be read during this boot.
    status_unknown: bool,
    /// Somebody picks this menu entry by hand in GRUB's one-second menu.
    manual_pick: Option<BootEntry>,
    /// The other deployment stops in emergency mode too on this boot (a cause both share,
    /// such as a bad fstab entry or a failing disk).
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
    /// The kernel panicked and rebooted by itself.
    Panicked,
    /// The assessment rebooted the machine.
    AssessmentRebooted(Decision),
    /// The machine stays up.
    Settled(Decision),
    /// Power was lost during the assessment.
    PowerLoss,
    /// GRUB could not load the chosen entry and had no fallback: it waits at an error
    /// until someone picks an entry or power-cycles.
    StuckInGrub,
    /// The boot stopped in emergency mode and the watchdog scheduled a reboot.
    WatchdogRebooted,
    /// The boot stopped in emergency mode and the watchdog left the shell alone: it waits
    /// for somebody.
    StuckInEmergency(emergency::Stay),
}

impl BootEnd {
    /// Whether the machine rebooted without anybody doing anything.
    fn automatic_reboot(&self) -> bool {
        matches!(
            self,
            BootEnd::Panicked | BootEnd::AssessmentRebooted(_) | BootEnd::WatchdogRebooted
        )
    }
}

/// One boot's assessment in progress: grubenv, the actions so far and the injected faults.
struct Run {
    writer: RecordingWriter,
    actions: usize,
    /// Power is lost when this many actions have been performed.
    limit: usize,
    /// A fallback the daemon performs after the assessment.
    effect_later: Option<Digest>,
    conditions: BootConditions,
}

impl Run {
    /// Counts one action, or loses power instead.
    fn act(&mut self) -> Result<(), BootEnd> {
        if self.actions == self.limit {
            return Err(BootEnd::PowerLoss);
        }
        self.actions += 1;
        Ok(())
    }

    /// An `ApplyEnv` step: one action per `grub2-editenv` call.
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

/// How a step ended, as the binary records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Skipped,
}

/// The machine, GRUB's environment and `apex-updated`.
struct Machine {
    /// BLS entries in menu order: 0 is the default deployment, 1 the rollback.
    entries: [Digest; 2],
    target: Digest,
    env: GrubEnv,
    denylist: Vec<Digest>,
    /// What ran on the last boot (`None` when GRUB stopped).
    booted: Option<Digest>,
    /// Whether GRUB itself (not a manual pick) chose the target's entry on the last boot.
    grub_chose_target: bool,
    /// The daemon swapped the entries for the target's fallback.
    rolled_back: bool,
    /// A green boot of the target confirmed it: the update is the system now.
    confirmed: bool,
}

impl Machine {
    /// `previous` is booted; `apex-updated` staged and applied `target`, and the shutdown
    /// finalized it, so the target is the default entry and the counter is armed.
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

    /// `bootc status` as the running deployment sees the boot order.
    fn default_deployment(&self, booted: &Digest) -> DefaultDeployment {
        if self.entries[0] == *booted {
            DefaultDeployment::Booted
        } else {
            DefaultDeployment::Rollback(Some(self.entries[0].clone()))
        }
    }

    /// `apex-updated`'s fallback operation (engine `fallback`): deny the digest, and run
    /// `bootc rollback` only when the digest is the queued rollback deployment.
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
        // GRUB: load_env, the snippet, then the chosen menu entry.
        let choice = grub_boot(&mut self.env).unwrap();
        let (entry, fallback) = match conditions.manual_pick {
            // A manual pick replaces the default, and GRUB has no fallback for it.
            Some(picked) => (picked, None),
            None => (choice.entry, choice.fallback),
        };
        self.grub_chose_target =
            conditions.manual_pick.is_none() && self.entries[entry.index()] == self.target;
        let mut booted = self.entries[entry.index()].clone();
        if booted == self.target && conditions.target_health == Health::Unloadable {
            // GRUB tries its `fallback` entry at once, within the same boot.
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
            // The drop-in's watchdog, before the shell starts: only grubenv, as GRUB left it.
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
            checks,
            user_session_active: conditions.user_session,
        });
        let end = self.perform(&decision, &booted, conditions);
        // The update is the system once its green boot is confirmed in grubenv (GRUB then
        // stops counting it), even if clearing the arming did not follow.
        if matches!(decision, Decision::Green { .. })
            && booted == self.target
            && self.vars().apex_ok == Some(true)
        {
            self.confirmed = true;
        }
        end
    }

    /// The binary's `perform`, action by action, with the injected faults.
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
            limit: conditions.crash_after_actions.unwrap_or(usize::MAX),
            effect_later: None,
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
        // A queued fallback the caller gave up on still runs (adversarially, even if power
        // is lost right after: ostree swaps the boot entries atomically).
        if let Some(failed) = run.effect_later {
            self.daemon_fallback(&failed, booted);
        }
        end.unwrap_or_else(|| BootEnd::Settled(decision.clone()))
    }

    /// One step; `Err` ends the boot (power loss or the reboot).
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
                    ReportFault::EffectLater => run.effect_later = fallback,
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
    // The next boot is an ordinary one on the new deployment, and writes nothing.
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
    // Afterwards the previous deployment boots normally and is confirmed.
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

/// ADR 0009, Consequences: a target whose real root stops in emergency mode is never
/// judged by the assessment. The watchdog reboots each counted boot, and GRUB falls back
/// after three; the previous deployment's assessment then finishes the rollback.
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

/// When every deployment stops in emergency mode (a cause they share), the watchdog
/// reboots the three counted boots of the update and then leaves the previous deployment's
/// shell alone: a reboot there would start the same deployment again, for ever.
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
    // Once the previous deployment boots properly, its assessment completes the fallback.
    let end = machine.boot(ok_boot(Health::Emergency));
    assert!(
        matches!(end, BootEnd::Settled(Decision::Fallback { .. })),
        "{end:?}"
    );
    assert_eq!(machine.denylist, [t]);
}

/// Panics, red boots without a session and emergency stops share the three attempts.
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

/// A normal emergency shell stays usable: once the update is confirmed (or nothing was
/// ever armed), a boot that stops in emergency mode is left alone.
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
    // A machine that was never armed.
    let mut env = GrubEnv::new();
    grub_boot(&mut env).unwrap();
    assert_eq!(
        emergency::action_for(&BootVars::from_env(&env).unwrap()),
        emergency::Action::Stay(emergency::Stay::NotArmed)
    );
}

/// An `Apply` during this boot followed by `systemctl rescue`: GRUB has not counted the new
/// arming, so the watchdog leaves the rescue shell alone.
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

/// Finding: `fallback=1` makes GRUB boot the previous deployment within the same boot when
/// it cannot load the target, while the target stays the default. The arming must stay,
/// so GRUB keeps counting and falls back for good; before, the arming was cleared and the
/// next boot stopped in GRUB.
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

/// Finding: picking the previous entry by hand while the target is armed must not disarm
/// a target that is still the default; a panicking target then rolls back as usual.
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
    // Boot 3 is the last attempt; boot 4 falls back.
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

/// Finding: the fallback report fails, but the daemon ran `bootc rollback` anyway (the
/// reply was lost, or the queued operation finished after the caller gave up), or the
/// report succeeded and the disarm could not be written, or power was lost right after the
/// report. GRUB must never boot the failed target again.
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

/// After a lost reply and a rollback, the next boots of the previous deployment may end
/// before their assessment writes anything (quick shutdowns); GRUB still never goes back
/// to the target while fewer than `HEDGE_COUNTER` of them come in a row.
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

/// The report fails and the daemon does nothing: the hedge gives the target its attempts
/// again, then GRUB falls back again and the next report goes through.
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
    // Still armed: if the user just restarts, the counter keeps going.
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
        checks: vec![CheckResult::pass(names::SESSION)],
        user_session_active: true,
    });
    assert_eq!(decision.kind(), "armed-for-next-boot");
    assert!(decision.ops().is_empty());
    // The next boot counts the new arming.
    let mut env = writer.env().clone();
    grub_boot(&mut env).unwrap();
    assert_eq!(BootVars::from_env(&env).unwrap().apex_counter, Some(2));
}

/// Whether a boot's assessment may have ended before its grubenv writes (or never ran,
/// because the previous deployment stopped in emergency mode), or GRUB's choice was
/// overridden by hand.
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

    /// Whatever happens on each boot, until the update is confirmed:
    /// - the assessment never reboots a user's session;
    /// - at most BOOT_ATTEMPTS boots in a row end in an automatic reboot, the kernel's
    ///   panic reboots included (a boot that stays up, a power loss, a boot stuck in GRUB
    ///   or a manual pick is somebody's action);
    /// - GRUB never stops at an error on its own choice;
    /// - once the daemon has rolled the target back, lost replies and an unreadable boot
    ///   order alone never bring it back: GRUB never chooses it again.
    ///
    /// The checks stop at the first boot after the rollback whose assessment may not have
    /// written grubenv (power loss, a failed write, the previous deployment stopping in
    /// emergency mode) or whose entry was picked by hand: from
    /// there, `HEDGE_COUNTER` such boots can use up the hedge (the residual case in the
    /// module docs; `a_lost_fallback_reply_survives_quick_shutdowns` shows the margin).
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

    /// After any faults, a target that stays broken is rolled back, denylisted and
    /// disarmed within six fault-free boots with nobody logged in, unless it was confirmed
    /// green on one of the boots before.
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
        // A confirmed update is the system.
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

    /// A target that turns healthy on some attempt is confirmed and stays the default.
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
