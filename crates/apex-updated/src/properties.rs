//! The published properties of `io.github.sekirokenjii.Apex.Update1` and the pure rules
//! that derive them.

use std::collections::HashMap;

use apex_dbus::{Channel, Deployment, State};
use apex_update::state::is_available;
use apex_update::{AfterCheck, SystemSnapshot, Version, is_stale, plan_after_check};
use zbus::zvariant::Value;

use crate::civil::unix_from_yyyymmdd;
use crate::text::display_text;

/// Every property of the interface, as last published.
#[derive(Debug, Clone, PartialEq)]
pub struct Properties {
    /// `State`.
    pub state: State,
    /// `Progress`: `[0, 1]` while downloading with known progress, otherwise -1.
    pub progress: f64,
    /// `Booted`.
    pub booted: Deployment,
    /// `Staged`.
    pub staged: Deployment,
    /// `Rollback`.
    pub rollback: Deployment,
    /// `Available`.
    pub available: Deployment,
    /// `Channel`.
    pub channel: Channel,
    /// `LastCheck`: Unix seconds of the last successful check, 0 if never.
    pub last_check: u64,
    /// `DeferredReason`.
    pub deferred_reason: String,
    /// `LastError`.
    pub last_error: String,
    /// `Stale`.
    pub stale: bool,
    /// `HealthFailure`: the failed digest, or "".
    pub health_failure: String,
}

impl Default for Properties {
    /// What a client sees before the first refresh: state `unknown`, progress unknown.
    fn default() -> Self {
        Self {
            state: State::Unknown,
            progress: -1.0,
            booted: Deployment::default(),
            staged: Deployment::default(),
            rollback: Deployment::default(),
            available: Deployment::default(),
            channel: Channel::Stable,
            last_check: 0,
            deferred_reason: String::new(),
            last_error: String::new(),
            stale: false,
            health_failure: String::new(),
        }
    }
}

impl Properties {
    /// The D-Bus names and values of the properties that differ from `previous`, for one
    /// `PropertiesChanged` signal.
    pub fn changed_since(&self, previous: &Self) -> HashMap<&'static str, Value<'static>> {
        self.entries(Some(previous))
    }

    /// Every property with its value, for the signal a new service instance sends first.
    pub fn all(&self) -> HashMap<&'static str, Value<'static>> {
        self.entries(None)
    }

    /// The same values with every string made safe for D-Bus and terminals
    /// ([`display_text`]): the messages, and the image labels and references in the
    /// deployment dictionaries, come from bootc, the registry and the state files.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        for deployment in [
            &mut self.booted,
            &mut self.staged,
            &mut self.rollback,
            &mut self.available,
        ] {
            for text in [
                &mut deployment.image,
                &mut deployment.digest,
                &mut deployment.version,
            ]
            .into_iter()
            .flatten()
            {
                *text = display_text(text);
            }
            for text in deployment.degraded.iter_mut().flatten() {
                *text = display_text(text);
            }
        }
        for text in [
            &mut self.deferred_reason,
            &mut self.last_error,
            &mut self.health_failure,
        ] {
            *text = display_text(text);
        }
        self
    }

    /// The properties that differ from `previous`, or all of them without one.
    fn entries(&self, previous: Option<&Self>) -> HashMap<&'static str, Value<'static>> {
        let all = previous.is_none();
        let previous = previous.unwrap_or(self);
        let mut changed = HashMap::new();
        let mut put = |name: &'static str, differs: bool, value: Value<'static>| {
            if differs || all {
                changed.insert(name, value);
            }
        };
        put("State", self.state != previous.state, self.state.into());
        // Bitwise comparison, so -1.0 vs -1.0 is equal and a NaN never re-emits.
        put(
            "Progress",
            self.progress.to_bits() != previous.progress.to_bits(),
            Value::F64(self.progress),
        );
        put(
            "Booted",
            self.booted != previous.booted,
            self.booted.clone().into(),
        );
        put(
            "Staged",
            self.staged != previous.staged,
            self.staged.clone().into(),
        );
        put(
            "Rollback",
            self.rollback != previous.rollback,
            self.rollback.clone().into(),
        );
        put(
            "Available",
            self.available != previous.available,
            self.available.clone().into(),
        );
        put(
            "Channel",
            self.channel != previous.channel,
            self.channel.into(),
        );
        put(
            "LastCheck",
            self.last_check != previous.last_check,
            Value::U64(self.last_check),
        );
        put(
            "DeferredReason",
            self.deferred_reason != previous.deferred_reason,
            Value::from(self.deferred_reason.clone()),
        );
        put(
            "LastError",
            self.last_error != previous.last_error,
            Value::from(self.last_error.clone()),
        );
        put(
            "Stale",
            self.stale != previous.stale,
            Value::Bool(self.stale),
        );
        put(
            "HealthFailure",
            self.health_failure != previous.health_failure,
            Value::from(self.health_failure.clone()),
        );
        changed
    }
}

/// The contract value of an engine state (the two enums spell the same twelve values).
pub fn dbus_state(state: apex_update::State) -> State {
    use apex_update::State as S;
    match state {
        S::UpToDate => State::UpToDate,
        S::Available => State::Available,
        S::Downloading => State::Downloading,
        S::Staged => State::Staged,
        S::ReadyForReboot => State::ReadyForReboot,
        S::RollbackQueued => State::RollbackQueued,
        S::Incompatible => State::Incompatible,
        S::UsrOverlay => State::UsrOverlay,
        S::FinalizeFailed => State::FinalizeFailed,
        S::HealthFailure => State::HealthFailure,
        S::Error => State::Error,
        S::Unknown => State::Unknown,
    }
}

/// What the daemon itself is doing, which overrides the derived state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Work {
    /// Nothing that changes the published state.
    Idle,
    /// Downloading an image; `progress` is the overall fraction once known.
    Downloading {
        /// Overall progress in `[0, 1]`, if bootc reports it.
        progress: Option<f64>,
    },
}

/// The `State` property.
///
/// - `error` when the last refresh failed in a way that leaves the state unknown;
/// - `downloading` while a download runs;
/// - `unknown` before the first successful refresh;
/// - otherwise `apex_update::derive_state`.
///
/// A failed operation does not turn the state into `error`: the state stays what the
/// system is (for example `available`) and `LastError` says what went wrong.
pub fn state(snapshot: Option<&SystemSnapshot>, work: Work, refresh_failed: bool) -> State {
    if refresh_failed {
        return State::Error;
    }
    if let Work::Downloading { .. } = work {
        return State::Downloading;
    }
    snapshot.map_or(State::Unknown, |s| dbus_state(apex_update::derive_state(s)))
}

/// The `Progress` property.
pub fn progress(work: Work) -> f64 {
    match work {
        Work::Downloading {
            progress: Some(fraction),
        } if fraction.is_finite() => fraction.clamp(0.0, 1.0),
        _ => -1.0,
    }
}

/// The `DeferredReason` property: why an automatic download of the available update is
/// deferred right now, or "".
///
/// It is derived from the current snapshot with the same rule the automatic run uses
/// (`apex_update::plan_after_check`), rather than remembered from the last timer run, so it
/// survives the daemon's idle exit and follows the charger and the network as they change.
pub fn deferred_reason(snapshot: Option<&SystemSnapshot>) -> String {
    let Some(snapshot) = snapshot.filter(|s| is_available(s)) else {
        return String::new();
    };
    match plan_after_check(snapshot) {
        AfterCheck::Defer(deferral) => deferral.reason,
        AfterCheck::NothingToDo | AfterCheck::Download => String::new(),
    }
}

/// Why the staged update will not be installed although it was downloaded: `Apply`'s
/// denylist and version gates (`apex_update::validate_apply`) on the digest bootc actually
/// staged, which may not be the one the download's gates saw (the channel tag can move
/// between the check and the pull). `switch`: the stage comes from `SwitchChannel`, for
/// which `Apply` waives the version gate, so only the denylist applies. `None` when
/// nothing is staged or the stage may be installed.
pub fn stage_refusal(snapshot: &SystemSnapshot, switch: bool) -> Option<String> {
    let host = &snapshot.host;
    let staged = host.staged.as_ref()?;
    if snapshot.denylist.contains(&staged.digest) {
        let what = staged
            .version
            .as_ref()
            .map_or_else(String::new, |version| format!(" {version}"));
        return Some(format!(
            "The downloaded update{what} failed to start correctly before, so it will not be installed. Waiting for a newer update."
        ));
    }
    if switch {
        return None;
    }
    match (host.booted_version(), staged.version.as_ref()) {
        (_, None) => Some(
            "The downloaded update has no version number, so it will not be installed.".to_owned(),
        ),
        (Some(booted), Some(version)) if version <= booted => Some(format!(
            "The downloaded update {version} is not newer than the installed version {booted}, so it will not be installed."
        )),
        _ => None,
    }
}

/// The `DeferredReason` property: [`deferred_reason`], or when that is empty and a locked
/// stage will not be installed, [`stage_refusal`] for it (after a download that staged a
/// denylisted or older build than the check had found). An unlocked stage is never
/// explained this way: it is set to install.
pub fn deferred_reason_or_stage(snapshot: Option<&SystemSnapshot>, switch: bool) -> String {
    let reason = deferred_reason(snapshot);
    if !reason.is_empty() {
        return reason;
    }
    snapshot
        .filter(|s| {
            s.host
                .staged
                .as_ref()
                .is_some_and(|staged| staged.download_only)
        })
        .and_then(|s| stage_refusal(s, switch))
        .unwrap_or_default()
}

/// The newest build time known on the channel: the latest of the given image build times
/// and the dates inside the given versions.
pub fn newest_build_time(
    timestamps: impl IntoIterator<Item = Option<u64>>,
    versions: impl IntoIterator<Item = Option<Version>>,
) -> Option<u64> {
    let from_versions = versions
        .into_iter()
        .map(|v| v.and_then(|v| unix_from_yyyymmdd(v.date())));
    timestamps.into_iter().chain(from_versions).flatten().max()
}

/// The `Stale` property: a check has run, and the newest build it knows of is more than
/// seven days old (`apex_update::is_stale`). Without any check there is no evidence either
/// way, so it is `false`.
pub fn stale(last_check_at: u64, newest_build: Option<u64>, now: u64) -> bool {
    last_check_at != 0 && newest_build.is_some_and(|built| is_stale(built, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_update::{DeploymentView, HostView, Power, RemoteView};

    const DAY: u64 = 86_400;

    fn view(digest: &str, version: &str) -> DeploymentView {
        DeploymentView {
            digest: digest.into(),
            version: Version::parse(version),
            download_only: false,
            pinned: false,
        }
    }

    fn available_snapshot() -> SystemSnapshot {
        SystemSnapshot {
            power: Power {
                on_ac: true,
                battery_percent: None,
            },
            metered: Some(true),
            sysroot_free: 100 * 1024 * 1024 * 1024,
            boot_free: 1024 * 1024 * 1024,
            host: HostView {
                booted: Some(view("sha256:b", "45.20260920.1")),
                ..HostView::default()
            },
            remote: Some(RemoteView {
                digest: "sha256:r".into(),
                version: Version::parse("45.20260924.1"),
            }),
            ..SystemSnapshot::default()
        }
    }

    #[test]
    fn every_engine_state_maps_to_the_same_string() {
        for state in apex_update::State::ALL {
            assert_eq!(dbus_state(state).as_str(), state.as_str());
        }
    }

    #[test]
    fn state_precedence() {
        let s = available_snapshot();
        assert_eq!(state(Some(&s), Work::Idle, false), State::Available);
        assert_eq!(
            state(Some(&s), Work::Downloading { progress: None }, false),
            State::Downloading
        );
        assert_eq!(state(Some(&s), Work::Idle, true), State::Error);
        assert_eq!(state(None, Work::Idle, false), State::Unknown);
    }

    #[test]
    fn progress_values() {
        assert_eq!(progress(Work::Idle).to_bits(), (-1.0f64).to_bits());
        assert_eq!(
            progress(Work::Downloading { progress: None }).to_bits(),
            (-1.0f64).to_bits()
        );
        assert!(
            (progress(Work::Downloading {
                progress: Some(0.25)
            }) - 0.25)
                .abs()
                < 1e-9
        );
        assert!(
            (progress(Work::Downloading {
                progress: Some(7.0)
            }) - 1.0)
                .abs()
                < 1e-9
        );
        assert_eq!(
            progress(Work::Downloading {
                progress: Some(f64::NAN)
            })
            .to_bits(),
            (-1.0f64).to_bits()
        );
    }

    #[test]
    fn deferred_reason_follows_the_gates() {
        let mut s = available_snapshot();
        assert!(deferred_reason(Some(&s)).contains("metered"));
        s.metered = Some(false);
        assert_eq!(deferred_reason(Some(&s)), "");
        s.remote = None;
        assert_eq!(deferred_reason(Some(&s)), "");
        assert_eq!(deferred_reason(None), "");
    }

    /// A snapshot with `version` staged (locked unless `applied`) under digest `sha256:s`.
    fn staged_snapshot(version: &str, applied: bool) -> SystemSnapshot {
        let mut s = available_snapshot();
        s.metered = Some(false);
        s.host.staged = Some(DeploymentView {
            download_only: !applied,
            ..view("sha256:s", version)
        });
        s.remote = Some(RemoteView {
            digest: "sha256:s".into(),
            version: Version::parse(version),
        });
        s
    }

    #[test]
    fn a_stage_that_apply_would_refuse_is_explained() {
        let newer = staged_snapshot("45.20260924.1", false);
        assert_eq!(stage_refusal(&newer, false), None);
        assert_eq!(deferred_reason_or_stage(Some(&newer), false), "");

        let older = staged_snapshot("45.20260918.1", false);
        let reason = stage_refusal(&older, false).expect("older");
        assert_eq!(
            reason,
            "The downloaded update 45.20260918.1 is not newer than the installed version 45.20260920.1, so it will not be installed."
        );
        assert_eq!(deferred_reason_or_stage(Some(&older), false), reason);
        // Equal is not newer either.
        assert!(stage_refusal(&staged_snapshot("45.20260920.1", false), false).is_some());
        // A channel switch may be older: Apply waives the version check for it.
        assert_eq!(stage_refusal(&older, true), None);

        let mut unversioned = staged_snapshot("45.20260924.1", false);
        if let Some(staged) = &mut unversioned.host.staged {
            staged.version = None;
        }
        assert!(
            stage_refusal(&unversioned, false)
                .expect("no version")
                .contains("no version number")
        );

        // A digest that failed before is refused even for a channel switch.
        let mut denied = staged_snapshot("45.20260924.1", false);
        denied.denylist.insert(apex_update::DenyEntry {
            digest: "sha256:s".into(),
            reason: "fell back".into(),
            at: 1,
        });
        for switch in [false, true] {
            assert_eq!(
                stage_refusal(&denied, switch).as_deref(),
                Some(
                    "The downloaded update 45.20260924.1 failed to start correctly before, so it will not be installed. Waiting for a newer update."
                )
            );
        }

        // An applied stage is set to install; it is never explained as refused.
        let applied = staged_snapshot("45.20260918.1", true);
        assert!(stage_refusal(&applied, false).is_some());
        assert_eq!(deferred_reason_or_stage(Some(&applied), false), "");
        assert_eq!(deferred_reason_or_stage(None, false), "");
    }

    #[test]
    fn a_deferred_download_is_explained_before_a_refused_stage() {
        let mut s = staged_snapshot("45.20260918.1", false);
        // A newer update is available and deferred (metered): that reason comes first.
        s.metered = Some(true);
        s.remote = Some(RemoteView {
            digest: "sha256:r".into(),
            version: Version::parse("45.20260926.1"),
        });
        assert!(deferred_reason_or_stage(Some(&s), false).contains("metered"));
        s.metered = Some(false);
        assert!(deferred_reason_or_stage(Some(&s), false).contains("not newer"));
    }

    #[test]
    fn staleness() {
        let now = 1_790_208_000 + 8 * DAY;
        let newest = newest_build_time([None], [Version::parse("45.20260924.1")]);
        assert_eq!(newest, Some(1_790_208_000));
        assert!(stale(now - 60, newest, now));
        assert!(!stale(0, newest, now), "never checked");
        assert!(!stale(now, newest, 1_790_208_000 + 7 * DAY));
        let newer = newest_build_time([Some(now - DAY)], [Version::parse("45.20260924.1")]);
        assert!(!stale(now, newer, now));
        assert_eq!(newest_build_time([None], [None]), None);
    }

    fn deployment(digest: &str) -> Deployment {
        Deployment {
            digest: Some(digest.into()),
            ..Deployment::default()
        }
    }

    /// A change to one field of [`Properties`].
    type Edit = fn(&mut Properties);

    /// Every property of the contract XML, with its signature.
    fn contract_properties() -> Vec<(String, String)> {
        let node = zbus_xml::Node::try_from(apex_dbus::INTROSPECTION_XML).unwrap();
        node.interfaces()[0]
            .properties()
            .iter()
            .map(|p| (p.name().to_string(), p.ty().to_string()))
            .collect()
    }

    #[test]
    fn every_contract_property_is_signalled_alone_when_it_changes() {
        let before = Properties::default();
        // One change per property, each touching exactly one field.
        let edits: [(&str, Edit); 12] = [
            ("State", |p| p.state = State::Staged),
            ("Progress", |p| p.progress = 0.5),
            ("Booted", |p| p.booted = deployment("b")),
            ("Staged", |p| p.staged = deployment("s")),
            ("Rollback", |p| {
                p.rollback = deployment("r");
            }),
            ("Available", |p| {
                p.available = deployment("a");
            }),
            ("Channel", |p| p.channel = Channel::Testing),
            ("LastCheck", |p| p.last_check = 1),
            ("DeferredReason", |p| p.deferred_reason = "later".into()),
            ("LastError", |p| p.last_error = "failed".into()),
            ("Stale", |p| p.stale = true),
            ("HealthFailure", |p| p.health_failure = "sha256:h".into()),
        ];
        let contract = contract_properties();
        assert_eq!(contract.len(), edits.len(), "the XML has twelve properties");
        let mut all = before.clone();
        for (name, edit) in edits {
            let mut after = before.clone();
            edit(&mut after);
            let changed = after.changed_since(&before);
            assert_eq!(changed.keys().copied().collect::<Vec<_>>(), [name]);
            let (_, signature) = contract
                .iter()
                .find(|(property, _)| property == name)
                .unwrap_or_else(|| panic!("{name} is not in the XML"));
            assert_eq!(
                changed[name].value_signature().to_string(),
                *signature,
                "{name}"
            );
            assert!(after.changed_since(&after).is_empty());
            edit(&mut all);
        }
        // Everything at once: one signal naming all twelve.
        let mut names: Vec<String> = all
            .changed_since(&before)
            .into_keys()
            .map(str::to_owned)
            .collect();
        names.sort_unstable();
        let mut expected: Vec<String> = contract.into_iter().map(|(name, _)| name).collect();
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    #[test]
    fn all_names_every_property_even_at_its_default() {
        let names: Vec<&str> = {
            let mut names: Vec<&str> = Properties::default().all().into_keys().collect();
            names.sort_unstable();
            names
        };
        let mut expected: Vec<String> = contract_properties().into_iter().map(|(n, _)| n).collect();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(
            Properties::default().all()["LastError"],
            Value::from(String::new())
        );
    }

    #[test]
    fn sanitized_strips_control_characters_everywhere() {
        let dirty = "bad\0\r\u{9b}2J";
        let properties = Properties {
            last_error: dirty.into(),
            deferred_reason: dirty.into(),
            health_failure: dirty.into(),
            available: Deployment {
                image: Some(dirty.into()),
                digest: Some(dirty.into()),
                version: Some(dirty.into()),
                degraded: Some(vec![dirty.into()]),
                ..Deployment::default()
            },
            ..Properties::default()
        }
        .sanitized();
        let clean = crate::text::display_text(dirty);
        assert!(!clean.contains(['\0', '\r', '\u{9b}']), "{clean:?}");
        assert_eq!(properties.last_error, clean);
        assert_eq!(properties.deferred_reason, clean);
        assert_eq!(properties.health_failure, clean);
        assert_eq!(properties.available.image.as_deref(), Some(clean.as_str()));
        assert_eq!(properties.available.digest.as_deref(), Some(clean.as_str()));
        assert_eq!(
            properties.available.version.as_deref(),
            Some(clean.as_str())
        );
        assert_eq!(properties.available.degraded, Some(vec![clean.clone()]));
        assert_eq!(properties.clone().sanitized(), properties);
    }

    #[test]
    fn changed_properties() {
        let before = Properties::default();
        assert!(before.changed_since(&before).is_empty());
        let after = Properties {
            state: State::Downloading,
            progress: 0.5,
            last_error: "x".into(),
            ..Properties::default()
        };
        let mut names: Vec<_> = after.changed_since(&before).into_keys().collect();
        names.sort_unstable();
        assert_eq!(names, ["LastError", "Progress", "State"]);
    }
}
