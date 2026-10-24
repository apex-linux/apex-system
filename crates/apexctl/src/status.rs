use std::cmp::Ordering;
use std::fmt::Write as _;

use apex_bootc::{APEX_STABLE_REPOSITORY, APEX_TESTING_REPOSITORY};
use apex_dbus::{Change, ChangeKind, Channel, Deployment, State};
use apex_update::{Evr, Version};
use serde::Serialize;

use crate::service::ServiceStatus;
use crate::timefmt::{format_utc, format_utc_date};

pub const STATUS_JSON_SCHEMA: u32 = 2;

pub const STATUS_JSON_HELP: &str = "\
JSON output (--json), schema 2. One object; every key is always present, and a value the
update service does not report is null. New keys may be added; existing keys keep their
meaning until \"schema\" changes.

  schema           2
  state            \"up-to-date\", \"available\", \"downloading\", \"staged\", \"ready-for-reboot\",
                   \"rollback-queued\", \"incompatible\", \"usr-overlay\", \"finalize-failed\",
                   \"health-failure\", \"error\" or \"unknown\"
  progress         download progress from 0 to 1 while \"downloading\", otherwise null
  channel          \"stable\" or \"testing\": the channel of the running image
  last_check       Unix time of the last successful check, or null
  deferred_reason  why the automatic download is waiting, or null
  last_error       message of the last failed update operation, or null
  health_failure   digest of the update that failed its health checks, or null
  degraded         components shipped degraded in the running image, e.g. [\"audio\"]
  booted, staged, rollback, available
                   a deployment, or null when there is none

A deployment has these keys, each null when unknown:
  image            image reference, e.g. \"ghcr.io/apex-linux/apex:45\"
  digest           the full manifest digest, \"sha256:\" and 64 hex digits
  version          e.g. \"45.20260924.117\"
  timestamp        build time, Unix seconds
  pinned           true or false
  download_only    staged only: true while the update is locked (not used at the next boot)
  degraded         components shipped degraded in that image, e.g. [\"audio\"]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentJson {
    pub image: Option<String>,
    pub digest: Option<String>,
    pub version: Option<String>,
    pub timestamp: Option<u64>,
    pub pinned: Option<bool>,
    pub download_only: Option<bool>,
    pub degraded: Option<Vec<String>>,
}

impl DeploymentJson {
    fn from_deployment(deployment: &Deployment) -> Option<Self> {
        let d = deployment.clone().into_option()?;
        Some(Self {
            image: d.image,
            digest: d.digest,
            version: d.version,
            timestamp: d.timestamp,
            pinned: d.pinned,
            download_only: d.download_only,
            degraded: d.degraded,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusJson {
    pub schema: u32,
    pub state: State,
    pub progress: Option<f64>,
    pub channel: Channel,
    pub last_check: Option<u64>,
    pub deferred_reason: Option<String>,
    pub last_error: Option<String>,
    pub health_failure: Option<String>,
    pub degraded: Vec<String>,
    pub booted: Option<DeploymentJson>,
    pub staged: Option<DeploymentJson>,
    pub rollback: Option<DeploymentJson>,
    pub available: Option<DeploymentJson>,
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

fn known_progress(status: &ServiceStatus) -> Option<f64> {
    (status.state == State::Downloading && (0.0..=1.0).contains(&status.progress))
        .then_some(status.progress)
}

impl StatusJson {
    pub fn new(status: &ServiceStatus) -> Self {
        Self {
            schema: STATUS_JSON_SCHEMA,
            state: status.state,
            progress: known_progress(status),
            channel: status.channel,
            last_check: (status.last_check != 0).then_some(status.last_check),
            deferred_reason: non_empty(&status.deferred_reason),
            last_error: non_empty(&status.last_error),
            health_failure: non_empty(&status.health_failure),
            degraded: status.booted.degraded.clone().unwrap_or_default(),
            booted: DeploymentJson::from_deployment(&status.booted),
            staged: DeploymentJson::from_deployment(&status.staged),
            rollback: DeploymentJson::from_deployment(&status.rollback),
            available: DeploymentJson::from_deployment(&status.available),
        }
    }
}

/// podman's short digest length.
const SHORT_DIGEST_HEX_DIGITS: usize = 12;

pub fn short_digest(digest: &str) -> String {
    match digest.strip_prefix("sha256:") {
        Some(hex) if hex.len() > SHORT_DIGEST_HEX_DIGITS => {
            format!("sha256:{}", &hex[..SHORT_DIGEST_HEX_DIGITS])
        }
        _ => digest.to_owned(),
    }
}

pub fn state_sentence(state: State) -> (&'static str, Option<&'static str>) {
    match state {
        State::UpToDate => ("Apex is up to date.", None),
        State::Available => (
            "An update is available.",
            Some("Download it with: apexctl update download"),
        ),
        State::Downloading => ("An update is downloading.", None),
        State::Staged => (
            "An update is downloaded and waiting.",
            Some("Use it at the next restart with: apexctl update apply"),
        ),
        State::ReadyForReboot => ("An update is ready.", Some("Reboot to finish the update.")),
        // Neutral: the queued deployment may be older or newer than the running one.
        State::RollbackQueued => (
            "Another version will start at the next boot (rollback queued).",
            Some("Automatic updates are paused until then."),
        ),
        State::Incompatible => (
            "Updates are paused: this system has local changes that bootc cannot update, \
             such as layered packages.",
            None,
        ),
        State::UsrOverlay => (
            "Updates are paused: this system was changed outside Apex (a temporary /usr \
             overlay is active).",
            Some("Updates resume after a restart, which also removes those changes."),
        ),
        State::FinalizeFailed => (
            "Installing the last update failed during shutdown.",
            Some("See: journalctl -b -1 -u ostree-finalize-staged.service"),
        ),
        State::HealthFailure => (
            "The last update failed its health checks.",
            Some(
                "Roll back with: apexctl update rollback --mark-bad, or keep it and clear this \
                 notice with: apexctl update acknowledge",
            ),
        ),
        State::Error => ("The last update operation failed.", None),
        State::Unknown => (
            "The update state is unknown.",
            Some(
                "The update service has not read it yet, or cannot. Try again in a moment; \
                 journalctl -u apex-updated shows why it stays unknown.",
            ),
        ),
    }
}

pub(crate) fn version_label(deployment: &Deployment) -> String {
    match (&deployment.version, &deployment.digest) {
        (Some(version), _) => version.clone(),
        (None, Some(digest)) => short_digest(digest),
        (None, None) => "unknown version".to_owned(),
    }
}

pub(crate) fn compare_age(a: &Deployment, b: &Deployment) -> Option<Ordering> {
    let version = |d: &Deployment| d.version.as_deref().and_then(Version::parse);
    if let (Some(a), Some(b)) = (version(a), version(b)) {
        return Some(a.cmp(&b));
    }
    Some(a.timestamp?.cmp(&b.timestamp?))
}

pub(crate) fn queued_rollback_goes_back(status: &ServiceStatus) -> bool {
    !status.rollback.is_empty()
        && compare_age(&status.rollback, &status.booted) == Some(Ordering::Less)
}

/// Never suggests `bootc rollback`: if `next` is still boot-counted (`apex_target` in
/// grubenv, not published by the service), that would boot `next` again, uncounted.
fn how_to_keep_running(booted: &str, next: &str) -> String {
    format!(
        "To keep running {booted}, first check whether {next} is an update whose boot check \
         is still running (apex_target is set):\n\
         sudo grub2-editenv /boot/grub2/grubenv list\n\
         Then follow docs/RECOVERY.md §1, which says what is safe in each case.\n\
         Automatic updates are paused until the next boot."
    )
}

/// bootc reports `rollback-queued` whenever its "rollback" deployment boots next: the older
/// one after `update rollback`, the newer one after an older entry was picked at boot.
fn rollback_queued_sentence(status: &ServiceStatus) -> (String, Option<String>) {
    let booted = version_label(&status.booted);
    let next = &status.rollback;
    if next.is_empty() {
        return (
            "Another version will start at the next boot (rollback queued).".to_owned(),
            Some(how_to_keep_running(&booted, "the other version")),
        );
    }
    let next_label = version_label(next);
    match compare_age(next, &status.booted) {
        Some(Ordering::Less) => (
            format!(
                "The previous version {next_label} will start at the next boot (rollback queued)."
            ),
            Some(
                "Reboot to finish the rollback. Automatic updates are paused until then."
                    .to_owned(),
            ),
        ),
        Some(Ordering::Greater) => (
            format!(
                "The running {booted} is not the default: the newer version {next_label} will \
                 start at the next boot (rollback queued)."
            ),
            Some(format!(
                "This happens after an older version is started from the boot menu.\n{}",
                how_to_keep_running(&booted, &next_label)
            )),
        ),
        _ => (
            format!(
                "{next_label} will start at the next boot instead of the running {booted} \
                 (rollback queued)."
            ),
            Some(format!(
                "Reboot to switch to it.\n{}",
                how_to_keep_running(&booted, &next_label)
            )),
        ),
    }
}

pub fn status_sentence(status: &ServiceStatus) -> (String, Option<String>) {
    if status.state == State::RollbackQueued {
        return rollback_queued_sentence(status);
    }
    let (sentence, next) = state_sentence(status.state);
    (sentence.to_owned(), next.map(str::to_owned))
}

/// A `:` before the last `/` is a registry port, not a tag.
fn repository_of(image: &str) -> &str {
    let without_digest = image.split_once('@').map_or(image, |(repo, _)| repo);
    let last_slash = without_digest.rfind('/').map_or(0, |i| i + 1);
    match without_digest[last_slash..].find(':') {
        Some(colon) => &without_digest[..last_slash + colon],
        None => without_digest,
    }
}

pub fn other_origin(booted: &Deployment, deployment: &Deployment) -> Option<String> {
    let image = deployment.image.as_deref()?;
    let repository = repository_of(image);
    if repository == repository_of(booted.image.as_deref()?) {
        return None;
    }
    Some(match repository {
        APEX_TESTING_REPOSITORY => {
            "the testing channel (signed with the CI key, not qualified for stable)".to_owned()
        }
        APEX_STABLE_REPOSITORY => "the stable channel".to_owned(),
        _ => image.to_owned(),
    })
}

pub fn progress_line(progress: f64) -> String {
    if (0.0..=1.0).contains(&progress) {
        format!("Downloading {:>3.0} %", progress * 100.0)
    } else {
        "Downloading…".to_owned()
    }
}

fn degraded_meaning(component: &str) -> &'static str {
    match component {
        "audio" => "the speaker fix is not available yet",
        "fingerprint" => "the fingerprint reader may fail to enroll fingers",
        "nvidia" => "no NVIDIA driver",
        "shell" => "stock GNOME Shell, without the Apex layer and theme",
        "terminal" => "stock Ptyxis, without the Apex terminal",
        _ => "degraded",
    }
}

fn deployment_line(deployment: &Deployment) -> String {
    let mut parts = vec![
        deployment
            .version
            .clone()
            .unwrap_or_else(|| "unknown version".to_owned()),
    ];
    if let Some(timestamp) = deployment.timestamp {
        parts.push(format!("built {}", format_utc_date(timestamp)));
    }
    if deployment.pinned == Some(true) {
        parts.push("pinned".to_owned());
    }
    parts.join("  ")
}

fn row(out: &mut String, label: &str, value: &str) {
    let _ = writeln!(out, "{label:<10} {value}");
}

fn deployment_rows(
    out: &mut String,
    label: &str,
    deployment: &Deployment,
    booted: &Deployment,
    suffix: &str,
) {
    row(
        out,
        label,
        &format!("{}{suffix}", deployment_line(deployment)),
    );
    if let Some(digest) = &deployment.digest {
        // In full: users compare it by eye with CI runs, and 12 hex digits are only 48 bits.
        row(out, "", digest);
    }
    if let Some(origin) = other_origin(booted, deployment) {
        row(out, "", &format!("from {origin}"));
    }
}

pub fn render_status(status: &ServiceStatus) -> String {
    let mut out = String::new();
    let (sentence, next) = status_sentence(status);
    let _ = writeln!(out, "{sentence}");
    if let Some(progress) = known_progress(status) {
        let _ = writeln!(out, "{}", progress_line(progress));
    }
    if let Some(next) = next {
        let _ = writeln!(out, "{next}");
    }
    let _ = writeln!(out);

    let booted = &status.booted;
    if booted.is_empty() {
        row(&mut out, "Booted", "unknown");
    } else {
        deployment_rows(&mut out, "Booted", booted, booted, "");
    }
    if !status.staged.is_empty() {
        let suffix = match status.staged.download_only {
            Some(true) => "  locked: not used at the next boot",
            Some(false) => "  applied: used at the next boot",
            None => "",
        };
        deployment_rows(&mut out, "Staged", &status.staged, booted, suffix);
    }
    if !status.rollback.is_empty() {
        deployment_rows(&mut out, "Rollback", &status.rollback, booted, "");
    }
    if !status.available.is_empty() {
        deployment_rows(&mut out, "Available", &status.available, booted, "");
    }
    row(
        &mut out,
        "Channel",
        &format!("{} (running image)", status.channel.as_str()),
    );
    let checked = if status.last_check == 0 {
        "never".to_owned()
    } else {
        format_utc(status.last_check)
    };
    row(&mut out, "Checked", &checked);
    let degraded = status.booted.degraded.clone().unwrap_or_default();
    if degraded.is_empty() {
        row(&mut out, "Degraded", "none");
    } else {
        for (i, component) in degraded.iter().enumerate() {
            let label = if i == 0 { "Degraded" } else { "" };
            row(
                &mut out,
                label,
                &format!("{component}: {}", degraded_meaning(component)),
            );
        }
    }

    let mut notes = Vec::new();
    if !status.deferred_reason.is_empty() {
        notes.push(format!(
            "The automatic download is waiting: {}",
            status.deferred_reason
        ));
    }
    if !status.health_failure.is_empty() {
        let mut note = format!(
            "Update {} failed its health checks after it was applied.",
            status.health_failure
        );
        let next_step_already_says_how_to_clear = status.state == State::HealthFailure;
        if !next_step_already_says_how_to_clear {
            note.push_str(" Clear this notice with: apexctl update acknowledge");
        }
        notes.push(note);
    }
    if !status.last_error.is_empty() {
        notes.push(format!("Last error: {}", status.last_error));
    }
    if !notes.is_empty() {
        let _ = writeln!(out);
        for note in notes {
            let _ = writeln!(out, "{note}");
        }
    }
    out
}

pub fn parse_evr(text: &str) -> Option<Evr> {
    let (epoch, rest) = match text.split_once(':') {
        Some((epoch, rest)) => (epoch.parse().ok()?, rest),
        None => (0, text),
    };
    let (version, release) = rest.rsplit_once('-')?;
    if version.is_empty() || release.is_empty() {
        return None;
    }
    Some(Evr::new(epoch, version, release))
}

/// The arches [`apex_update::summarize`] prefers as native.
const NATIVE_ARCHES: [&str; 3] = ["x86_64", "aarch64", "noarch"];

pub fn package_label(name: &str, arch: &str) -> String {
    if arch.is_empty() || NATIVE_ARCHES.contains(&arch) {
        name.to_owned()
    } else {
        format!("{name}.{arch}")
    }
}

fn to_update_change(change: &Change) -> Option<apex_update::Change> {
    let (name, arch, kind, from, to) = change;
    let kind = match kind.parse::<ChangeKind>().ok()? {
        ChangeKind::Added => apex_update::ChangeKind::Added,
        ChangeKind::Removed => apex_update::ChangeKind::Removed,
        ChangeKind::Upgraded => apex_update::ChangeKind::Upgraded,
        ChangeKind::Downgraded => apex_update::ChangeKind::Downgraded,
    };
    let evr = |text: &str| {
        if text.is_empty() {
            Some(None)
        } else {
            parse_evr(text).map(Some)
        }
    };
    Some(apex_update::Change {
        name: name.clone(),
        arch: arch.clone(),
        kind,
        from_evr: evr(from)?,
        to_evr: evr(to)?,
    })
}

pub fn render_changes(changes: &[Change]) -> String {
    let mut out = String::new();
    let converted: Option<Vec<apex_update::Change>> =
        changes.iter().map(to_update_change).collect();
    match converted {
        Some(converted) => {
            let _ = writeln!(out, "{}", apex_update::summarize(&converted));
        }
        None if changes.len() == 1 => {
            let _ = writeln!(out, "1 package changed");
        }
        None => {
            let _ = writeln!(out, "{} packages changed", changes.len());
        }
    }
    if changes.is_empty() {
        return out;
    }
    let _ = writeln!(out);
    let labels: Vec<String> = changes
        .iter()
        .map(|(name, arch, ..)| package_label(name, arch))
        .collect();
    let name_width = labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    for ((_, _, kind, from, to), label) in changes.iter().zip(&labels) {
        let versions = match (from.is_empty(), to.is_empty()) {
            (true, false) => to.clone(),
            (false, true) => from.clone(),
            _ => format!("{from} → {to}"),
        };
        let _ = writeln!(out, "  {kind:<10}  {label:<name_width$}  {versions}");
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn deployment(version: &str, digest_char: char) -> Deployment {
        Deployment {
            image: Some("ghcr.io/apex-linux/apex:45".to_owned()),
            digest: Some(format!("sha256:{}", digest_char.to_string().repeat(64))),
            version: Some(version.to_owned()),
            timestamp: Some(1_790_208_000),
            pinned: Some(false),
            download_only: None,
            degraded: Some(vec!["audio".to_owned()]),
        }
    }

    pub(crate) fn staged_status() -> ServiceStatus {
        ServiceStatus {
            state: State::Staged,
            progress: -1.0,
            booted: deployment("45.20260924.117", 'a'),
            staged: Deployment {
                download_only: Some(true),
                degraded: Some(Vec::new()),
                ..deployment("45.20260925.118", 'b')
            },
            rollback: deployment("45.20260920.110", 'c'),
            available: Deployment::default(),
            channel: Channel::Stable,
            last_check: 1_790_240_400,
            deferred_reason: String::new(),
            last_error: String::new(),
            health_failure: String::new(),
        }
    }

    #[test]
    fn human_status() {
        insta::assert_snapshot!(render_status(&staged_status()));
    }

    #[test]
    fn human_status_with_notes() {
        let status = ServiceStatus {
            state: State::Downloading,
            progress: 0.425,
            deferred_reason: "On battery below 50 %".to_owned(),
            last_error: "bootc failed: no space".to_owned(),
            staged: Deployment::default(),
            rollback: Deployment::default(),
            available: deployment("45.20260925.118", 'b'),
            last_check: 0,
            ..staged_status()
        };
        insta::assert_snapshot!(render_status(&status));
    }

    fn rollback_queued(booted: Deployment, next: Deployment) -> ServiceStatus {
        ServiceStatus {
            state: State::RollbackQueued,
            booted,
            staged: Deployment::default(),
            rollback: next,
            ..staged_status()
        }
    }

    #[test]
    fn human_status_rollback_queued_to_the_previous_version() {
        let status = rollback_queued(
            deployment("45.20260924.117", 'a'),
            deployment("45.20260920.110", 'c'),
        );
        let text = render_status(&status);
        assert!(text.starts_with(
            "The previous version 45.20260920.110 will start at the next boot (rollback queued).\n\
             Reboot to finish the rollback."
        ));
        insta::assert_snapshot!(text);
    }

    #[test]
    fn human_status_rollback_queued_after_picking_an_older_entry() {
        let status = rollback_queued(
            deployment("45.20260920.110", 'c'),
            deployment("45.20260924.117", 'a'),
        );
        let text = render_status(&status);
        assert!(
            text.starts_with(
                "The running 45.20260920.110 is not the default: the newer version \
                 45.20260924.117 will start at the next boot (rollback queued).\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("sudo grub2-editenv /boot/grub2/grubenv list\n"),
            "{text}"
        );
        assert!(text.contains("docs/RECOVERY.md §1"), "{text}");
        assert!(!text.contains("bootc rollback"), "{text}");
        assert!(!text.contains("finish the rollback"), "{text}");
        insta::assert_snapshot!(text);
    }

    #[test]
    fn a_queued_rollback_never_advises_bootc_rollback() {
        let older = deployment("45.20260920.110", 'c');
        let newer = deployment("45.20260924.117", 'a');
        let unordered = |c: char| Deployment {
            digest: Some(format!("sha256:{}", String::from(c).repeat(64))),
            ..Deployment::default()
        };
        for (status, keeps_running) in [
            (rollback_queued(newer.clone(), older.clone()), false),
            (rollback_queued(older, newer.clone()), true),
            (rollback_queued(unordered('a'), unordered('c')), true),
            (rollback_queued(newer, Deployment::default()), true),
        ] {
            let text = render_status(&status);
            assert!(!text.contains("bootc rollback"), "{text}");
            assert_eq!(text.contains("grub2-editenv"), keeps_running, "{text}");
            assert_eq!(
                text.contains("Then follow docs/RECOVERY.md §1"),
                keeps_running,
                "{text}"
            );
            assert_eq!(queued_rollback_goes_back(&status), !keeps_running, "{text}");
        }
    }

    #[test]
    fn an_unknown_state_says_to_try_again() {
        let (sentence, next) = state_sentence(State::Unknown);
        assert_eq!(sentence, "The update state is unknown.");
        let next = next.unwrap();
        assert!(next.contains("Try again in a moment"), "{next}");
        assert!(next.contains("journalctl -u apex-updated"), "{next}");
    }

    #[test]
    fn rollback_queued_without_versions_falls_back_to_build_time_then_neutral() {
        let bare = |c, timestamp| Deployment {
            digest: Some(format!("sha256:{}", String::from(c).repeat(64))),
            timestamp,
            ..Deployment::default()
        };
        let (sentence, _) =
            status_sentence(&rollback_queued(bare('a', Some(200)), bare('c', Some(100))));
        assert_eq!(
            sentence,
            "The previous version sha256:cccccccccccc will start at the next boot (rollback queued)."
        );
        let (sentence, next) = status_sentence(&rollback_queued(bare('a', None), bare('c', None)));
        assert_eq!(
            sentence,
            "sha256:cccccccccccc will start at the next boot instead of the running \
             sha256:aaaaaaaaaaaa (rollback queued)."
        );
        let next = next.unwrap();
        assert!(next.starts_with("Reboot to switch to it.\n"), "{next}");
        assert!(!next.contains("bootc rollback"), "{next}");
        let (sentence, _) =
            status_sentence(&rollback_queued(bare('a', None), Deployment::default()));
        assert_eq!(
            sentence,
            "Another version will start at the next boot (rollback queued)."
        );
    }

    #[test]
    fn human_status_testing_stage_under_a_stable_system() {
        let mut status = staged_status();
        status.staged.image = Some("ghcr.io/apex-linux/apex-testing:45".to_owned());
        status.staged.version = Some("45.20260925.7".to_owned());
        let text = render_status(&status);
        assert!(
            text.contains(
                "\n           from the testing channel (signed with the CI key, not qualified \
                 for stable)\nRollback"
            ),
            "{text}"
        );
        assert!(
            text.contains("Channel    stable (running image)\n"),
            "{text}"
        );
        insta::assert_snapshot!(text);
    }

    #[test]
    fn origins() {
        let with_image = |image: &str| Deployment {
            image: Some(image.to_owned()),
            ..Deployment::default()
        };
        let stable = with_image("ghcr.io/apex-linux/apex:45");
        assert_eq!(other_origin(&stable, &stable), None);
        assert_eq!(
            other_origin(&stable, &with_image("ghcr.io/apex-linux/apex@sha256:00")),
            None
        );
        assert_eq!(
            other_origin(&with_image("ghcr.io/apex-linux/apex-testing:45"), &stable).as_deref(),
            Some("the stable channel")
        );
        assert_eq!(
            other_origin(&stable, &with_image("localhost:5000/apex:45")).as_deref(),
            Some("localhost:5000/apex:45")
        );
        assert_eq!(other_origin(&Deployment::default(), &stable), None);
        assert_eq!(
            repository_of("localhost:5000/apex:45"),
            "localhost:5000/apex"
        );
        assert_eq!(repository_of("localhost:5000/apex"), "localhost:5000/apex");
        assert_eq!(repository_of("quay.io/a/b@sha256:1"), "quay.io/a/b");
    }

    #[test]
    fn digests_are_printed_in_full() {
        let text = render_status(&staged_status());
        for c in ['a', 'b', 'c'] {
            let full = format!("sha256:{}", String::from(c).repeat(64));
            assert!(text.contains(&format!("\n           {full}\n")), "{text}");
        }
    }

    #[test]
    fn a_terminal_degraded_image_names_stock_ptyxis() {
        let mut status = staged_status();
        status.booted.degraded = Some(vec!["shell".to_owned(), "terminal".to_owned()]);
        let text = render_status(&status);
        assert!(
            text.contains(
                "\nDegraded   shell: stock GNOME Shell, without the Apex layer and theme\n           \
                 terminal: stock Ptyxis, without the Apex terminal\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_old_health_failure_note_says_how_to_clear_it() {
        let mut status = staged_status();
        status.state = State::UpToDate;
        status.staged = Deployment::default();
        status.health_failure = format!("sha256:{}", "f".repeat(64));
        let text = render_status(&status);
        assert!(
            text.ends_with(&format!(
                "Update sha256:{} failed its health checks after it was applied. Clear this \
                 notice with: apexctl update acknowledge\n",
                "f".repeat(64)
            )),
            "{text}"
        );
        status.state = State::HealthFailure;
        let text = render_status(&status);
        assert_eq!(
            text.matches("apexctl update acknowledge").count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn json_status_is_stable() {
        let json = serde_json::to_value(StatusJson::new(&staged_status())).unwrap();
        insta::assert_json_snapshot!(json);
    }

    #[test]
    fn progress_only_while_downloading() {
        let mut status = staged_status();
        status.progress = 0.5;
        assert_eq!(StatusJson::new(&status).progress, None);
        status.state = State::Downloading;
        assert_eq!(StatusJson::new(&status).progress, Some(0.5));
        status.progress = -1.0;
        assert_eq!(StatusJson::new(&status).progress, None);
        assert_eq!(progress_line(-1.0), "Downloading…");
        assert_eq!(progress_line(0.42), "Downloading  42 %");
    }

    #[test]
    fn evr_parsing() {
        assert_eq!(
            parse_evr("51.1-1.fc45"),
            Some(Evr::new(0, "51.1", "1.fc45"))
        );
        assert_eq!(parse_evr("2:1.0-3-4"), Some(Evr::new(2, "1.0-3", "4")));
        assert_eq!(parse_evr("nodash"), None);
        assert_eq!(parse_evr("x:1-2"), None);
        assert_eq!(parse_evr("-1"), None);
    }

    fn change(name: &str, arch: &str, kind: &str, from: &str, to: &str) -> Change {
        (
            name.into(),
            arch.into(),
            kind.into(),
            from.into(),
            to.into(),
        )
    }

    #[test]
    fn change_list() {
        let changes: Vec<Change> = vec![
            change("glibc", "i686", "upgraded", "2.42-3.fc45", "2.42-4.fc45"),
            change("glibc", "x86_64", "upgraded", "2.42-3.fc45", "2.42-4.fc45"),
            change(
                "gnome-shell",
                "x86_64",
                "upgraded",
                "51.0-1.fc45",
                "51.1-1.fc45",
            ),
            change(
                "kernel",
                "x86_64",
                "upgraded",
                "7.2.7-300.fc45",
                "7.2.9-300.fc45",
            ),
            change(
                "mesa-dri-drivers",
                "i686",
                "upgraded",
                "26.1.0-1.fc45",
                "26.1.2-1.fc45",
            ),
            change(
                "mesa-dri-drivers",
                "x86_64",
                "upgraded",
                "26.1.0-1.fc45",
                "26.1.2-1.fc45",
            ),
            change("newpkg", "noarch", "added", "", "1.0-1.fc45"),
            change("oldpkg", "x86_64", "removed", "2.0-1.fc45", ""),
        ];
        insta::assert_snapshot!(render_changes(&changes));
        assert_eq!(render_changes(&[]), "No package changes\n");
        let odd = [change("x", "x86_64", "sideways", "1-1", "2-1")];
        assert!(render_changes(&odd).starts_with("1 package changed\n"));
    }

    #[test]
    fn the_arch_is_shown_only_when_it_is_not_native() {
        assert_eq!(package_label("glibc", "x86_64"), "glibc");
        assert_eq!(package_label("tzdata", "noarch"), "tzdata");
        assert_eq!(package_label("glibc", "aarch64"), "glibc");
        assert_eq!(package_label("glibc", "i686"), "glibc.i686");
        assert_eq!(package_label("wine-core", "i386"), "wine-core.i386");
        assert_eq!(package_label("odd", ""), "odd");
    }

    #[test]
    fn the_summary_highlights_the_native_entry_of_a_multilib_package() {
        let changes = [
            change(
                "mesa-dri-drivers",
                "i686",
                "upgraded",
                "26.1.0-1.fc45",
                "26.1.3-1.fc45",
            ),
            change(
                "mesa-dri-drivers",
                "x86_64",
                "upgraded",
                "26.1.0-1.fc45",
                "26.1.2-1.fc45",
            ),
        ];
        let text = render_changes(&changes);
        assert!(
            text.starts_with("Mesa 26.1.0 → 26.1.2, 1 other package\n"),
            "{text}"
        );
        assert!(text.contains("  mesa-dri-drivers.i686  26.1.0-1.fc45 → 26.1.3-1.fc45\n"));
        assert!(text.contains("  mesa-dri-drivers       26.1.0-1.fc45 → 26.1.2-1.fc45\n"));
    }
}
