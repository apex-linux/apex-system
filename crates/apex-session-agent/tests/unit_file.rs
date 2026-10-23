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

#[test]
fn the_preset_enables_exactly_the_agent() {
    let rules: Vec<&str> = PRESET
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .collect();
    assert_eq!(rules, ["enable apex-session-agent.service"]);
}
