use std::fs;
use std::sync::Arc;
use std::time::Duration;

use apex_boot::checks::names;
use apex_boot::writer::EditenvCall;
use apex_boot::{
    BootVars, CheckOutcome, Decision, DefaultDeployment, FnCheck, GrubEnv, RecordingWriter,
    Verdict, decide,
};
use proptest::prelude::*;

use super::*;
use crate::fakes::{
    FakeClock, FakeHost, FakeRebooter, FakeSessions, FakeStore, FakeUpdater, UNIX_START,
    booted_image, session,
};
use crate::ports::SessionInfo;

fn digest(c: char) -> Digest {
    Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
}

fn target() -> Digest {
    digest('a')
}

fn previous() -> Digest {
    digest('b')
}

type Script = fn(Duration) -> Result<Vec<SessionInfo>, String>;

#[expect(clippy::unnecessary_wraps, reason = "a logind script")]
fn nobody(_: Duration) -> Result<Vec<SessionInfo>, String> {
    Ok(vec![session("c1", "greeter", "active", "seat0", 600)])
}

#[expect(clippy::unnecessary_wraps, reason = "a logind script")]
fn logged_in(_: Duration) -> Result<Vec<SessionInfo>, String> {
    Ok(vec![session("2", "user", "active", "seat0", 600)])
}

struct Scenario {
    grubenv: Vec<(&'static str, String)>,
    host: Arc<FakeHost>,
    sessions: Script,
    sessions_before_reboot: Option<Script>,
    updater: FakeUpdater,
    failed_units: Option<&'static str>,
    writer_fails_at: Option<usize>,
    store: FakeStore,
    rebooter: FakeRebooter,
    force_red_content: Option<String>,
}

struct Run {
    decision: Decision,
    gathered: Gathered,
    outcomes: Vec<StepOutcome>,
    writer: RecordingWriter,
    host: Arc<FakeHost>,
    updater: Arc<FakeUpdater>,
    store: FakeStore,
    rebooter: FakeRebooter,
    clock: Arc<FakeClock>,
}

impl Scenario {
    /// `Apply` armed the counter for [`target`]; GRUB left `counter`.
    fn armed(counter: i32) -> Self {
        Self::with_grubenv(vec![
            ("apex_target", target().as_str().to_owned()),
            ("apex_counter", counter.to_string()),
            ("apex_ok", "0".to_owned()),
        ])
    }

    fn unarmed() -> Self {
        Self::with_grubenv(vec![("apex_ok", "0".to_owned())])
    }

    fn with_grubenv(grubenv: Vec<(&'static str, String)>) -> Self {
        Self {
            grubenv,
            host: Arc::new(FakeHost::booted(&target())),
            sessions: nobody,
            sessions_before_reboot: None,
            updater: FakeUpdater::healthy(),
            failed_units: None,
            writer_fails_at: None,
            store: FakeStore::default(),
            rebooter: FakeRebooter::default(),
            force_red_content: None,
        }
    }

    fn booted(mut self, digest: &Digest) -> Self {
        self.host = Arc::new(FakeHost::booted(digest));
        self
    }

    fn booted_with_first(mut self, digest: &Digest, first: &Digest) -> Self {
        self.host = Arc::new(FakeHost::booted_with_first(digest, first));
        self
    }

    /// GRUB fell back from the target, which is still entry 0.
    fn fell_back() -> Self {
        Self::armed(-1).booted_with_first(&previous(), &target())
    }

    fn sessions(mut self, script: Script) -> Self {
        self.sessions = script;
        self
    }

    fn failing_unit(mut self) -> Self {
        self.failed_units = Some("failed units: gdm.service");
        self
    }

    fn force_red(mut self, content: &str) -> Self {
        self.force_red_content = Some(content.to_owned());
        self
    }

    fn run(self) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grubenv");
        let mut env = GrubEnv::new();
        for (name, value) in &self.grubenv {
            env.set(name, value).unwrap();
        }
        fs::write(&path, env.to_bytes()).unwrap();
        let force_red = dir.path().join("state/force-red");
        if let Some(content) = &self.force_red_content {
            write_private(&force_red, content);
        }

        let clock = Arc::new(FakeClock::new());
        let sessions = Arc::new(FakeSessions::new(Arc::clone(&clock), self.sessions));
        let updater = Arc::new(self.updater);
        let ports = Ports {
            sessions: sessions.clone(),
            updater: updater.clone(),
            host: self.host.clone(),
            clock: clock.clone(),
        };
        let failed_units = self.failed_units;
        let local = LocalChecks {
            core_units: Box::new(FnCheck::new(names::CORE_UNITS_ACTIVE, || Ok(()))),
            failed_units: Box::new(FnCheck::new(names::FAILED_UNITS, move || {
                failed_units.map_or(Ok(()), |reason| Err(reason.to_owned()))
            })),
            grub_cfg: Box::new(FnCheck::new(names::GRUB_CFG_MARKER, || Ok(()))),
            reported: vec![Box::new(FnCheck::new(names::CHARGE_THRESHOLD, || Ok(())))],
            force_red_path: force_red,
            fedora_major: Ok(45),
        };
        let gathered = gather(&ports, local, &path).unwrap();
        let decision = decide(&gathered.input);

        if let Some(script) = self.sessions_before_reboot {
            sessions.set(script);
        }
        let mut writer = RecordingWriter::new(env);
        if let Some(call) = self.writer_fails_at {
            writer = writer.failing_at(call);
        }
        let outcomes = perform(
            &decision.steps(),
            &ports,
            &mut Effects {
                writer: &mut writer,
                store: &self.store,
                rebooter: &self.rebooter,
            },
        );
        Run {
            decision,
            gathered,
            outcomes,
            writer,
            host: self.host,
            updater,
            store: self.store,
            rebooter: self.rebooter,
            clock,
        }
    }
}

impl Run {
    fn vars(&self) -> BootVars {
        BootVars::from_env(self.writer.env()).unwrap()
    }

    fn failed(&self) -> bool {
        any_failed(&self.outcomes)
    }

    fn outcome(&self, index: usize) -> &StepOutcome {
        &self.outcomes[index]
    }
}

fn write_private(path: &std::path::Path, content: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir).unwrap();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn set(pairs: &[(&str, &str)]) -> EditenvCall {
    EditenvCall::Set(
        pairs
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect(),
    )
}

fn unset(names: &[&str]) -> EditenvCall {
    EditenvCall::Unset(names.iter().map(|n| (*n).to_owned()).collect())
}

#[test]
fn checks_run_in_the_documented_order() {
    let run = Scenario::unarmed().run();
    let required: Vec<&str> = run
        .gathered
        .input
        .required_checks
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(required, names::REQUIRED);
    let reported: Vec<&str> = run
        .gathered
        .reported
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(reported, [names::CHARGE_THRESHOLD, names::IMAGE_SOURCE]);
    assert!(run.gathered.reported.iter().all(|c| c.outcome.is_pass()));
    assert_eq!(run.gathered.booted_source, BootedSource::Bootc);
    assert_eq!(run.clock.elapsed(), Duration::ZERO);
}

#[test]
fn green_confirms_the_target_and_clears_the_arming() {
    let run = Scenario::armed(1).run();
    assert!(matches!(run.decision, Decision::Green { .. }));
    assert_eq!(
        run.writer.calls(),
        [
            set(&[("apex_ok", "1")]),
            unset(&["apex_target", "apex_counter"])
        ]
    );
    assert_eq!(
        run.vars(),
        BootVars {
            apex_target: None,
            apex_counter: None,
            apex_ok: Some(true)
        }
    );
    assert_eq!(run.updater.reports(), [(Verdict::Green, Some(target()))]);
    assert!(run.store.records().is_empty());
    assert_eq!(run.rebooter.reboots(), 0);
    assert!(!run.failed());
}

#[test]
fn an_ordinary_green_boot_is_confirmed_too() {
    let run = Scenario::unarmed().booted(&previous()).run();
    assert!(matches!(run.decision, Decision::Green { .. }));
    assert_eq!(run.writer.calls(), [set(&[("apex_ok", "1")])]);
    assert_eq!(
        run.vars().apex_ok,
        Some(true),
        "07_apex-menu.cfg hides the menu"
    );
    assert_eq!(run.updater.reports(), [(Verdict::Green, Some(previous()))]);
    assert!(!run.failed());
}

#[test]
fn a_failed_report_does_not_undo_the_confirmation() {
    let mut scenario = Scenario::armed(1);
    scenario.updater = FakeUpdater::healthy().report_failing("ReportBoot timed out after 30 s");
    let run = scenario.run();
    assert!(matches!(run.decision, Decision::Green { .. }));
    assert_eq!(run.vars().apex_ok, Some(true));
    assert_eq!(run.outcome(0), &StepOutcome::Done);
    assert_eq!(
        run.outcome(1),
        &StepOutcome::Failed("ReportBoot timed out after 30 s".to_owned())
    );
    assert!(run.failed());
}

#[test]
fn red_without_a_session_reboots_so_the_counter_moves() {
    let run = Scenario::armed(1).failing_unit().run();
    assert!(matches!(
        run.decision,
        Decision::RedArmedNoSession { reboot: true, .. }
    ));
    assert_eq!(run.writer.calls(), [set(&[("apex_ok", "0")])]);
    assert_eq!(run.vars().apex_target, Some(target()));
    assert_eq!(
        run.store.records(),
        [HealthFailure::new(
            target(),
            vec![names::FAILED_UNITS.to_owned()],
            UNIX_START
        )]
    );
    assert_eq!(run.updater.reports(), [(Verdict::Red, Some(target()))]);
    assert_eq!(run.rebooter.reboots(), 1);
    assert!(run.outcomes.iter().all(|o| *o == StepOutcome::Done));
    assert!(!run.failed());
}

#[test]
fn red_without_a_session_does_not_reboot_into_a_new_login() {
    let mut scenario = Scenario::armed(1).failing_unit();
    scenario.sessions_before_reboot = Some(logged_in);
    let run = scenario.run();
    assert!(run.decision.reboot());
    assert_eq!(run.rebooter.reboots(), 0);
    assert_eq!(
        run.outcomes.last(),
        Some(&StepOutcome::Skipped(Skip::UserLoggedIn))
    );
    assert_eq!(run.store.records().len(), 1);
    assert_eq!(run.updater.reports(), [(Verdict::Red, Some(target()))]);
    assert!(!run.failed());
}

#[test]
fn red_without_a_session_and_an_unreachable_daemon_still_reboots() {
    let mut scenario = Scenario::armed(2);
    scenario.updater = FakeUpdater::unreachable();
    let run = scenario.run();
    let Decision::RedArmedNoSession {
        failed_checks,
        reboot,
        ..
    } = &run.decision
    else {
        panic!("{:?}", run.decision);
    };
    assert_eq!(failed_checks, &[names::UPDATER_PING.to_owned()]);
    assert!(reboot);
    assert_eq!(run.store.records().len(), 1);
    assert!(matches!(run.outcome(2), StepOutcome::Failed(err) if err.contains("system bus")));
    assert_eq!(run.rebooter.reboots(), 1);
    assert_eq!(run.outcome(3), &StepOutcome::Done);
    assert!(run.failed());
}

#[test]
fn red_with_a_session_records_and_reports_without_rebooting() {
    let run = Scenario::armed(1).failing_unit().sessions(logged_in).run();
    assert!(matches!(run.decision, Decision::RedArmedSession { .. }));
    assert!(run.writer.calls().is_empty());
    assert_eq!(run.store.records().len(), 1);
    assert_eq!(run.updater.reports(), [(Verdict::Red, Some(target()))]);
    assert_eq!(run.rebooter.reboots(), 0);
    assert!(!run.failed());
}

#[test]
fn red_with_a_session_and_an_unreachable_daemon_is_not_lost() {
    let mut scenario = Scenario::armed(1).sessions(logged_in);
    scenario.updater = FakeUpdater::unreachable();
    let run = scenario.run();
    assert!(matches!(run.decision, Decision::RedArmedSession { .. }));
    assert_eq!(
        run.decision.failed_checks(),
        [names::UPDATER_PING.to_owned()]
    );
    let records = run.store.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].digest, target());
    assert_eq!(records[0].failed_checks, [names::UPDATER_PING.to_owned()]);
    assert_eq!(run.outcome(0), &StepOutcome::Done);
    assert!(matches!(run.outcome(1), StepOutcome::Failed(_)));
    assert_eq!(run.updater.reports().len(), 1);
    assert_eq!(run.rebooter.reboots(), 0);
    assert!(run.writer.calls().is_empty());
    assert!(run.failed());
}

#[test]
fn red_unarmed_reports_only() {
    let run = Scenario::unarmed().failing_unit().booted(&previous()).run();
    assert!(matches!(run.decision, Decision::RedUnarmed { .. }));
    assert!(run.writer.calls().is_empty());
    assert!(run.store.records().is_empty());
    assert_eq!(
        run.updater.reports(),
        [(Verdict::RedUnarmed, Some(previous()))]
    );
    assert_eq!(run.rebooter.reboots(), 0);
    assert!(!run.failed());
}

#[test]
fn fallback_hedges_reports_then_clears() {
    let run = Scenario::fell_back().run();
    assert!(matches!(run.decision, Decision::Fallback { .. }));
    assert_eq!(run.updater.reports(), [(Verdict::Fallback, Some(target()))]);
    assert_eq!(
        run.writer.calls(),
        [
            set(&[("apex_counter", "3")]),
            unset(&["apex_target", "apex_counter"])
        ]
    );
    assert_eq!(run.vars().apex_target, None);
    assert_eq!(run.vars().apex_counter, None);
    assert_eq!(run.outcome(3), &StepOutcome::Skipped(Skip::NotNeeded));
    assert_eq!(run.host.calls(), 1);
    assert!(!run.failed());
}

#[test]
fn a_fallback_reply_lost_after_the_rollback_still_disarms() {
    let mut scenario = Scenario::fell_back();
    let host = Arc::clone(&scenario.host);
    scenario.updater = FakeUpdater::healthy()
        .on_report(move |_| host.set(Ok(booted_image(&previous(), None))))
        .report_failing("ReportBoot timed out after 30 s");
    let run = scenario.run();
    assert_eq!(
        run.writer.calls(),
        [
            set(&[("apex_counter", "3")]),
            unset(&["apex_target", "apex_counter"])
        ]
    );
    assert_eq!(
        run.outcome(2),
        &StepOutcome::Skipped(Skip::AfterFailure { failed_step: 1 })
    );
    assert_eq!(run.outcome(3), &StepOutcome::Done);
    assert!(!run.vars().is_armed());
    assert_eq!(run.vars().apex_counter, None);
    assert_eq!(run.host.calls(), 2);
    assert!(run.failed());
}

#[test]
fn fallback_keeps_the_hedge_while_the_rollback_is_not_seen() {
    for updater in [
        FakeUpdater::unreachable(),
        FakeUpdater::healthy().report_failing("ReportBoot timed out after 30 s"),
    ] {
        let mut scenario = Scenario::fell_back();
        scenario.updater = updater;
        let run = scenario.run();
        assert!(matches!(run.decision, Decision::Fallback { .. }));
        assert_eq!(run.writer.calls(), [set(&[("apex_counter", "3")])]);
        assert_eq!(run.vars().apex_target, Some(target()));
        assert_eq!(run.vars().apex_counter, Some(3));
        assert!(matches!(run.outcome(1), StepOutcome::Failed(_)));
        assert_eq!(
            run.outcome(2),
            &StepOutcome::Skipped(Skip::AfterFailure { failed_step: 1 })
        );
        assert_eq!(run.outcome(3), &StepOutcome::Done);
        assert!(run.failed());
    }
    let mut scenario = Scenario::fell_back();
    let host = Arc::clone(&scenario.host);
    scenario.updater = FakeUpdater::healthy()
        .on_report(move |_| host.set(Err("bootc status timed out".to_owned())))
        .report_failing("ReportBoot timed out after 30 s");
    let run = scenario.run();
    assert_eq!(run.vars().apex_counter, Some(3));
    assert!(
        matches!(run.outcome(3), StepOutcome::Failed(err) if err.contains("bootc status timed out")),
        "{:?}",
        run.outcome(3)
    );
}

#[test]
fn a_failed_hedge_never_asks_for_the_rollback() {
    let mut scenario = Scenario::fell_back();
    scenario.writer_fails_at = Some(0);
    let run = scenario.run();
    assert!(run.updater.reports().is_empty());
    for step in 1..4 {
        assert_eq!(
            run.outcome(step),
            &StepOutcome::Skipped(Skip::AfterFailure { failed_step: 0 })
        );
    }
    assert_eq!(run.vars().apex_counter, Some(-1));
    assert!(run.failed());
}

#[test]
fn a_skipped_target_keeps_its_arming() {
    let run = Scenario::armed(1)
        .booted_with_first(&previous(), &target())
        .run();
    assert!(matches!(run.decision, Decision::TargetSkipped { .. }));
    assert!(run.writer.calls().is_empty());
    assert_eq!(run.vars().apex_target, Some(target()));
    assert_eq!(run.updater.reports(), [(Verdict::Green, Some(previous()))]);
    assert!(!run.failed());
}

#[test]
fn inconsistent_boot_clears_the_arming_and_reports_the_running_deployment() {
    let run = Scenario::armed(1).booted(&previous()).run();
    assert!(matches!(run.decision, Decision::Inconsistent { .. }));
    assert_eq!(
        run.writer.calls(),
        [
            set(&[("apex_ok", "1")]),
            unset(&["apex_target", "apex_counter"])
        ]
    );
    assert_eq!(run.updater.reports(), [(Verdict::Green, Some(previous()))]);
    assert!(!run.failed());
}

#[test]
fn a_fresh_arming_is_left_for_the_next_boot() {
    let run = Scenario::armed(3).booted(&previous()).run();
    assert!(matches!(run.decision, Decision::ArmedForNextBoot { .. }));
    assert!(run.writer.calls().is_empty());
    assert_eq!(run.updater.reports(), [(Verdict::Green, Some(previous()))]);
    assert!(!run.failed());
}

#[test]
fn the_daemon_names_the_booted_digest_when_bootc_cannot() {
    let mut scenario = Scenario::armed(1);
    scenario.host = Arc::new(FakeHost::new(Err("Operation not permitted".to_owned())));
    scenario.updater = FakeUpdater::healthy().with_booted(Ok(Some(target().as_str().to_owned())));
    let run = scenario.run();
    assert_eq!(run.gathered.booted_source, BootedSource::Daemon);
    assert!(matches!(run.decision, Decision::Green { .. }));
    let image = run.gathered.reported.last().unwrap();
    assert_eq!(
        image.outcome,
        CheckOutcome::Fail("bootc status failed: Operation not permitted".to_owned())
    );
}

#[test]
fn an_unknown_booted_digest_while_armed_is_red_without_a_reboot() {
    let mut scenario = Scenario::armed(1);
    scenario.host = Arc::new(FakeHost::new(Err("bootc status timed out".to_owned())));
    scenario.updater = FakeUpdater::healthy().with_booted(Ok(Some("sha256:bogus".to_owned())));
    let run = scenario.run();
    assert_eq!(run.gathered.booted_source, BootedSource::Unknown);
    assert_eq!(run.gathered.input.booted_digest, None);
    assert!(matches!(
        run.decision,
        Decision::RedArmedNoSession { reboot: false, .. }
    ));
    assert_eq!(
        run.decision.failed_checks(),
        [names::BOOTED_DIGEST.to_owned()]
    );
    assert_eq!(run.rebooter.reboots(), 0);
}

#[test]
fn the_hook_turns_the_armed_target_red_and_the_counter_moves() {
    let run = Scenario::armed(1)
        .force_red(&format!("{}\n", target()))
        .run();
    let last = run.gathered.input.required_checks.last().unwrap();
    assert_eq!(last.name, names::FORCED_RED);
    assert!(!last.outcome.is_pass());
    assert!(matches!(
        run.decision,
        Decision::RedArmedNoSession { reboot: true, .. }
    ));
    assert_eq!(run.decision.failed_checks(), [names::FORCED_RED.to_owned()]);
    assert_eq!(run.writer.calls(), [set(&[("apex_ok", "0")])]);
    assert_eq!(run.store.records()[0].failed_checks, [names::FORCED_RED]);
    assert_eq!(run.rebooter.reboots(), 1);
}

#[test]
fn the_hook_on_an_unarmed_boot_only_reports() {
    let run = Scenario::unarmed().force_red(target().as_str()).run();
    assert!(matches!(run.decision, Decision::RedUnarmed { .. }));
    assert_eq!(
        run.updater.reports(),
        [(Verdict::RedUnarmed, Some(target()))]
    );
    assert!(run.writer.calls().is_empty());
    assert_eq!(run.rebooter.reboots(), 0);
}

#[test]
fn the_hook_changes_nothing_for_another_digest() {
    let run = Scenario::armed(1).force_red(previous().as_str()).run();
    assert!(matches!(run.decision, Decision::Green { .. }));
    assert!(
        run.gathered
            .input
            .required_checks
            .iter()
            .all(|c| c.name != names::FORCED_RED)
    );
    let run = Scenario::armed(1).force_red("").run();
    assert!(matches!(run.decision, Decision::Green { .. }));
}

#[test]
fn the_hook_changes_nothing_when_the_booted_digest_is_unknown() {
    let mut scenario = Scenario::armed(1).force_red(target().as_str());
    scenario.host = Arc::new(FakeHost::new(Err("bootc status timed out".to_owned())));
    let run = scenario.run();
    assert_eq!(run.gathered.input.booted_digest, None);
    assert_eq!(
        run.decision.failed_checks(),
        [names::BOOTED_DIGEST.to_owned()]
    );
}

#[test]
fn a_hook_file_others_can_write_is_ignored() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/force-red");
    write_private(&path, target().as_str());
    assert!(forced_red(&path, Some(&target())).is_some());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(forced_red(&path, Some(&target())), None);
}

#[test]
fn a_failed_grubenv_write_cancels_the_reboot_but_not_the_record_or_report() {
    let mut scenario = Scenario::armed(1).failing_unit();
    scenario.writer_fails_at = Some(0);
    let run = scenario.run();
    assert!(run.decision.reboot());
    assert!(matches!(run.outcome(0), StepOutcome::Failed(_)));
    assert_eq!(run.outcome(1), &StepOutcome::Done);
    assert_eq!(run.outcome(2), &StepOutcome::Done);
    assert_eq!(
        run.outcome(3),
        &StepOutcome::Skipped(Skip::AfterFailure { failed_step: 0 })
    );
    assert_eq!(run.rebooter.reboots(), 0);
    assert!(run.failed());
}

#[test]
fn a_failed_record_does_not_stop_the_report_or_the_reboot() {
    let mut scenario = Scenario::armed(0).failing_unit();
    scenario.store = FakeStore::failing("No space left on device");
    let run = scenario.run();
    assert!(matches!(run.outcome(1), StepOutcome::Failed(_)));
    assert_eq!(run.updater.reports().len(), 1);
    assert_eq!(run.rebooter.reboots(), 1);
    assert!(run.failed());
}

#[test]
fn a_failed_reboot_is_a_failure() {
    let mut scenario = Scenario::armed(1).failing_unit();
    scenario.rebooter = FakeRebooter::failing("Access denied");
    let run = scenario.run();
    assert_eq!(
        run.outcomes.last(),
        Some(&StepOutcome::Failed("Access denied".to_owned()))
    );
    assert!(run.failed());
}

#[test]
fn the_wait_gives_a_slow_greeter_its_two_minutes() {
    // The greeter comes up after 90 s and has been active for 20 s at 110 s.
    let run = Scenario::armed(1)
        .sessions(|t| {
            Ok(if t >= Duration::from_secs(90) {
                vec![session(
                    "c1",
                    "greeter",
                    "active",
                    "seat0",
                    t.as_secs() - 90,
                )]
            } else {
                vec![]
            })
        })
        .run();
    assert!(
        matches!(run.decision, Decision::Green { .. }),
        "{:?}",
        run.decision
    );
    assert_eq!(run.clock.elapsed(), Duration::from_secs(110));
}

#[test]
fn no_session_at_all_fails_the_session_check_after_the_wait() {
    let run = Scenario::armed(1)
        .sessions(|_| Ok(vec![session("c3", "greeter", "active", "seat0", 3)]))
        .run();
    let session_check = &run.gathered.input.required_checks[0];
    assert_eq!(
        session_check.outcome,
        CheckOutcome::Fail(
            "no greeter or user session on a seat has been active for 20 s \
             (c3 greeter active on seat0 for 3 s)"
                .to_owned()
        )
    );
    // Two minutes of waiting, then the check's own 24 s.
    assert_eq!(run.clock.elapsed(), Duration::from_secs(144));
    assert!(matches!(
        run.decision,
        Decision::RedArmedNoSession { reboot: true, .. }
    ));
}

#[test]
fn logind_down_means_red_and_no_reboot() {
    let run = Scenario::armed(1)
        .sessions(|_| Err("cannot connect to the system bus".to_owned()))
        .run();
    assert!(run.gathered.input.user_session_active);
    assert!(matches!(run.decision, Decision::RedArmedSession { .. }));
    assert_eq!(run.decision.failed_checks(), [names::SESSION.to_owned()]);
    assert_eq!(run.store.records().len(), 1);
    assert_eq!(run.rebooter.reboots(), 0);
}

#[test]
fn an_unreadable_session_counts_as_a_user() {
    let run = Scenario::armed(1)
        .failing_unit()
        .sessions(|_| {
            Ok(vec![
                session("c1", "greeter", "active", "seat0", 600),
                SessionInfo {
                    age: None,
                    ..session("2", "", "", "seat0", 0)
                },
            ])
        })
        .run();
    assert!(run.gathered.input.user_session_active);
    assert_eq!(run.rebooter.reboots(), 0);
}

#[test]
fn the_reboot_is_skipped_when_logind_cannot_answer_the_recheck() {
    let mut scenario = Scenario::armed(1).failing_unit();
    scenario.sessions_before_reboot = Some(|_| Err("logind restarting".to_owned()));
    let run = scenario.run();
    assert!(run.decision.reboot());
    assert_eq!(run.rebooter.reboots(), 0);
    assert_eq!(
        run.outcomes.last(),
        Some(&StepOutcome::Skipped(Skip::SessionUnknown))
    );
    assert_eq!(run.store.records().len(), 1);
    assert_eq!(run.updater.reports(), [(Verdict::Red, Some(target()))]);
}

#[test]
fn an_uninterpretable_grubenv_stops_before_any_check() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(FakeClock::new());
    let sessions = Arc::new(FakeSessions::new(Arc::clone(&clock), nobody));
    let ports = Ports {
        sessions: sessions.clone(),
        updater: Arc::new(FakeUpdater::healthy()),
        host: Arc::new(FakeHost::booted(&target())),
        clock,
    };
    let local = || LocalChecks {
        core_units: Box::new(FnCheck::new(names::CORE_UNITS_ACTIVE, || Ok(()))),
        failed_units: Box::new(FnCheck::new(names::FAILED_UNITS, || Ok(()))),
        grub_cfg: Box::new(FnCheck::new(names::GRUB_CFG_MARKER, || Ok(()))),
        reported: Vec::new(),
        force_red_path: dir.path().join("force-red"),
        fedora_major: Ok(45),
    };

    let missing = dir.path().join("missing");
    let err = gather(&ports, local(), &missing).unwrap_err();
    assert!(matches!(err, AssessError::Grubenv(_)), "{err}");

    let corrupt = dir.path().join("corrupt");
    fs::write(&corrupt, b"not a block").unwrap();
    assert!(matches!(
        gather(&ports, local(), &corrupt).unwrap_err(),
        AssessError::Grubenv(_)
    ));

    let bad_target = dir.path().join("bad-target");
    let mut env = GrubEnv::new();
    env.set("apex_target", "latest").unwrap();
    fs::write(&bad_target, env.to_bytes()).unwrap();
    let err = gather(&ports, local(), &bad_target).unwrap_err();
    assert!(
        err.to_string().starts_with(&format!(
            "{}: apex_target is not a valid digest",
            bad_target.display()
        )),
        "{err}"
    );
    assert_eq!(sessions.calls(), 0);
}

#[test]
fn checks_finish_before_the_runner_times_them_out() {
    use crate::ping::{PING_BUDGET, PING_TIMEOUT};
    use crate::session::SESSION_CHECK_BUDGET;
    use apex_boot::checks::units::SYSTEMCTL_TIMEOUT;
    use apex_boot::{CHECK_TIMEOUT, LOGIND_TIMEOUT};

    assert!(SESSION_CHECK_BUDGET + LOGIND_TIMEOUT < CHECK_TIMEOUT);
    assert!(PING_BUDGET < CHECK_TIMEOUT && PING_TIMEOUT <= PING_BUDGET);
    assert!(SYSTEMCTL_TIMEOUT < CHECK_TIMEOUT);
}

#[test]
fn the_time_budget_uses_the_real_bootc_bound() {
    assert!(apex_bootc::STATUS_TIMEOUT + apex_bootc::KILL_GRACE <= apex_boot::BOOTC_STATUS_BOUND);
    assert_eq!(apex_boot::ASSESS_TIME_BUDGET, Duration::from_secs(305));
}

#[test]
fn steps_are_described_on_one_line() {
    let describe = |counter: i32, booted: Option<Digest>| -> Vec<String> {
        decide(&AssessInput {
            vars: BootVars {
                apex_target: Some(target()),
                apex_counter: Some(counter),
                apex_ok: Some(false),
            },
            booted_digest: booted,
            default_deployment: DefaultDeployment::Booted,
            required_checks: vec![CheckResult::fail(names::SESSION, "no session")],
            user_session_active: false,
        })
        .steps()
        .iter()
        .map(describe_step)
        .collect()
    };
    let t = target();
    assert_eq!(
        describe(1, Some(target())),
        [
            "grubenv: set apex_ok=0".to_owned(),
            format!("record health failure of {t}"),
            format!("ReportBoot(red, {t})"),
            "systemctl reboot".to_owned(),
        ]
    );
    assert_eq!(
        describe(-1, Some(previous())),
        [
            "grubenv: set apex_counter=3".to_owned(),
            format!("ReportBoot(fallback, {t})"),
            "grubenv: unset apex_target, unset apex_counter".to_owned(),
            format!("reconcile grubenv with the boot order (fallback from {t})"),
        ]
    );
}

fn arb_input() -> impl Strategy<Value = AssessInput> {
    let digest = || prop_oneof![Just(target()), Just(previous())];
    let default = prop_oneof![
        Just(DefaultDeployment::Unknown),
        Just(DefaultDeployment::Booted),
        proptest::option::of(digest()).prop_map(DefaultDeployment::Rollback),
    ];
    (
        proptest::option::of(digest()),
        prop_oneof![Just(None), (-2i32..=4).prop_map(Some)],
        proptest::option::of(digest()),
        default,
        proptest::collection::vec(any::<bool>(), names::REQUIRED.len()),
        any::<bool>(),
    )
        .prop_map(
            |(apex_target, apex_counter, booted, default, passes, session)| AssessInput {
                vars: BootVars {
                    apex_target,
                    apex_counter,
                    apex_ok: Some(false),
                },
                booted_digest: booted,
                default_deployment: default,
                required_checks: names::REQUIRED
                    .iter()
                    .zip(passes)
                    .map(|(name, pass)| {
                        if pass {
                            CheckResult::pass(*name)
                        } else {
                            CheckResult::fail(*name, "broken")
                        }
                    })
                    .collect(),
                user_session_active: session,
            },
        )
}

proptest! {
    #[test]
    fn perform_follows_the_failure_policy(
        input in arb_input(),
        writer_fails_at in proptest::option::of(0usize..3),
        store_fails in any::<bool>(),
        report_fails in any::<bool>(),
        reboot_fails in any::<bool>(),
        login_before_reboot in any::<bool>(),
        bootc_fails in any::<bool>(),
    ) {
        let decision = decide(&input);
        let steps = decision.steps();
        let clock = Arc::new(FakeClock::new());
        let script: Script = if login_before_reboot { logged_in } else { nobody };
        let mut updater = FakeUpdater::healthy();
        if report_fails {
            updater = updater.report_failing("unreachable");
        }
        let updater = Arc::new(updater);
        let host = if bootc_fails {
            FakeHost::new(Err("bootc status timed out".to_owned()))
        } else {
            FakeHost::booted(&previous())
        };
        let ports = Ports {
            sessions: Arc::new(FakeSessions::new(Arc::clone(&clock), script)),
            updater: updater.clone(),
            host: Arc::new(host),
            clock,
        };
        let mut writer = RecordingWriter::new(GrubEnv::new());
        if let Some(call) = writer_fails_at {
            writer = writer.failing_at(call);
        }
        let store = if store_fails { FakeStore::failing("full") } else { FakeStore::default() };
        let rebooter = if reboot_fails { FakeRebooter::failing("denied") } else { FakeRebooter::default() };
        let outcomes = perform(&steps, &ports, &mut Effects {
            writer: &mut writer,
            store: &store,
            rebooter: &rebooter,
        });

        prop_assert_eq!(outcomes.len(), steps.len());
        let env_failed = writer_fails_at.is_some_and(|n| n < writer.calls().len());
        if rebooter.reboots() > 0 {
            prop_assert!(decision.reboot());
            prop_assert!(!env_failed);
            prop_assert!(!login_before_reboot);
        }
        let fallback = matches!(decision, Decision::Fallback { .. });
        if fallback && writer_fails_at == Some(0) {
            prop_assert!(updater.reports().is_empty());
        }
        if fallback && report_fails && writer_fails_at != Some(0) {
            prop_assert_eq!(&outcomes[2], &StepOutcome::Skipped(Skip::AfterFailure { failed_step: 1 }));
            prop_assert!(!matches!(outcomes[3], StepOutcome::Skipped(_)));
            prop_assert!(writer.calls().len() <= 2);
        }
        for (step, outcome) in steps.iter().zip(&outcomes) {
            if matches!(step, Step::ReconcileFallback { .. }) {
                let report_failed = matches!(outcomes[1], StepOutcome::Failed(_));
                prop_assert_eq!(!matches!(outcome, StepOutcome::Skipped(_)), report_failed);
            }
        }
        if decision.reboot() && !env_failed && !login_before_reboot {
            prop_assert_eq!(rebooter.reboots(), 1);
        }
        for (step, outcome) in steps.iter().zip(&outcomes) {
            match outcome {
                StepOutcome::Skipped(Skip::AfterFailure { failed_step: i }) => {
                    prop_assert!(matches!(outcomes[*i], StepOutcome::Failed(_)));
                    prop_assert!(step.skipped_after_failure_of(&steps[*i]));
                }
                StepOutcome::Skipped(Skip::NotNeeded) => {
                    let waited_for_a_failure = steps.iter().zip(&outcomes).any(|(e, o)| {
                        step.only_after_failure_of(e) && !matches!(o, StepOutcome::Failed(_))
                    });
                    prop_assert!(waited_for_a_failure);
                }
                StepOutcome::Skipped(Skip::UserLoggedIn | Skip::SessionUnknown) => {
                    prop_assert_eq!(step, &Step::Reboot);
                }
                StepOutcome::Done | StepOutcome::Failed(_) => {}
            }
        }
        prop_assert_eq!(any_failed(&outcomes), outcomes.iter().any(|o| matches!(o, StepOutcome::Failed(_))));
        let reported = updater.reports().len();
        prop_assert!(reported <= 1);
    }
}
