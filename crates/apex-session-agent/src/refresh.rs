use std::collections::HashMap;

use glib::prelude::*;

/// `org.gnome.Mutter.DisplayConfig` names a monitor by connector, vendor, product and serial.
pub type MonitorSpec = (String, String, String, String);

type ModeTuple = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, glib::Variant>,
);
type MonitorTuple = (MonitorSpec, Vec<ModeTuple>, HashMap<String, glib::Variant>);
type LogicalTuple = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, glib::Variant>,
);
type StateTuple = (
    u32,
    Vec<MonitorTuple>,
    Vec<LogicalTuple>,
    HashMap<String, glib::Variant>,
);
type AppliedMonitor = (String, String, HashMap<String, glib::Variant>);
type AppliedLogical = (i32, i32, f64, u32, bool, Vec<AppliedMonitor>);

/// `ApplyMonitorsConfig`'s temporary method: the change is not written to monitors.xml.
pub const METHOD_TEMPORARY: u32 = 1;

/// Rates below this are never chosen, even when the panel offers them.
pub const MIN_RATE_HZ: f64 = 60.0;

/// Mutter reports 59.95 or 60.001 for a 60 Hz mode.
const RATE_EPSILON: f64 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub spec: MonitorSpec,
    pub builtin: bool,
    pub modes: Vec<Mode>,
}

impl Monitor {
    fn current(&self) -> Option<&Mode> {
        self.modes.iter().find(|mode| mode.current)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Logical {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub monitors: Vec<MonitorSpec>,
}

/// The reply of `GetCurrentState`, reduced to what a mode change needs.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayState {
    pub serial: u32,
    pub monitors: Vec<Monitor>,
    pub logical: Vec<Logical>,
    pub layout_mode: Option<u32>,
}

fn flag(properties: &HashMap<String, glib::Variant>, key: &str) -> bool {
    properties
        .get(key)
        .and_then(glib::Variant::get::<bool>)
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rate {
    Lowest,
    Highest,
}

impl DisplayState {
    pub fn from_variant(reply: &glib::Variant) -> Option<Self> {
        let (serial, monitors, logical, properties) = reply.get::<StateTuple>()?;
        let monitors = monitors
            .into_iter()
            .map(|(spec, modes, properties)| Monitor {
                spec,
                builtin: flag(&properties, "is-builtin"),
                modes: modes
                    .into_iter()
                    .map(|(id, width, height, refresh, _, _, properties)| Mode {
                        id,
                        width,
                        height,
                        refresh,
                        current: flag(&properties, "is-current"),
                    })
                    .collect(),
            })
            .collect();
        let logical = logical
            .into_iter()
            .map(|(x, y, scale, transform, primary, monitors, _)| Logical {
                x,
                y,
                scale,
                transform,
                primary,
                monitors,
            })
            .collect();
        let layout_mode = properties
            .get("layout-mode")
            .and_then(glib::Variant::get::<u32>);
        Some(Self {
            serial,
            monitors,
            logical,
            layout_mode,
        })
    }

    pub fn connectors(&self) -> Vec<String> {
        let mut connectors: Vec<String> = self.monitors.iter().map(|m| m.spec.0.clone()).collect();
        connectors.sort();
        connectors
    }

    fn builtin(&self) -> Option<&Monitor> {
        self.monitors.iter().find(|monitor| monitor.builtin)
    }

    pub fn builtin_mode(&self) -> Option<&str> {
        Some(self.builtin()?.current()?.id.as_str())
    }

    /// The built-in panel's mode at its current resolution with the lowest rate of 60 Hz or
    /// more, or the highest rate; `None` when it is already current or there is no choice.
    pub fn target(&self, rate: Rate) -> Option<&Mode> {
        let panel = self.builtin()?;
        let current = panel.current()?;
        let candidates: Vec<&Mode> = panel
            .modes
            .iter()
            .filter(|m| m.width == current.width && m.height == current.height)
            .filter(|m| m.refresh >= MIN_RATE_HZ - RATE_EPSILON)
            .collect();
        let distinct = |a: &&Mode, b: &&Mode| (a.refresh - b.refresh).abs() >= RATE_EPSILON;
        let lowest = candidates
            .iter()
            .min_by(|a, b| a.refresh.total_cmp(&b.refresh))?;
        let highest = candidates
            .iter()
            .max_by(|a, b| a.refresh.total_cmp(&b.refresh))?;
        if !distinct(lowest, highest) {
            return None;
        }
        let chosen = match rate {
            Rate::Lowest => lowest,
            Rate::Highest => highest,
        };
        (chosen.id != current.id).then_some(*chosen)
    }

    /// `ApplyMonitorsConfig` parameters that change only the built-in panel's mode: every
    /// logical monitor keeps its position, scale, transform and primary flag.
    pub fn apply_parameters(&self, mode_id: &str) -> Option<glib::Variant> {
        let panel = self.builtin()?;
        let mut logical: Vec<AppliedLogical> = Vec::new();
        for l in &self.logical {
            let mut monitors: Vec<AppliedMonitor> = Vec::new();
            for spec in &l.monitors {
                let monitor = self.monitors.iter().find(|m| &m.spec == spec)?;
                let mode = if monitor.spec == panel.spec {
                    mode_id.to_owned()
                } else {
                    monitor.current()?.id.clone()
                };
                monitors.push((spec.0.clone(), mode, HashMap::new()));
            }
            logical.push((l.x, l.y, l.scale, l.transform, l.primary, monitors));
        }
        let mut properties: HashMap<String, glib::Variant> = HashMap::new();
        if let Some(layout_mode) = self.layout_mode {
            properties.insert("layout-mode".to_owned(), layout_mode.to_variant());
        }
        Some((self.serial, METHOD_TEMPORARY, logical, properties).to_variant())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The power source, the setting or mutter itself changed: decide afresh.
    Transition,
    MonitorsChanged,
}

/// When to lower or restore the panel's rate, as a pure state machine. It restores only
/// what it lowered, and a mode the user picks stands until the next transition.
#[derive(Debug, Default)]
pub struct Policy {
    enabled: bool,
    on_battery: Option<bool>,
    lowered: bool,
    overridden: bool,
    applied: Option<String>,
    connectors: Option<Vec<String>>,
}

impl Policy {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_on_battery(&mut self, on_battery: bool) {
        self.on_battery = Some(on_battery);
    }

    /// The mode to apply to the built-in panel, if any.
    pub fn decide(&mut self, state: &DisplayState, event: Event) -> Option<String> {
        let connectors = state.connectors();
        let hotplug = self.connectors.as_ref() != Some(&connectors);
        self.connectors = Some(connectors);
        if event == Event::MonitorsChanged && !hotplug {
            if let Some(applied) = &self.applied
                && state.builtin_mode() != Some(applied.as_str())
            {
                self.overridden = true;
                self.applied = None;
            }
            return None;
        }
        self.overridden = false;
        let on_battery = self.on_battery?;
        let mode = if self.enabled && on_battery {
            let mode = state.target(Rate::Lowest)?.id.clone();
            self.lowered = true;
            mode
        } else {
            if !self.lowered {
                return None;
            }
            self.lowered = false;
            state.target(Rate::Highest)?.id.clone()
        };
        self.applied = Some(mode.clone());
        Some(mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(connector: &str) -> MonitorSpec {
        (
            connector.to_owned(),
            "BOE".to_owned(),
            "0x0a1b".to_owned(),
            "0".to_owned(),
        )
    }

    fn mode(id: &str, width: i32, height: i32, refresh: f64, current: bool) -> Mode {
        Mode {
            id: id.to_owned(),
            width,
            height,
            refresh,
            current,
        }
    }

    fn panel(current: &str) -> DisplayState {
        let modes = [
            ("2560x1600@90.000", 2560, 1600, 90.0),
            ("2560x1600@60.000", 2560, 1600, 60.0),
            ("2560x1600@48.000", 2560, 1600, 48.0),
            ("1920x1200@120.000", 1920, 1200, 120.0),
        ]
        .map(|(id, w, h, r)| mode(id, w, h, r, id == current));
        DisplayState {
            serial: 4,
            monitors: vec![Monitor {
                spec: spec("eDP-1"),
                builtin: true,
                modes: modes.to_vec(),
            }],
            logical: vec![Logical {
                x: 0,
                y: 0,
                scale: 1.5,
                transform: 0,
                primary: true,
                monitors: vec![spec("eDP-1")],
            }],
            layout_mode: Some(1),
        }
    }

    #[test]
    fn the_lowest_rate_of_60_or_more_at_the_current_resolution() {
        let state = panel("2560x1600@90.000");
        assert_eq!(state.target(Rate::Lowest).unwrap().id, "2560x1600@60.000");
        assert_eq!(state.target(Rate::Highest), None, "already the highest");
        let low = panel("2560x1600@60.000");
        assert_eq!(low.target(Rate::Lowest), None, "already the lowest");
        assert_eq!(low.target(Rate::Highest).unwrap().id, "2560x1600@90.000");
    }

    #[test]
    fn one_rate_or_no_builtin_panel_means_no_choice() {
        let mut state = panel("1920x1200@120.000");
        assert_eq!(state.target(Rate::Lowest), None, "one rate at 1920x1200");
        state = panel("2560x1600@90.000");
        state.monitors[0].builtin = false;
        assert_eq!(state.target(Rate::Lowest), None);
        assert_eq!(state.apply_parameters("2560x1600@60.000"), None);
    }

    #[test]
    fn the_apply_keeps_the_layout_and_is_temporary() {
        let mut state = panel("2560x1600@90.000");
        state.monitors.push(Monitor {
            spec: spec("HDMI-1"),
            builtin: false,
            modes: vec![mode("1920x1080@144.000", 1920, 1080, 144.0, true)],
        });
        state.logical.push(Logical {
            x: 1707,
            y: 0,
            scale: 1.0,
            transform: 1,
            primary: false,
            monitors: vec![spec("HDMI-1")],
        });
        let parameters = state.apply_parameters("2560x1600@60.000").unwrap();
        assert_eq!(parameters.type_().as_str(), "(uua(iiduba(ssa{sv}))a{sv})");
        let (serial, method, logical, properties) = parameters
            .get::<(
                u32,
                u32,
                Vec<AppliedLogical>,
                HashMap<String, glib::Variant>,
            )>()
            .unwrap();
        assert_eq!((serial, method), (4, METHOD_TEMPORARY));
        let monitors = |l: &AppliedLogical| -> Vec<(String, String)> {
            l.5.iter().map(|(c, m, _)| (c.clone(), m.clone())).collect()
        };
        assert_eq!(
            (
                logical[0].0,
                logical[0].1,
                logical[0].2,
                logical[0].3,
                logical[0].4
            ),
            (0, 0, 1.5, 0, true)
        );
        assert_eq!(
            monitors(&logical[0]),
            [("eDP-1".to_owned(), "2560x1600@60.000".to_owned())]
        );
        assert_eq!(
            (logical[1].0, logical[1].2, logical[1].3, logical[1].4),
            (1707, 1.0, 1, false)
        );
        assert_eq!(
            monitors(&logical[1]),
            [("HDMI-1".to_owned(), "1920x1080@144.000".to_owned())]
        );
        assert_eq!(properties["layout-mode"].get::<u32>(), Some(1));
    }

    #[test]
    fn get_current_state_parses() {
        let mode_properties: HashMap<String, glib::Variant> =
            HashMap::from([("is-current".to_owned(), true.to_variant())]);
        let monitor_properties: HashMap<String, glib::Variant> =
            HashMap::from([("is-builtin".to_owned(), true.to_variant())]);
        let state_properties: HashMap<String, glib::Variant> =
            HashMap::from([("layout-mode".to_owned(), 2u32.to_variant())]);
        let reply = (
            9u32,
            vec![(
                spec("eDP-1"),
                vec![(
                    "2560x1600@90.000".to_owned(),
                    2560i32,
                    1600i32,
                    90.0f64,
                    1.5f64,
                    vec![1.0f64, 1.5],
                    mode_properties,
                )],
                monitor_properties,
            )],
            vec![(
                0i32,
                0i32,
                1.5f64,
                0u32,
                true,
                vec![spec("eDP-1")],
                HashMap::<String, glib::Variant>::new(),
            )],
            state_properties,
        )
            .to_variant();
        let state = DisplayState::from_variant(&reply).unwrap();
        assert_eq!(state.serial, 9);
        assert!(state.monitors[0].builtin);
        assert_eq!(state.builtin_mode(), Some("2560x1600@90.000"));
        assert!((state.logical[0].scale - 1.5).abs() < 1e-9);
        assert_eq!(state.layout_mode, Some(2));
        assert_eq!(DisplayState::from_variant(&"x".to_variant()), None);
    }

    fn on_battery(policy: &mut Policy, on: bool, state: &DisplayState) -> Option<String> {
        policy.set_on_battery(on);
        policy.decide(state, Event::Transition)
    }

    #[test]
    fn battery_lowers_and_ac_restores() {
        let mut policy = Policy::new(true);
        assert_eq!(
            policy.decide(&panel("2560x1600@90.000"), Event::Transition),
            None,
            "no power source yet"
        );
        assert_eq!(
            on_battery(&mut policy, false, &panel("2560x1600@90.000")),
            None
        );
        assert_eq!(
            on_battery(&mut policy, true, &panel("2560x1600@90.000")).as_deref(),
            Some("2560x1600@60.000")
        );
        assert_eq!(
            policy.decide(&panel("2560x1600@60.000"), Event::MonitorsChanged),
            None,
            "our own change"
        );
        assert_eq!(
            on_battery(&mut policy, false, &panel("2560x1600@60.000")).as_deref(),
            Some("2560x1600@90.000")
        );
    }

    #[test]
    fn turning_the_setting_off_on_battery_restores() {
        let mut policy = Policy::new(true);
        assert!(on_battery(&mut policy, true, &panel("2560x1600@90.000")).is_some());
        policy.set_enabled(false);
        assert_eq!(
            policy
                .decide(&panel("2560x1600@60.000"), Event::Transition)
                .as_deref(),
            Some("2560x1600@90.000")
        );
        let mut off = Policy::new(false);
        assert_eq!(on_battery(&mut off, true, &panel("2560x1600@90.000")), None);
    }

    #[test]
    fn a_rate_it_did_not_lower_is_left_on_ac() {
        let mut policy = Policy::new(true);
        assert_eq!(
            on_battery(&mut policy, true, &panel("2560x1600@60.000")),
            None,
            "already right"
        );
        assert_eq!(
            on_battery(&mut policy, false, &panel("2560x1600@60.000")),
            None
        );
    }

    #[test]
    fn a_mode_the_user_picks_stands_until_the_next_transition() {
        let mut policy = Policy::new(true);
        assert!(on_battery(&mut policy, true, &panel("2560x1600@90.000")).is_some());
        assert_eq!(
            policy.decide(&panel("2560x1600@90.000"), Event::MonitorsChanged),
            None
        );
        assert_eq!(
            policy.decide(&panel("2560x1600@90.000"), Event::MonitorsChanged),
            None,
            "still the user's"
        );
        assert_eq!(
            on_battery(&mut policy, true, &panel("2560x1600@90.000")).as_deref(),
            Some("2560x1600@60.000"),
            "a new transition decides again"
        );
    }

    #[test]
    fn a_hotplug_decides_again() {
        let mut policy = Policy::new(true);
        assert!(on_battery(&mut policy, true, &panel("2560x1600@90.000")).is_some());
        assert_eq!(
            policy.decide(&panel("2560x1600@90.000"), Event::MonitorsChanged),
            None
        );
        let mut docked = panel("2560x1600@90.000");
        docked.monitors.push(Monitor {
            spec: spec("HDMI-1"),
            builtin: false,
            modes: vec![mode("1920x1080@60.000", 1920, 1080, 60.0, true)],
        });
        assert_eq!(
            policy.decide(&docked, Event::MonitorsChanged).as_deref(),
            Some("2560x1600@60.000")
        );
    }
}
