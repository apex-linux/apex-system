//! The `--dry-run` output: the inputs, the decision and the steps it would take, as JSON.
//!
//! The field names are stable for scripts and the KVM qualification to read: `decision`
//! (the row, e.g. `red-armed-no-session`), `verdict`, `digest`, `reboot`, `failed_checks`,
//! `steps` and `input`.

use apex_boot::{CheckOutcome, CheckResult, Decision, Digest, Step};
use serde::Serialize;

use crate::assess::Gathered;

/// The whole document.
#[derive(Debug, Serialize)]
pub(crate) struct DryRun {
    /// [`Decision::kind`].
    decision: &'static str,
    /// The journal line ([`Decision`]'s `Display`).
    summary: String,
    /// The `ReportBoot` verdict.
    verdict: &'static str,
    /// The `ReportBoot` digest; `null` is sent as `""`.
    digest: Option<String>,
    /// Whether the decision reboots.
    reboot: bool,
    /// Failed required checks.
    failed_checks: Vec<String>,
    /// What would be done, in order.
    steps: Vec<StepView>,
    /// What the decision was made from.
    input: InputView,
}

/// One step.
#[derive(Debug, Serialize)]
#[serde(tag = "step", rename_all = "kebab-case")]
enum StepView {
    ApplyEnv {
        ops: Vec<String>,
    },
    RecordHealthFailure {
        digest: String,
        failed_checks: Vec<String>,
    },
    Report {
        verdict: &'static str,
        digest: Option<String>,
        must_succeed: bool,
    },
    ReconcileFallback {
        target: String,
        booted: String,
    },
    Reboot,
}

/// The decision's input plus the reported checks.
#[derive(Debug, Serialize)]
struct InputView {
    apex_target: Option<String>,
    apex_counter: Option<i32>,
    apex_ok: Option<bool>,
    booted_digest: Option<String>,
    booted_source: &'static str,
    /// Entry 0 at the next boot: `booted`, `rollback sha256:…`, or `unknown`.
    default_deployment: String,
    user_session_active: bool,
    required: Vec<CheckView>,
    reported: Vec<CheckView>,
}

/// One check result.
#[derive(Debug, Serialize)]
struct CheckView {
    name: String,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

fn digest_string(digest: Option<&Digest>) -> Option<String> {
    digest.map(|d| d.as_str().to_owned())
}

fn check_view(result: &CheckResult) -> CheckView {
    let (outcome, reason) = match &result.outcome {
        CheckOutcome::Pass => ("pass", None),
        CheckOutcome::Fail(reason) => ("fail", Some(reason.clone())),
        CheckOutcome::Timeout => ("timeout", None),
    };
    CheckView {
        name: result.name.clone(),
        outcome,
        reason,
    }
}

fn step_view(step: &Step) -> StepView {
    match step {
        Step::ApplyEnv(ops) => StepView::ApplyEnv {
            ops: ops.iter().map(ToString::to_string).collect(),
        },
        Step::RecordHealthFailure {
            digest,
            failed_checks,
        } => StepView::RecordHealthFailure {
            digest: digest.as_str().to_owned(),
            failed_checks: failed_checks.clone(),
        },
        Step::Report {
            verdict,
            digest,
            must_succeed,
        } => StepView::Report {
            verdict: verdict.as_str(),
            digest: digest_string(digest.as_ref()),
            must_succeed: *must_succeed,
        },
        Step::ReconcileFallback { target, booted } => StepView::ReconcileFallback {
            target: target.as_str().to_owned(),
            booted: booted.as_str().to_owned(),
        },
        Step::Reboot => StepView::Reboot,
    }
}

impl DryRun {
    /// The document for a decision and what it was made from.
    pub(crate) fn new(gathered: &Gathered, decision: &Decision) -> Self {
        let input = &gathered.input;
        Self {
            decision: decision.kind(),
            summary: decision.to_string(),
            verdict: decision.verdict().as_str(),
            digest: digest_string(decision.report_digest()),
            reboot: decision.reboot(),
            failed_checks: decision.failed_checks().to_vec(),
            steps: decision.steps().iter().map(step_view).collect(),
            input: InputView {
                apex_target: digest_string(input.vars.apex_target.as_ref()),
                apex_counter: input.vars.apex_counter,
                apex_ok: input.vars.apex_ok,
                booted_digest: digest_string(input.booted_digest.as_ref()),
                booted_source: gathered.booted_source.as_str(),
                default_deployment: input.default_deployment.to_string(),
                user_session_active: input.user_session_active,
                required: input.checks.iter().map(check_view).collect(),
                reported: gathered.reported.iter().map(check_view).collect(),
            },
        }
    }

    /// Pretty-printed JSON.
    pub(crate) fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assess::BootedSource;
    use apex_boot::checks::names;
    use apex_boot::{AssessInput, BootVars, DefaultDeployment, decide};

    fn digest(c: char) -> Digest {
        Digest::parse(&format!("sha256:{}", c.to_string().repeat(64))).unwrap()
    }

    #[test]
    fn red_armed_without_session() {
        let mut checks: Vec<CheckResult> = names::REQUIRED
            .iter()
            .map(|n| CheckResult::pass(*n))
            .collect();
        checks[3] = CheckResult::fail(names::FAILED_UNITS, "failed units: gdm.service");
        checks[2] = CheckResult::timeout(names::UPDATER_PING);
        let gathered = Gathered {
            input: AssessInput {
                vars: BootVars {
                    apex_target: Some(digest('a')),
                    apex_counter: Some(1),
                    apex_ok: Some(false),
                },
                booted_digest: Some(digest('a')),
                default_deployment: DefaultDeployment::Booted,
                checks,
                user_session_active: false,
            },
            reported: vec![
                CheckResult::pass(names::CHARGE_THRESHOLD),
                CheckResult::fail(names::FINGERPRINT_DEVICE, "no USB device 04f3:0c6e"),
            ],
            booted_source: BootedSource::Bootc,
        };
        let decision = decide(&gathered.input);
        let json = DryRun::new(&gathered, &decision).to_json().unwrap();
        insta::assert_snapshot!(json);
    }

    #[test]
    fn a_fallback_shows_its_reconciliation() {
        let gathered = Gathered {
            input: AssessInput {
                vars: BootVars {
                    apex_target: Some(digest('a')),
                    apex_counter: Some(-1),
                    apex_ok: Some(false),
                },
                booted_digest: Some(digest('b')),
                default_deployment: DefaultDeployment::Rollback(Some(digest('a'))),
                checks: names::REQUIRED
                    .iter()
                    .map(|n| CheckResult::pass(*n))
                    .collect(),
                user_session_active: false,
            },
            reported: vec![],
            booted_source: BootedSource::Bootc,
        };
        let decision = decide(&gathered.input);
        let json: serde_json::Value =
            serde_json::from_str(&DryRun::new(&gathered, &decision).to_json().unwrap()).unwrap();
        let steps: Vec<&str> = json["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["step"].as_str().unwrap())
            .collect();
        assert_eq!(
            steps,
            ["apply-env", "report", "apply-env", "reconcile-fallback"]
        );
        assert_eq!(
            json["steps"][0]["ops"],
            serde_json::json!(["set apex_counter=3"])
        );
        assert_eq!(
            json["input"]["default_deployment"],
            format!("rollback {}", digest('a'))
        );
    }
}
