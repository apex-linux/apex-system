use std::cmp::Ordering;
use std::fmt::Write as _;
use std::path::Path;

use apex_boot::nvidia::{self, DiscreteGpu, NvidiaReport};

use apex_bootc::{APEX_STABLE_REPOSITORY, APEX_TESTING_REPOSITORY};
use apex_dbus::{Change, ChangeKind, Channel, Deployment, Hold, State, Stock};
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
  last_fallback    digest of the update that did not start and was set aside, or null
  automatic_downloads
                   true or false: whether the daily check downloads what it finds; null
                   when the update service does not report it
  degraded         components shipped degraded in the running image, e.g. [\"audio\"]
  held             lane families held at their last good build in the running image,
                   e.g. [\"gnome-shell\"]
  holds, stock     the running image's, as in a deployment below; [] when it reports none
  booted, staged, rollback, available
                   a deployment, or null when there is none
  hardware         hardware notes read on this computer, one object per check: check
                   (\"nvidia\"), ok (true or false), detail (a sentence)
  kept             deployments the update service pinned as the last version known to work
                   that are none of those, so only the boot menu offers them; [] when none

A deployment has these keys, each null when unknown:
  image            image reference, e.g. \"ghcr.io/apex-linux/apex:45\"
  digest           the full manifest digest, \"sha256:\" and 64 hex digits
  version          e.g. \"45.20260924.117\"
  timestamp        build time, Unix seconds
  pinned           true or false
  download_only    staged only: true while the update is locked (not used at the next boot)
  degraded         components shipped degraded in that image, e.g. [\"audio\"]
  held             lane families held at their last good build in that image
  holds            one object per hold: family; newer (the builds held back), since and
                   limit (Unix seconds), side (\"kernel\" or \"kmod\" for the kernel pair),
                   each null when unknown; security, true or false
  stock            one object per family shipped as Fedora's build: family; reason, null
                   when the image gives none (the break-glass image says why)
  pin_reason       why the update service pinned it; null unless it did";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentJson {
    pub image: Option<String>,
    pub digest: Option<String>,
    pub version: Option<String>,
    pub timestamp: Option<u64>,
    pub pinned: Option<bool>,
    pub download_only: Option<bool>,
    pub degraded: Option<Vec<String>>,
    pub held: Option<Vec<String>>,
    pub holds: Option<Vec<HoldJson>>,
    pub stock: Option<Vec<StockJson>>,
    pub pin_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoldJson {
    pub family: String,
    pub newer: Option<String>,
    pub since: Option<u64>,
    pub security: bool,
    pub limit: Option<u64>,
    pub side: Option<String>,
}

impl From<&Hold> for HoldJson {
    fn from(hold: &Hold) -> Self {
        Self {
            family: hold.family.clone(),
            newer: non_empty(&hold.newer),
            since: (hold.since != 0).then_some(hold.since),
            security: hold.security,
            limit: (hold.limit != 0).then_some(hold.limit),
            side: non_empty(&hold.side),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StockJson {
    pub family: String,
    pub reason: Option<String>,
}

impl From<&Stock> for StockJson {
    fn from(stock: &Stock) -> Self {
        Self {
            family: stock.family.clone(),
            reason: non_empty(&stock.reason),
        }
    }
}

fn holds_json(holds: Option<&Vec<Hold>>) -> Option<Vec<HoldJson>> {
    holds.map(|holds| holds.iter().map(HoldJson::from).collect())
}

fn stock_json(stock: Option<&Vec<Stock>>) -> Option<Vec<StockJson>> {
    stock.map(|stock| stock.iter().map(StockJson::from).collect())
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
            held: d.held,
            holds: holds_json(d.holds.as_ref()),
            stock: stock_json(d.stock.as_ref()),
            pin_reason: d.pin_reason,
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
    pub last_fallback: Option<String>,
    pub automatic_downloads: Option<bool>,
    pub degraded: Vec<String>,
    pub held: Vec<String>,
    pub holds: Vec<HoldJson>,
    pub stock: Vec<StockJson>,
    pub booted: Option<DeploymentJson>,
    pub staged: Option<DeploymentJson>,
    pub rollback: Option<DeploymentJson>,
    pub available: Option<DeploymentJson>,
    pub kept: Vec<DeploymentJson>,
    pub hardware: Vec<HardwareNote>,
}

/// Read on this computer, not from the update service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HardwareNote {
    pub check: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// S1c (ARCHITECTURE §5.3): the NVIDIA driver the running image ships, below `root`.
pub fn hardware_notes(root: &Path) -> Vec<HardwareNote> {
    let report = DiscreteGpu::of_this_machine()
        .map_or_else(NvidiaReport::Unreadable, |gpu| nvidia::report(root, &gpu));
    vec![HardwareNote {
        check: "nvidia",
        ok: report.is_ok(),
        detail: report.to_string(),
    }]
}

pub fn render_hardware_notes(notes: &[HardwareNote]) -> String {
    let mut out = String::from("\nHardware notes\n");
    for note in notes {
        let _ = writeln!(out, "  {:<8} {}", note.check, note.detail);
    }
    out
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
            last_fallback: non_empty(&status.last_fallback),
            automatic_downloads: status.automatic_downloads,
            degraded: status.booted.degraded.clone().unwrap_or_default(),
            held: status.booted.held.clone().unwrap_or_default(),
            holds: holds_json(status.booted.holds.as_ref()).unwrap_or_default(),
            stock: stock_json(status.booted.stock.as_ref()).unwrap_or_default(),
            booted: DeploymentJson::from_deployment(&status.booted),
            staged: DeploymentJson::from_deployment(&status.staged),
            rollback: DeploymentJson::from_deployment(&status.rollback),
            available: DeploymentJson::from_deployment(&status.available),
            kept: status
                .kept
                .iter()
                .filter_map(DeploymentJson::from_deployment)
                .collect(),
            hardware: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_hardware(mut self, notes: Vec<HardwareNote>) -> Self {
        self.hardware = notes;
        self
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
        "adwaita" => "stock libadwaita, so apps take a new window radius only at their next start",
        "files" => "stock Files, without the Apex footer, details pane, recent strip and chips",
        "settings" => "stock Settings, without the Apex Desktop panel and Software Updates page",
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

fn held_list(held: &[String]) -> String {
    if held.is_empty() {
        "none".to_owned()
    } else {
        held.join(", ")
    }
}

/// `kernel-cachyos (kmod side), held since 2026-12-01 00:00 UTC, limit 2026-12-04 00:00 UTC`.
fn hold_text(hold: &Hold) -> String {
    let mut text = hold.family.clone();
    if !hold.side.is_empty() {
        let _ = write!(text, " ({} side)", hold.side);
    }
    if hold.since != 0 {
        let _ = write!(text, ", held since {}", format_utc(hold.since));
    }
    if hold.limit != 0 {
        let _ = write!(text, ", limit {}", format_utc(hold.limit));
    }
    text
}

/// One row per hold the lane lock describes, then any held family it does not; the plain
/// family list when the service reports no lane lock.
fn held_rows(deployment: &Deployment) -> Option<Vec<String>> {
    let holds = deployment.holds.as_deref().unwrap_or_default();
    if holds.is_empty() {
        return deployment.held.as_deref().map(|held| vec![held_list(held)]);
    }
    let mut rows: Vec<String> = holds.iter().map(hold_text).collect();
    for family in deployment.held.iter().flatten() {
        if !holds.iter().any(|hold| &hold.family == family) {
            rows.push(family.clone());
        }
    }
    Some(rows)
}

fn stock_rows(deployment: &Deployment) -> Vec<String> {
    let text = |stock: &Stock| {
        if stock.reason.is_empty() {
            stock.family.clone()
        } else {
            format!("{}: {}", stock.family, stock.reason)
        }
    };
    deployment.stock.iter().flatten().map(text).collect()
}

fn labelled_rows(out: &mut String, label: &str, rows: &[String]) {
    for (i, text) in rows.iter().enumerate() {
        row(out, if i == 0 { label } else { "" }, text);
    }
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
    if let Some(held) = &deployment.held
        && deployment.held != booted.held
    {
        row(out, "", &format!("held: {}", held_list(held)));
    }
    if let Some(origin) = other_origin(booted, deployment) {
        row(out, "", &format!("from {origin}"));
    }
    if let Some(reason) = &deployment.pin_reason {
        row(out, "", reason);
    }
}

fn deployment_section(out: &mut String, status: &ServiceStatus) {
    let booted = &status.booted;
    if booted.is_empty() {
        row(out, "Booted", "unknown");
    } else {
        deployment_rows(out, "Booted", booted, booted, "");
    }
    if !status.staged.is_empty() {
        let suffix = match status.staged.download_only {
            Some(true) => "  locked: not used at the next boot",
            Some(false) => "  applied: used at the next boot",
            None => "",
        };
        deployment_rows(out, "Staged", &status.staged, booted, suffix);
    }
    if !status.rollback.is_empty() {
        deployment_rows(out, "Rollback", &status.rollback, booted, "");
    }
    for kept in &status.kept {
        deployment_rows(out, "Kept", kept, booted, "  in the boot menu");
    }
    if !status.available.is_empty() {
        deployment_rows(out, "Available", &status.available, booted, "");
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

    deployment_section(&mut out, status);
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
    match status.automatic_downloads {
        Some(true) => row(&mut out, "Downloads", "automatic"),
        Some(false) => row(&mut out, "Downloads", "manual: the daily check only checks"),
        None => {}
    }
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
    if let Some(rows) = held_rows(&status.booted) {
        labelled_rows(&mut out, "Held", &rows);
    }
    labelled_rows(&mut out, "Stock", &stock_rows(&status.booted));

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
    if !status.last_fallback.is_empty() {
        notes.push(format!(
            "Update {} did not start, so the previous version was restored; it will not be \
             offered again. Clear this notice with: apexctl update acknowledge",
            status.last_fallback
        ));
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
            held: Some(vec!["gnome-shell".to_owned()]),
            ..Deployment::default()
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
                held: Some(Vec::new()),
                ..deployment("45.20260925.118", 'b')
            },
            rollback: deployment("45.20260920.110", 'c'),
            available: Deployment::default(),
            channel: Channel::Stable,
            last_check: 1_790_240_400,
            deferred_reason: String::new(),
            last_error: String::new(),
            health_failure: String::new(),
            last_fallback: String::new(),
            automatic_downloads: Some(true),
            kept: Vec::new(),
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
    fn held_families_are_listed_for_booted_and_where_another_image_differs() {
        let mut status = staged_status();
        status.booted.held = Some(vec!["gnome-shell".to_owned(), "libadwaita".to_owned()]);
        status.rollback.held = status.booted.held.clone();
        status.staged.held = Some(vec!["libadwaita".to_owned()]);
        let text = render_status(&status);
        assert!(
            text.contains("\nHeld       gnome-shell, libadwaita\n"),
            "{text}"
        );
        assert_eq!(text.matches("held: ").count(), 1, "{text}");
        assert!(text.contains("\n           held: libadwaita\n"), "{text}");

        status.booted.held = Some(Vec::new());
        status.staged.held = Some(Vec::new());
        status.rollback.held = Some(vec!["gnome-shell".to_owned()]);
        let text = render_status(&status);
        assert!(text.contains("\nHeld       none\n"), "{text}");
        assert!(text.contains("\n           held: gnome-shell\n"), "{text}");
    }

    fn kernel_hold(side: &str) -> Hold {
        Hold {
            family: "kernel-cachyos".to_owned(),
            newer: "kernel-cachyos-0:7.3.1-1.cachyos1.fc45.src".to_owned(),
            since: 1_796_083_200,
            security: true,
            limit: 1_796_342_400,
            side: side.to_owned(),
        }
    }

    fn up_to_date_with(booted: Deployment) -> ServiceStatus {
        ServiceStatus {
            state: State::UpToDate,
            booted,
            staged: Deployment::default(),
            rollback: Deployment::default(),
            ..staged_status()
        }
    }

    #[test]
    fn a_kernel_side_hold_with_its_clock() {
        let status = up_to_date_with(Deployment {
            degraded: Some(Vec::new()),
            held: Some(vec!["kernel-cachyos".to_owned()]),
            holds: Some(vec![kernel_hold("kernel")]),
            stock: Some(Vec::new()),
            ..deployment("45.20261201.140", 'a')
        });
        insta::assert_snapshot!(render_status(&status));
    }

    #[test]
    fn a_kmod_side_hold_next_to_a_lane_family() {
        let gnome_shell = Hold {
            family: "gnome-shell".to_owned(),
            newer: "gnome-shell-51.1-1.fc45".to_owned(),
            since: 1_796_083_200,
            security: false,
            limit: 0,
            side: String::new(),
        };
        let status = up_to_date_with(Deployment {
            degraded: Some(Vec::new()),
            held: Some(vec!["gnome-shell".to_owned(), "kernel-cachyos".to_owned()]),
            holds: Some(vec![gnome_shell, kernel_hold("kmod")]),
            stock: Some(Vec::new()),
            ..deployment("45.20261201.140", 'a')
        });
        let text = render_status(&status);
        assert!(
            text.contains(
                "kernel-cachyos (kmod side), held since 2026-12-01 00:00 UTC, \
                               limit 2026-12-04 00:00 UTC"
            ),
            "{text}"
        );
        insta::assert_snapshot!(text);
    }

    #[test]
    fn degraded_nvidia_reads_no_nvidia_driver() {
        let status = up_to_date_with(Deployment {
            degraded: Some(vec!["nvidia".to_owned()]),
            held: Some(Vec::new()),
            holds: Some(Vec::new()),
            stock: Some(Vec::new()),
            ..deployment("45.20261205.144", 'a')
        });
        let text = render_status(&status);
        assert!(
            text.contains("\nDegraded   nvidia: no NVIDIA driver\n"),
            "{text}"
        );
        insta::assert_snapshot!(text);
    }

    #[test]
    fn the_break_glass_image_says_why() {
        let reason =
            "break-glass: just image KERNEL=fedora, Fedora's kernel without the NVIDIA set";
        let status = up_to_date_with(Deployment {
            degraded: Some(vec!["nvidia".to_owned()]),
            held: Some(Vec::new()),
            holds: Some(Vec::new()),
            stock: Some(vec![Stock {
                family: "kernel-cachyos".to_owned(),
                reason: reason.to_owned(),
            }]),
            ..deployment("45.20261206.145", 'a')
        });
        let text = render_status(&status);
        assert!(
            text.contains(&format!("\nStock      kernel-cachyos: {reason}\n")),
            "{text}"
        );
        insta::assert_snapshot!(text);
        insta::assert_json_snapshot!(
            "the_break_glass_image_json",
            serde_json::to_value(StatusJson::new(&status)).unwrap()
        );
    }

    #[test]
    fn a_version_kept_before_an_update_says_why() {
        let mut status = staged_status();
        status.state = State::ReadyForReboot;
        status.staged.download_only = Some(false);
        status.rollback.pinned = Some(true);
        status.rollback.pin_reason = Some(
            "Kept as the last version known to work: an update was installed before the \
             version running then had passed its startup check."
                .to_owned(),
        );
        let text = render_status(&status);
        insta::assert_snapshot!(text);
        let json = StatusJson::new(&status);
        assert_eq!(
            json.rollback.unwrap().pin_reason,
            status.rollback.pin_reason
        );
        assert_eq!(json.booted.unwrap().pin_reason, None);
    }

    #[test]
    fn a_version_kept_after_the_update_is_listed_with_the_boot_menu() {
        let mut status = up_to_date_with(deployment("45.20260925.118", 'b'));
        status.rollback = deployment("45.20260924.117", 'a');
        status.kept = vec![Deployment {
            pinned: Some(true),
            degraded: None,
            held: None,
            pin_reason: Some("Kept as the last version known to work.".to_owned()),
            ..deployment("45.20260920.110", 'c')
        }];
        let text = render_status(&status);
        insta::assert_snapshot!(text);
        let json = StatusJson::new(&status);
        assert_eq!(json.kept.len(), 1);
        assert_eq!(json.kept[0].pin_reason, status.kept[0].pin_reason);
        status.kept.clear();
        assert!(!render_status(&status).contains("Kept"));
    }

    fn fake_root(manifest: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("usr/share/apex/manifest.json", manifest);
        let device = "sys/bus/pci/devices/0000:01:00.0";
        write(&format!("{device}/vendor"), "0x10de\n");
        write(&format!("{device}/device"), "0x25a2\n");
        let driver = root.path().join("sys/bus/pci/drivers/nvidia");
        std::fs::create_dir_all(&driver).unwrap();
        std::os::unix::fs::symlink(driver, root.path().join(device).join("driver")).unwrap();
        write("sys/module/nvidia/version", "615.78.08\n");
        root
    }

    #[test]
    fn hardware_notes_report_the_nvidia_driver() {
        let ready = fake_root(
            r#"{"packages": [{"name": "kmod-nvidia-7.3.0-4.cachyos1.fc45.x86_64", "version": "615.78.08"}]}"#,
        );
        let notes = hardware_notes(ready.path());
        assert_eq!(
            notes,
            [HardwareNote {
                check: "nvidia",
                ok: true,
                detail: "NVIDIA driver 615.78.08 is bound to the dGPU, as the image ships it"
                    .to_owned(),
            }]
        );
        let degraded = fake_root(r#"{"degraded": ["nvidia"], "packages": []}"#);
        let notes = hardware_notes(degraded.path());
        insta::assert_snapshot!(render_hardware_notes(&notes));
        assert!(!notes[0].ok);
        let json =
            serde_json::to_value(StatusJson::new(&staged_status()).with_hardware(notes)).unwrap();
        assert_eq!(
            json["hardware"],
            serde_json::json!([{
                "check": "nvidia",
                "ok": false,
                "detail": "no NVIDIA driver (degraded)",
            }])
        );
    }

    #[test]
    fn an_older_service_without_held_prints_no_held_row() {
        let mut status = staged_status();
        for deployment in [&mut status.booted, &mut status.staged, &mut status.rollback] {
            deployment.held = None;
        }
        let text = render_status(&status);
        assert!(!text.contains("Held") && !text.contains("held:"), "{text}");
        assert_eq!(StatusJson::new(&status).held, Vec::<String>::new());
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
    fn stock_app_degraded_images_name_what_they_lack() {
        let mut status = staged_status();
        status.booted.degraded = Some(vec![
            "adwaita".to_owned(),
            "files".to_owned(),
            "settings".to_owned(),
        ]);
        let text = render_status(&status);
        assert!(
            text.contains(
                "\nDegraded   adwaita: stock libadwaita, so apps take a new window radius only at \
                 their next start\n           files: stock Files, without the Apex footer, details pane, recent \
                 strip and chips\n           settings: stock Settings, without the Apex Desktop \
                 panel and Software Updates page\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_fallback_and_manual_downloads_are_shown() {
        let mut status = staged_status();
        status.last_fallback = format!("sha256:{}", "e".repeat(64));
        status.automatic_downloads = Some(false);
        let text = render_status(&status);
        assert!(
            text.contains("\nDownloads  manual: the daily check only checks\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "Update sha256:{} did not start, so the previous version was restored; it will \
                 not be offered again. Clear this notice with: apexctl update acknowledge\n",
                "e".repeat(64)
            )),
            "{text}"
        );
        let json = StatusJson::new(&status);
        assert_eq!(json.last_fallback, Some(status.last_fallback.clone()));
        assert_eq!(json.automatic_downloads, Some(false));

        status.automatic_downloads = None;
        status.last_fallback.clear();
        let text = render_status(&status);
        assert!(
            !text.contains("Downloads") && !text.contains("did not start"),
            "{text}"
        );
        assert_eq!(StatusJson::new(&status).automatic_downloads, None);
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
