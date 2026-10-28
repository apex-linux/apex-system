#![allow(
    clippy::expect_used,
    reason = "integration test: a failed expectation is the test failure"
)]

use apex_session_agent::paths::MARKER_ENV;
use apex_session_agent::{BINARY_PATH, USER_UNIT};

const PRESET: &str = include_str!("../data/80-apex.preset");

fn values(key: &str) -> impl Iterator<Item = &str> + '_ {
    USER_UNIT
        .lines()
        .filter_map(move |line| line.strip_prefix(key)?.strip_prefix('='))
}

#[test]
fn the_unit_runs_the_installed_binary_in_the_graphical_session() {
    assert_eq!(values("ExecStart").collect::<Vec<_>>(), [BINARY_PATH]);
    assert_eq!(
        values("WantedBy").collect::<Vec<_>>(),
        ["graphical-session.target"]
    );
    assert_eq!(
        values("PartOf").collect::<Vec<_>>(),
        ["graphical-session.target"]
    );
    assert_eq!(
        values("After").collect::<Vec<_>>(),
        ["graphical-session.target"]
    );
}

fn seconds(key: &str) -> u64 {
    let value: Vec<&str> = values(key).collect();
    let [value] = value[..] else {
        panic!("{key} is set {} times", value.len())
    };
    value.trim_end_matches('s').parse().expect("seconds")
}

#[test]
fn the_greeter_and_system_users_are_skipped() {
    assert_eq!(values("ConditionUser").collect::<Vec<_>>(), ["!@system"]);
    assert_eq!(
        values("ConditionEnvironment").collect::<Vec<_>>(),
        ["XDG_SESSION_CLASS=user"]
    );
}

/// Restart=on-failure alone would retry a start that cannot succeed every few seconds.
#[test]
fn failed_starts_reach_the_start_limit() {
    assert_eq!(values("Restart").collect::<Vec<_>>(), ["on-failure"]);
    let burst = seconds("StartLimitBurst");
    assert!(burst >= 1);
    assert!(
        burst * seconds("RestartSec") < seconds("StartLimitIntervalSec"),
        "the restarts must fall inside one start-limit interval"
    );
}

#[test]
fn the_marker_is_where_the_crash_guard_writes_it() {
    assert_eq!(
        values("Environment").collect::<Vec<_>>(),
        [format!("{MARKER_ENV}=%S/apex/shell-guard-tripped")]
    );
}

#[test]
fn the_preset_enables_exactly_the_agent() {
    let rules: Vec<&str> = PRESET
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(rules, ["enable apex-session-agent.service"]);
}
