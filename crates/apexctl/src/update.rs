//! `apexctl update …`: each subcommand as a function over [`UpdateService`].
//!
//! Every function returns the text for stdout or a [`ClientError`] whose
//! [`exit`](ClientError::exit) gives the process exit code (0 ok, 1 error, 2 policy denied,
//! 3 nothing to do). None of them reboots: `apply` and `rollback` only say that a reboot
//! finishes the job, because the daemon never reboots and the desktop session owns that
//! decision (ADR 0008).

use std::cmp::Ordering;
use std::fmt::Write as _;

use apex_bootc::Digest;
use apex_dbus::{ApexError, Channel, Deployment, ErrorKind, State};

use crate::cli::UpdateCommand;
use crate::exit::Exit;
use crate::service::{ClientError, ServiceStatus, UpdateService};
use crate::status::{
    compare_age, other_origin, progress_line, queued_rollback_goes_back, render_changes,
    short_digest, status_sentence, version_label,
};

/// `update automatic` refused as not authorized: the daemon accepts it from uid 0 only.
const AUTOMATIC_ROOT_HINT: &str = "This command is for the system timer \
(apex-update-auto.service) and runs as root: sudo apexctl update automatic";

/// `update discard` failed because the registry cannot be reached.
const DISCARD_NETWORK_HINT: &str = "The update is still set to install at the next restart. \
Discarding locks it again by downloading from the update channel, which needs the network; \
try again once online.";

/// `update discard` failed in bootc, or the service could not confirm the outcome.
const DISCARD_STATUS_HINT: &str =
    "Check with apexctl status whether the update is still set to install at the next restart.";

/// The follow-up line printed after `command` failed with `error`: [`ClientError::hint`],
/// with these exceptions:
///
/// - the `--force` advice for a refused gate follows only a plain `update download`, the
///   one command whose power and metered gates `--force` overrides. After any other
///   command (or `download --force`) the refusal came from a gate that nothing overrides;
/// - `update automatic` is root-only (checked by uid, not polkit), so the desktop-session
///   advice for `NotAuthorized` would be wrong;
/// - a failed `update discard` says whether the update may still install.
pub fn hint(command: &UpdateCommand, error: &ClientError) -> Option<&'static str> {
    match (command, daemon_kind(error)) {
        (UpdateCommand::Automatic, Some(ErrorKind::NotAuthorized)) => {
            return Some(AUTOMATIC_ROOT_HINT);
        }
        (UpdateCommand::Discard, Some(ErrorKind::Network)) => return Some(DISCARD_NETWORK_HINT),
        (UpdateCommand::Discard, Some(ErrorKind::Bootc | ErrorKind::Internal)) => {
            return Some(DISCARD_STATUS_HINT);
        }
        _ => {}
    }
    if error.exit() == Exit::PolicyDenied
        && !matches!(command, UpdateCommand::Download { force: false })
    {
        return None;
    }
    error.hint()
}

/// The kind of a contract error reply from the daemon; `None` for any other failure.
fn daemon_kind(error: &ClientError) -> Option<ErrorKind> {
    match error {
        ClientError::Daemon(error) => error.kind(),
        _ => None,
    }
}

/// The deployment's digest, or a placeholder.
fn digest_of(deployment: &Deployment) -> &str {
    deployment.digest.as_deref().unwrap_or("unknown digest")
}

/// `It comes from the testing channel (…).` when `deployment` is not from the running
/// image's repository (after `apexctl update channel`).
fn origin_sentence(status: &ServiceStatus, deployment: &Deployment) -> Option<String> {
    other_origin(&status.booted, deployment).map(|origin| format!("It comes from {origin}."))
}

/// Whether `deployment` is the build that is running: a stage made by `apexctl update
/// channel` when the other channel offers the same image (an origin-only switch).
fn is_running_build(status: &ServiceStatus, deployment: &Deployment) -> bool {
    deployment.digest.is_some() && deployment.digest == status.booted.digest
}

/// What applying an origin-only channel switch does (M1 contract §5).
const SAME_BUILD_NOTE: &str = "It is the build that is running now, so only the channel \
changes: the same build starts again, and no boot check is armed for that restart.";

/// A newer build the last check found while an older one is staged: the `staged` and
/// `ready-for-reboot` states rank before `available`, so the state alone hides it.
fn newer_than_stage(status: &ServiceStatus) -> Option<&Deployment> {
    let available = &status.available;
    (!available.is_empty() && !status.staged.is_empty() && available.digest != status.staged.digest)
        .then_some(available)
}

fn version_of(deployment: &apex_dbus::Deployment) -> String {
    deployment
        .version
        .clone()
        .unwrap_or_else(|| "the update".to_owned())
}

/// `update check` answered `Busy`: the service did not check. The status read afterwards
/// is best effort, since the skip itself is the answer.
async fn check_skipped<S: UpdateService>(service: &S, reason: &str) -> String {
    let mut out = String::new();
    match service.status().await {
        Ok(status) if status.state == State::Downloading => {
            let _ = writeln!(out, "Not checked: an update is being downloaded now.");
            let _ = writeln!(out, "{}", progress_line(status.progress));
            let _ = writeln!(
                out,
                "Check again when the download has finished; apexctl status shows its progress."
            );
        }
        _ => {
            let _ = writeln!(out, "Not checked: the update service is busy. {reason}");
            let _ = writeln!(out, "apexctl status shows what it is doing.");
        }
    }
    out
}

/// `apexctl update check`.
///
/// With an older update already staged, a newer build found by the check is named too:
/// the state stays `staged` (or `ready-for-reboot`), so the state sentence alone would
/// advise applying the older one.
///
/// While a download, a channel switch, the timer's run or a discard runs or waits, the
/// service answers `Busy` without checking: those fetch from the update channel
/// themselves. That is not a failure, so the command says that nothing was checked and
/// exits 0, as `apexctl update check --help` documents. Any other `Busy` (a full queue,
/// the service stopping) is reported the same way, with the service's reason.
pub async fn check<S: UpdateService>(service: &S) -> Result<String, ClientError> {
    if let Err(error) = service.check().await {
        return match &error {
            ClientError::Daemon(busy) if busy.kind() == Some(ErrorKind::Busy) => {
                Ok(check_skipped(service, &busy.message()).await)
            }
            _ => Err(error),
        };
    }
    let status = service.status().await?;
    let mut out = String::new();
    match status.state {
        State::Available if !status.available.is_empty() => {
            let _ = writeln!(
                out,
                "An update is available: {} ({}).",
                version_of(&status.available),
                short_digest(digest_of(&status.available))
            );
            if let Some(origin) = origin_sentence(&status, &status.available) {
                let _ = writeln!(out, "{origin}");
            }
            let _ = writeln!(out, "Download it with: apexctl update download");
        }
        State::UpToDate => {
            let booted = status
                .booted
                .version
                .as_deref()
                .unwrap_or("unknown version");
            let _ = writeln!(out, "Apex is up to date ({booted}).");
        }
        _ => {
            let (sentence, next) = status_sentence(&status);
            let _ = writeln!(out, "{sentence}");
            match newer_than_stage(&status) {
                Some(newer) => {
                    let _ = writeln!(
                        out,
                        "A newer update is available: {} ({}).",
                        version_of(newer),
                        short_digest(digest_of(newer))
                    );
                    if status.staged.download_only == Some(false) {
                        let _ = writeln!(
                            out,
                            "To install it instead, first cancel the applied update: apexctl update discard"
                        );
                    } else {
                        let _ = writeln!(
                            out,
                            "Download it in place of {} with: apexctl update download",
                            version_of(&status.staged)
                        );
                    }
                }
                None => {
                    if let Some(next) = next {
                        let _ = writeln!(out, "{next}");
                    }
                }
            }
        }
    }
    if status.stale {
        let _ = writeln!(
            out,
            "No newer build was published on this channel for more than 7 days."
        );
    }
    Ok(out)
}

/// `apexctl update download [--force]`. `force` overrides the power and metered gates only.
pub async fn download<S: UpdateService>(service: &S, force: bool) -> Result<String, ClientError> {
    service.download(force).await?;
    let status = service.status().await?;
    let mut out = String::new();
    if status.staged.is_empty() {
        let (sentence, _) = status_sentence(&status);
        let _ = writeln!(out, "Download finished. {sentence}");
    } else {
        let _ = writeln!(
            out,
            "Downloaded {} ({}). It stays locked until you apply it.",
            version_of(&status.staged),
            short_digest(digest_of(&status.staged))
        );
        if let Some(origin) = origin_sentence(&status, &status.staged) {
            let _ = writeln!(out, "{origin}");
        }
        let _ = writeln!(out, "Use it at the next restart with: apexctl update apply");
    }
    Ok(out)
}

/// The digest `apply` should confirm: `--digest`, validated, or the staged one.
fn apply_target(status: &ServiceStatus, digest: Option<&str>) -> Result<String, ClientError> {
    if let Some(digest) = digest {
        return Digest::parse(digest)
            .map(|d| d.as_str().to_owned())
            .map_err(|e| ClientError::InvalidArgument(format!("--digest {digest:?}: {e}")));
    }
    match status.staged.digest.as_deref() {
        Some(staged) if !status.staged.is_empty() => Ok(staged.to_owned()),
        _ => Err(ClientError::NothingToDo(
            "Nothing is staged, so there is nothing to apply. Download an update first: apexctl update download"
                .to_owned(),
        )),
    }
}

/// `apexctl update apply [--digest D]`: unlocks the staged update for the next boot.
/// Without `--digest` it applies the staged digest that `apexctl status` shows.
pub async fn apply<S: UpdateService>(
    service: &S,
    digest: Option<&str>,
) -> Result<String, ClientError> {
    let before = service.status().await?;
    let target = apply_target(&before, digest)?;
    service.apply(&target).await?;
    let staged_is_target = before.staged.digest.as_deref() == Some(target.as_str());
    let version = if staged_is_target {
        version_of(&before.staged)
    } else {
        "the update".to_owned()
    };
    let mut out = format!(
        "Applied {version} ({}). It will be used at the next boot.\n",
        short_digest(&target)
    );
    if staged_is_target {
        if let Some(origin) = origin_sentence(&before, &before.staged) {
            let _ = writeln!(out, "{origin}");
        }
        if is_running_build(&before, &before.staged) {
            let _ = writeln!(out, "{SAME_BUILD_NOTE}");
        }
    }
    out.push_str("Reboot to finish the update. apexctl never reboots the computer by itself.\n");
    Ok(out)
}

/// `apexctl update discard`: locks an applied, not yet booted update again.
///
/// The daemon re-locks by pulling the channel's tag again (Update1 `DiscardStaged`), so
/// when the channel moved on, another build replaces the applied one, locked. The status
/// read afterwards says which of the two happened. Either way the locked build is named
/// with its full digest and the `apply --digest` command for exactly that build: the
/// automatic download may replace a locked stage with a newer build later, so nothing
/// promises that this one stays downloaded, and a plain `apply` would take whatever is
/// staged by then. A replacement is also offered for review first (ADR 0008).
pub async fn discard<S: UpdateService>(service: &S) -> Result<String, ClientError> {
    let before = service.status().await?;
    service.discard_staged().await?;
    let after = service.status().await?;
    // Starts a sentence.
    let applied = before
        .staged
        .version
        .clone()
        .unwrap_or_else(|| "The update".to_owned());
    let mut out = String::new();
    let _ = writeln!(out, "{applied} will not be installed at the next restart.");
    let staged = &after.staged;
    if staged.is_empty() {
        let _ = writeln!(
            out,
            "No update is downloaded now. Download one again with: apexctl update download"
        );
        return Ok(out);
    }
    // Whether the locked build is the one that was applied; unknown without both digests.
    let same = match (&before.staged.digest, &staged.digest) {
        (Some(applied), Some(locked)) => Some(applied == locked),
        _ => None,
    };
    let _ = writeln!(
        out,
        "{}",
        match same {
            Some(true) => "The update channel still offers it, so it is downloaded and locked:",
            Some(false) => {
                "The update channel has moved on: cancelling downloaded its current build, \
                 which replaced it and is locked:"
            }
            None => "The update downloaded now is locked:",
        }
    );
    let _ = writeln!(out, "  {}  {}", version_of(staged), digest_of(staged));
    if let Some(origin) = origin_sentence(&after, staged) {
        let _ = writeln!(out, "{origin}");
    }
    if same != Some(true) {
        let _ = writeln!(out, "Review it with: apexctl update changes");
    }
    match &staged.digest {
        Some(digest) => {
            let _ = writeln!(
                out,
                "To install exactly this build later: apexctl update apply --digest {digest}"
            );
        }
        None => {
            let _ = writeln!(
                out,
                "To install it later, check apexctl status, then: apexctl update apply"
            );
        }
    }
    let _ = writeln!(
        out,
        "An automatic download may replace it with a newer build before then."
    );
    Ok(out)
}

/// `apexctl update rollback [--mark-bad]`.
pub async fn rollback<S: UpdateService>(
    service: &S,
    mark_bad: bool,
) -> Result<String, ClientError> {
    let before = service.status().await?;
    if let Err(error) = service.rollback(mark_bad).await {
        // The daemon refuses while a rollback is queued. Its message assumes the queued
        // deployment is the older one; name it, and say what to do instead. When the
        // running version may be the one to keep, that is the status text's safe path
        // (RECOVERY.md §1), never a bare `bootc rollback`: the queued deployment may be an
        // update whose boots are still being counted, which the service does not publish.
        if before.state == State::RollbackQueued && error.exit() == Exit::NothingToDo {
            let (sentence, next) = status_sentence(&before);
            let mut text = sentence;
            if !queued_rollback_goes_back(&before) {
                text.push_str("\napexctl update rollback cannot undo a queued rollback.");
            }
            if let Some(next) = next {
                text.push('\n');
                text.push_str(&next);
            }
            return Err(ClientError::NothingToDo(text));
        }
        return Err(error);
    }
    let mut out = String::new();
    let previous = before
        .rollback
        .version
        .clone()
        .unwrap_or_else(|| "the previous version".to_owned());
    let _ = writeln!(out, "{previous} will start at the next boot.");
    if mark_bad {
        let booted = before
            .booted
            .digest
            .as_deref()
            .map_or_else(|| "The current version".to_owned(), short_digest);
        let _ = writeln!(out, "{booted} is marked bad and will not be offered again.");
    }
    let _ = writeln!(
        out,
        "Reboot to finish the rollback. apexctl never reboots the computer by itself."
    );
    Ok(out)
}

/// `apexctl update changes`: the package diff from the booted to the staged image, with
/// the staged version and full digest above it and, below, the `apply --digest` command
/// that applies exactly that image.
///
/// The automatic timer may replace a locked stage at any time, and `apply` without
/// `--digest` takes whatever is staged then. The status is read before and after
/// `GetChanges`; if the staged digest changed in between, the list may describe the old
/// image, so the command fails instead of printing it.
///
/// `GetChanges` fails while the service does not know the system's state yet: `Busy` right
/// after it started (it waits up to 20 s for its first reading) and `Internal` when bootc
/// cannot be used. Both exit 1 with a message about the package list rather than the
/// generic "another update operation is running".
pub async fn changes<S: UpdateService>(service: &S) -> Result<String, ClientError> {
    let before = service.status().await?;
    let changes = service.changes().await.map_err(changes_error)?;
    let status = service.status().await?;
    if status.staged.is_empty() {
        return Err(ClientError::NothingToDo(
            "Nothing is staged, so there are no package changes to show.".to_owned(),
        ));
    }
    if before.staged.digest != status.staged.digest {
        return Err(ClientError::Changed(
            "The downloaded update was replaced while its package list was read. Run \
             apexctl update changes again."
                .to_owned(),
        ));
    }
    let staged = &status.staged;
    let mut out = String::new();
    let _ = write!(
        out,
        "Changes from {} to {}",
        version_of(&status.booted),
        version_of(staged)
    );
    match &staged.digest {
        Some(digest) => {
            let _ = writeln!(out, " ({digest})");
        }
        None => out.push('\n'),
    }
    if let Some(origin) = origin_sentence(&status, staged) {
        let _ = writeln!(out, "{origin}");
    }
    let _ = writeln!(out);
    out.push_str(&render_changes(&changes));
    let _ = writeln!(out);
    match (&staged.digest, staged.download_only) {
        (_, Some(false)) => {
            let _ = writeln!(
                out,
                "This update is already set to install at the next restart."
            );
        }
        (Some(digest), _) => {
            let _ = writeln!(
                out,
                "Apply exactly this update with: apexctl update apply --digest {digest}"
            );
        }
        (None, _) => {
            let _ = writeln!(out, "Apply it with: apexctl update apply");
        }
    }
    Ok(out)
}

/// A failed `GetChanges`, phrased for `update changes` (see [`changes`]).
fn changes_error(error: ClientError) -> ClientError {
    let ClientError::Daemon(reply) = &error else {
        return error;
    };
    match reply.kind() {
        Some(ErrorKind::Busy) => ClientError::NotReady(format!(
            "Cannot list the package changes now. {}",
            reply.message()
        )),
        Some(ErrorKind::Internal) => ClientError::Daemon(ApexError::new(
            ErrorKind::Internal,
            format!("cannot list the package changes: {}", reply.message()),
        )),
        _ => error,
    }
}

/// `apexctl update acknowledge`: clears the `HealthFailure` notice once the user has read
/// it (contract §7; polkit `discard`, contract §8).
///
/// Only the notice goes away: nothing is rolled back and the deployment keeps running.
/// Without a recorded failure this is nothing to do (exit 3) and the daemon is not called.
pub async fn acknowledge<S: UpdateService>(service: &S) -> Result<String, ClientError> {
    let before = service.status().await?;
    let failed = before.health_failure.as_str();
    if failed.is_empty() {
        return Err(ClientError::NothingToDo(
            "No update has failed its health checks, so there is nothing to acknowledge."
                .to_owned(),
        ));
    }
    service.acknowledge_health_failure().await?;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Acknowledged: update {} failed its health checks. The notice is cleared; nothing was \
         rolled back.",
        short_digest(failed)
    );
    // `rollback --mark-bad` denylists the booted digest, so suggest it only when the failed
    // update is the one running.
    if before.booted.digest.as_deref() == Some(failed) {
        let _ = writeln!(
            out,
            "It is still the running version. To go back to the previous version instead: \
             apexctl update rollback --mark-bad"
        );
    }
    Ok(out)
}

/// `apexctl update channel stable|testing`: follow another channel of the same Fedora
/// release (polkit `switch-channel`, `auth_admin`).
///
/// The daemon downloads the channel's image and stages it locked before it replies, so the
/// running system is unchanged until the user applies the update and restarts. `Apply`
/// installs such a stage even when it is not newer: choosing the channel was the
/// confirmation. When the other channel offers the running build (an origin-only switch),
/// applying it changes only the channel and arms no boot check (M1 contract §5); the
/// output says both. Asking for the channel already followed is the daemon's `NothingToDo`
/// (exit 3); the check is left to the daemon because the `Channel` property names the
/// booted image's channel, which differs from the followed one after an earlier switch
/// that was not booted yet.
pub async fn channel<S: UpdateService>(
    service: &S,
    channel: Channel,
) -> Result<String, ClientError> {
    service.switch_channel(channel).await?;
    let status = service.status().await?;
    let staged = &status.staged;
    let mut out = String::new();
    let _ = write!(out, "Apex now follows the {channel} channel.");
    if staged.is_empty() {
        let (sentence, _) = status_sentence(&status);
        let _ = writeln!(out, " {sentence}");
    } else {
        let _ = writeln!(
            out,
            " Its image {} ({}) is downloaded and stays locked until you apply it.",
            version_of(staged),
            short_digest(digest_of(staged))
        );
        if is_running_build(&status, staged) {
            let _ = writeln!(out, "{SAME_BUILD_NOTE}");
        } else if compare_age(staged, &status.booted) == Some(Ordering::Less) {
            let _ = writeln!(
                out,
                "It is older than the running {}; applying it installs it anyway, since you \
                 chose this channel.",
                version_label(&status.booted)
            );
        }
        let _ = writeln!(out, "To finish the switch, run: apexctl update apply");
        let _ = writeln!(
            out,
            "Then restart the computer; apexctl never restarts it by itself."
        );
    }
    if channel == Channel::Testing {
        let _ = writeln!(
            out,
            "Testing images are signed with the CI key and have not been qualified for stable."
        );
    }
    Ok(out)
}

/// Why an automatic run that failed with `error` only waits for the next timer run: another
/// update operation holds the daemon (`Busy`; a user download can take two hours), or the
/// registry cannot be reached (`Network`; an offline laptop). `None` for real failures.
fn automatic_deferral(error: &ClientError) -> Option<String> {
    let ClientError::Daemon(error) = error else {
        return None;
    };
    let why = match error.kind()? {
        ErrorKind::Busy => "another update operation is running",
        ErrorKind::Network => "the image registry cannot be reached",
        _ => return None,
    };
    Some(format!(
        "Automatic update run skipped: {why} ({}). The next timer run tries again.\n",
        error.message()
    ))
}

/// `apexctl update automatic`: the timer's run (root only; the daemon checks the uid).
///
/// The daemon treats a deferral as success ("the timer did its job"). A run that finds
/// another operation running (`Busy`) or no network (`Network`) is the same kind of
/// deferral, so it exits 0 with the reason on stdout (the journal of
/// `apex-update-auto.service`) instead of leaving a failed unit behind. A network failure
/// is still recorded by the daemon and shown by `apexctl status` as the last error.
pub async fn automatic<S: UpdateService>(service: &S) -> Result<String, ClientError> {
    if let Err(error) = service.automatic().await {
        return automatic_deferral(&error).ok_or(error);
    }
    let status = service.status().await?;
    let mut out = String::new();
    let (sentence, _) = status_sentence(&status);
    let _ = writeln!(out, "Automatic update run finished. {sentence}");
    if !status.deferred_reason.is_empty() {
        let _ = writeln!(out, "Download deferred: {}", status.deferred_reason);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_dbus::{ApexError, Change, Deployment, ErrorKind};
    use std::cell::RefCell;
    use std::future::{Future, ready};

    /// A scripted service: fixed status, one optional error for the next call, a call log.
    struct Fake {
        status: RefCell<ServiceStatus>,
        /// The status after the next successful method call (what the daemon changed).
        after: RefCell<Option<ServiceStatus>>,
        fail_with: RefCell<Option<ErrorKind>>,
        changes: Vec<Change>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(status: ServiceStatus) -> Self {
            Self {
                status: RefCell::new(status),
                after: RefCell::new(None),
                fail_with: RefCell::new(None),
                changes: Vec::new(),
                calls: RefCell::new(Vec::new()),
            }
        }

        /// A fake whose status becomes `after` once a method call succeeds.
        fn changing(before: ServiceStatus, after: ServiceStatus) -> Self {
            let fake = Self::new(before);
            *fake.after.borrow_mut() = Some(after);
            fake
        }

        fn call(&self, name: String) -> Result<(), ClientError> {
            self.calls.borrow_mut().push(name);
            if let Some(kind) = self.fail_with.borrow_mut().take() {
                return Err(ClientError::Daemon(ApexError::new(kind, "refused")));
            }
            if let Some(after) = self.after.borrow_mut().take() {
                *self.status.borrow_mut() = after;
            }
            Ok(())
        }
    }

    // The fake answers immediately, so every method is a ready future.
    impl UpdateService for Fake {
        fn status(&self) -> impl Future<Output = Result<ServiceStatus, ClientError>> {
            ready(Ok(self.status.borrow().clone()))
        }
        fn check(&self) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call("check".into()))
        }
        fn download(&self, force: bool) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call(format!("download({force})")))
        }
        fn apply(&self, digest: &str) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call(format!("apply({digest})")))
        }
        fn discard_staged(&self) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call("discard".into()))
        }
        fn rollback(&self, mark_bad: bool) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call(format!("rollback({mark_bad})")))
        }
        fn changes(&self) -> impl Future<Output = Result<Vec<Change>, ClientError>> {
            ready(self.call("changes".into()).map(|()| self.changes.clone()))
        }
        fn switch_channel(
            &self,
            channel: Channel,
        ) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call(format!("switch_channel({channel})")))
        }
        fn acknowledge_health_failure(&self) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call("acknowledge".into()))
        }
        fn automatic(&self) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call("automatic".into()))
        }
    }

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    fn status(state: State) -> ServiceStatus {
        let deployment = |version: &str, c| Deployment {
            digest: Some(digest(c)),
            version: Some(version.to_owned()),
            ..Deployment::default()
        };
        ServiceStatus {
            state,
            progress: -1.0,
            booted: deployment("45.20260924.117", 'a'),
            staged: Deployment {
                download_only: Some(true),
                ..deployment("45.20260925.118", 'b')
            },
            rollback: deployment("45.20260920.110", 'c'),
            available: Deployment::default(),
            channel: Channel::Stable,
            last_check: 0,
            stale: false,
            deferred_reason: String::new(),
            last_error: String::new(),
            health_failure: String::new(),
        }
    }

    /// Runs a future on a `current_thread` runtime, like the binary does.
    fn run<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn apply_defaults_to_the_staged_digest_and_never_reboots() {
        let fake = Fake::new(status(State::Staged));
        let out = run(apply(&fake, None)).unwrap();
        assert_eq!(
            fake.calls.borrow().as_slice(),
            [format!("apply({})", digest('b'))]
        );
        assert!(out.contains("Applied 45.20260925.118 (sha256:bbbbbbbbbbbb)"));
        assert!(out.contains("Reboot to finish the update"));
    }

    #[test]
    fn apply_with_nothing_staged_is_nothing_to_do() {
        let mut s = status(State::UpToDate);
        s.staged = Deployment::default();
        let fake = Fake::new(s);
        let error = run(apply(&fake, None)).unwrap_err();
        assert_eq!(error.exit(), Exit::NothingToDo);
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn apply_validates_an_explicit_digest() {
        let fake = Fake::new(status(State::Staged));
        let error = run(apply(&fake, Some("sha256:XYZ"))).unwrap_err();
        assert!(matches!(error, ClientError::InvalidArgument(_)));
        assert_eq!(error.exit(), Exit::Error);
        let explicit = digest('d');
        let out = run(apply(&fake, Some(&explicit))).unwrap();
        assert!(out.contains("Applied the update (sha256:dddddddddddd)"));
    }

    #[test]
    fn gate_refusals_exit_2() {
        let fake = Fake::new(status(State::Available));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::PolicyDenied);
        let error = run(download(&fake, false)).unwrap_err();
        assert_eq!(error.exit(), Exit::PolicyDenied);
        assert_eq!(fake.calls.borrow().as_slice(), ["download(false)"]);
        let out = run(download(&fake, true)).unwrap();
        assert!(out.contains("Downloaded 45.20260925.118"));
    }

    #[test]
    fn daemon_nothing_to_do_exits_3() {
        let fake = Fake::new(status(State::Staged));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::NothingToDo);
        assert_eq!(run(discard(&fake)).unwrap_err().exit(), Exit::NothingToDo);
    }

    #[test]
    fn rollback_names_both_versions() {
        let fake = Fake::new(status(State::UpToDate));
        let out = run(rollback(&fake, true)).unwrap();
        assert!(out.contains("45.20260920.110 will start at the next boot."));
        assert!(out.contains("sha256:aaaaaaaaaaaa is marked bad"));
        assert_eq!(fake.calls.borrow().as_slice(), ["rollback(true)"]);
    }

    #[test]
    fn check_reports_the_available_update() {
        let mut s = status(State::Available);
        s.available = Deployment {
            digest: Some(digest('e')),
            version: Some("45.20260926.120".into()),
            ..Deployment::default()
        };
        let out = run(check(&Fake::new(s))).unwrap();
        assert!(out.starts_with("An update is available: 45.20260926.120 (sha256:eeeeeeeeeeee)."));
        let out = run(check(&Fake::new(status(State::UpToDate)))).unwrap();
        assert_eq!(out, "Apex is up to date (45.20260924.117).\n");
    }

    #[test]
    fn changes_without_a_stage_is_nothing_to_do() {
        let mut s = status(State::UpToDate);
        s.staged = Deployment::default();
        let error = run(changes(&Fake::new(s))).unwrap_err();
        assert_eq!(error.exit(), Exit::NothingToDo);
        let mut fake = Fake::new(status(State::Staged));
        fake.changes = vec![
            (
                "bash".into(),
                "x86_64".into(),
                "upgraded".into(),
                "5.3.0-1".into(),
                "5.3.0-2".into(),
            ),
            (
                "glibc".into(),
                "i686".into(),
                "upgraded".into(),
                "2.42-3".into(),
                "2.42-4".into(),
            ),
        ];
        let out = run(changes(&fake)).unwrap();
        assert!(
            out.contains("  upgraded    bash        5.3.0-1 → 5.3.0-2\n"),
            "{out}"
        );
        assert!(
            out.contains("  upgraded    glibc.i686  2.42-3 → 2.42-4\n"),
            "{out}"
        );
    }

    #[test]
    fn acknowledge_clears_a_recorded_failure() {
        // The failed update is the running one: rolling back is offered as the alternative.
        let mut s = status(State::HealthFailure);
        s.health_failure = digest('a');
        let fake = Fake::new(s);
        let out = run(acknowledge(&fake)).unwrap();
        assert_eq!(fake.calls.borrow().as_slice(), ["acknowledge"]);
        assert!(
            out.starts_with(
                "Acknowledged: update sha256:aaaaaaaaaaaa failed its health checks. The notice \
                 is cleared; nothing was rolled back.\n"
            ),
            "{out}"
        );
        assert!(out.contains("apexctl update rollback --mark-bad"), "{out}");

        // The failed update is no longer booted: --mark-bad would denylist the wrong image.
        let mut s = status(State::UpToDate);
        s.health_failure = digest('f');
        let out = run(acknowledge(&Fake::new(s))).unwrap();
        assert!(out.contains("sha256:ffffffffffff"), "{out}");
        assert!(!out.contains("rollback"), "{out}");
    }

    #[test]
    fn acknowledge_without_a_failure_is_nothing_to_do() {
        let fake = Fake::new(status(State::UpToDate));
        let error = run(acknowledge(&fake)).unwrap_err();
        assert_eq!(error.exit(), Exit::NothingToDo);
        assert!(error.to_string().contains("nothing to acknowledge"));
        assert!(fake.calls.borrow().is_empty(), "the daemon is not called");
    }

    #[test]
    fn acknowledge_reports_a_refusal() {
        let mut s = status(State::HealthFailure);
        s.health_failure = digest('a');
        let fake = Fake::new(s);
        *fake.fail_with.borrow_mut() = Some(ErrorKind::NotAuthorized);
        let error = run(acknowledge(&fake)).unwrap_err();
        assert_eq!(error.exit(), Exit::Error);
        assert!(error.to_string().starts_with("not authorized: "));
    }

    #[test]
    fn channel_switch_passes_the_typed_channel_and_names_the_stage() {
        let fake = Fake::new(status(State::Staged));
        let out = run(channel(&fake, Channel::Testing)).unwrap();
        assert_eq!(fake.calls.borrow().as_slice(), ["switch_channel(testing)"]);
        assert_eq!(
            out,
            "Apex now follows the testing channel. Its image 45.20260925.118 \
             (sha256:bbbbbbbbbbbb) is downloaded and stays locked until you apply it.\n\
             To finish the switch, run: apexctl update apply\n\
             Then restart the computer; apexctl never restarts it by itself.\n\
             Testing images are signed with the CI key and have not been qualified for stable.\n"
        );

        let fake = Fake::new(status(State::Staged));
        let out = run(channel(&fake, Channel::Stable)).unwrap();
        assert_eq!(fake.calls.borrow().as_slice(), ["switch_channel(stable)"]);
        assert!(out.starts_with("Apex now follows the stable channel. Its image"));
        assert!(!out.contains("CI key"), "{out}");

        let mut s = status(State::UpToDate);
        s.staged = Deployment::default();
        let out = run(channel(&Fake::new(s), Channel::Stable)).unwrap();
        assert_eq!(
            out,
            "Apex now follows the stable channel. Apex is up to date.\n"
        );
    }

    #[test]
    fn a_channel_switch_to_the_running_build_changes_only_the_channel() {
        // The stable channel offers the build that runs now (the testing build was
        // promoted): applying the stage arms nothing (M1 contract §5).
        let mut s = status(State::Staged);
        s.booted.image = Some("ghcr.io/sekirokenjii/apex-testing:45".to_owned());
        s.staged = Deployment {
            image: Some("ghcr.io/sekirokenjii/apex:45".to_owned()),
            download_only: Some(true),
            ..s.booted.clone()
        };
        let out = run(channel(&Fake::new(s.clone()), Channel::Stable)).unwrap();
        assert_eq!(
            out,
            "Apex now follows the stable channel. Its image 45.20260924.117 \
             (sha256:aaaaaaaaaaaa) is downloaded and stays locked until you apply it.\n\
             It is the build that is running now, so only the channel changes: the same build \
             starts again, and no boot check is armed for that restart.\n\
             To finish the switch, run: apexctl update apply\n\
             Then restart the computer; apexctl never restarts it by itself.\n"
        );
        // `apply` says the same.
        let out = run(apply(&Fake::new(s), None)).unwrap();
        assert_eq!(
            out,
            "Applied 45.20260924.117 (sha256:aaaaaaaaaaaa). It will be used at the next boot.\n\
             It comes from the stable channel.\n\
             It is the build that is running now, so only the channel changes: the same build \
             starts again, and no boot check is armed for that restart.\n\
             Reboot to finish the update. apexctl never reboots the computer by itself.\n"
        );
    }

    #[test]
    fn a_channel_switch_to_an_older_build_says_apply_installs_it_anyway() {
        let mut s = status(State::Staged);
        s.staged = testing_image("45.20260923.5", 'f');
        let out = run(channel(&Fake::new(s.clone()), Channel::Testing)).unwrap();
        assert!(
            out.contains(
                "\nIt is older than the running 45.20260924.117; applying it installs it anyway, \
                 since you chose this channel.\nTo finish the switch, run: apexctl update apply\n"
            ),
            "{out}"
        );
        assert!(!out.contains("no boot check"), "{out}");
        // A newer build gets neither note.
        s.staged = testing_image("45.20260925.7", 'f');
        let out = run(channel(&Fake::new(s), Channel::Testing)).unwrap();
        assert!(!out.contains("older"), "{out}");
        assert!(!out.contains("no boot check"), "{out}");
    }

    #[test]
    fn channel_switch_exit_codes() {
        for (kind, exit) in [
            (ErrorKind::NothingToDo, Exit::NothingToDo),
            (ErrorKind::PolicyDenied, Exit::PolicyDenied),
            (ErrorKind::NotAuthorized, Exit::Error),
            (ErrorKind::Busy, Exit::Error),
            (ErrorKind::Network, Exit::Error),
        ] {
            let fake = Fake::new(status(State::UpToDate));
            *fake.fail_with.borrow_mut() = Some(kind);
            let error = run(channel(&fake, Channel::Stable)).unwrap_err();
            assert_eq!(error.exit(), exit, "{kind:?}");
        }
    }

    #[test]
    fn the_force_hint_follows_only_a_plain_download() {
        let denied = ClientError::Daemon(ApexError::new(ErrorKind::PolicyDenied, "no space"));
        let force_hint = denied.hint();
        assert!(force_hint.is_some_and(|h| h.contains("--force")));
        assert_eq!(
            hint(&UpdateCommand::Download { force: false }, &denied),
            force_hint
        );
        for command in [
            UpdateCommand::Download { force: true },
            UpdateCommand::Channel {
                channel: Channel::Testing,
            },
            UpdateCommand::Apply { digest: None },
            UpdateCommand::Acknowledge,
            UpdateCommand::Automatic,
        ] {
            assert_eq!(hint(&command, &denied), None, "{command:?}");
        }
        // Other hints are unaffected.
        let busy = ClientError::Daemon(ApexError::new(ErrorKind::Busy, "downloading"));
        assert_eq!(
            hint(
                &UpdateCommand::Channel {
                    channel: Channel::Stable
                },
                &busy
            ),
            busy.hint()
        );
        assert!(busy.hint().is_some());
    }

    fn testing_image(version: &str, c: char) -> Deployment {
        Deployment {
            image: Some("ghcr.io/sekirokenjii/apex-testing:45".to_owned()),
            digest: Some(digest(c)),
            version: Some(version.to_owned()),
            download_only: Some(true),
            ..Deployment::default()
        }
    }

    fn stable_booted(mut status: ServiceStatus) -> ServiceStatus {
        status.booted.image = Some("ghcr.io/sekirokenjii/apex:45".to_owned());
        status
    }

    #[test]
    fn discard_says_when_the_same_update_is_locked_again() {
        let applied = ServiceStatus {
            staged: Deployment {
                download_only: Some(false),
                ..status(State::Staged).staged
            },
            ..status(State::ReadyForReboot)
        };
        let fake = Fake::changing(applied, status(State::Staged));
        let out = run(discard(&fake)).unwrap();
        assert_eq!(fake.calls.borrow().as_slice(), ["discard"]);
        // Present tense only, and the exact build to apply: the timer may replace a locked
        // stage with a newer build, so nothing says that this one stays downloaded.
        assert_eq!(
            out,
            format!(
                "45.20260925.118 will not be installed at the next restart.\n\
                 The update channel still offers it, so it is downloaded and locked:\n\
                 \x20 45.20260925.118  {d}\n\
                 To install exactly this build later: apexctl update apply --digest {d}\n\
                 An automatic download may replace it with a newer build before then.\n",
                d = digest('b')
            )
        );
        assert!(!out.contains("stays downloaded"), "{out}");
        assert!(!out.contains("apexctl update changes"), "{out}");
    }

    #[test]
    fn discard_never_calls_an_unknown_digest_the_same_update() {
        // Neither status names a digest: the output cannot claim the same build is locked.
        let mut applied = status(State::ReadyForReboot);
        applied.staged.download_only = Some(false);
        applied.staged.digest = None;
        let mut after = status(State::Staged);
        after.staged.digest = None;
        let out = run(discard(&Fake::changing(applied, after))).unwrap();
        assert!(
            out.contains(
                "\nThe update downloaded now is locked:\n  45.20260925.118  unknown digest\n"
            ),
            "{out}"
        );
        assert!(!out.contains("moved on"), "{out}");
        assert!(
            out.contains("Review it with: apexctl update changes\n"),
            "{out}"
        );
        assert!(
            out.contains("To install it later, check apexctl status, then: apexctl update apply\n"),
            "{out}"
        );
        assert!(!out.contains("still offers it"), "{out}");
    }

    #[test]
    fn discard_names_a_newer_build_that_replaced_the_applied_one() {
        let mut applied = status(State::ReadyForReboot);
        applied.staged.download_only = Some(false);
        let mut replaced = status(State::Staged);
        replaced.staged = Deployment {
            digest: Some(digest('e')),
            version: Some("45.20260926.119".to_owned()),
            download_only: Some(true),
            ..Deployment::default()
        };
        let out = run(discard(&Fake::changing(applied.clone(), replaced))).unwrap();
        assert_eq!(
            out,
            format!(
                "45.20260925.118 will not be installed at the next restart.\n\
                 The update channel has moved on: cancelling downloaded its current build, which \
                 replaced it and is locked:\n\
                 \x20 45.20260926.119  {d}\n\
                 Review it with: apexctl update changes\n\
                 To install exactly this build later: apexctl update apply --digest {d}\n\
                 An automatic download may replace it with a newer build before then.\n",
                d = digest('e')
            )
        );

        let mut gone = status(State::UpToDate);
        gone.staged = Deployment::default();
        let out = run(discard(&Fake::changing(applied, gone))).unwrap();
        assert_eq!(
            out,
            "45.20260925.118 will not be installed at the next restart.\n\
             No update is downloaded now. Download one again with: apexctl update download\n"
        );
    }

    #[test]
    fn a_failed_discard_says_whether_the_update_still_installs() {
        let error = |kind| ClientError::Daemon(ApexError::new(kind, "m"));
        let network = hint(&UpdateCommand::Discard, &error(ErrorKind::Network)).unwrap();
        assert!(
            network.starts_with("The update is still set to install at the next restart."),
            "{network}"
        );
        assert!(network.contains("needs the network"));
        for kind in [ErrorKind::Bootc, ErrorKind::Internal] {
            // bootc failed, or the service could not confirm what it did.
            assert_eq!(
                hint(&UpdateCommand::Discard, &error(kind)),
                Some(DISCARD_STATUS_HINT),
                "{kind:?}"
            );
        }
        assert!(DISCARD_STATUS_HINT.contains("apexctl status"));
        // Other commands keep the generic hints (none for Network).
        assert_eq!(
            hint(&UpdateCommand::Check, &error(ErrorKind::Network)),
            None
        );
        // The fake: the call fails and the output is only the error.
        let fake = Fake::new(status(State::ReadyForReboot));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::Network);
        let failed = run(discard(&fake)).unwrap_err();
        assert_eq!(failed.exit(), Exit::Error);
        assert_eq!(
            hint(&UpdateCommand::Discard, &failed),
            Some(DISCARD_NETWORK_HINT)
        );
    }

    #[test]
    fn automatic_busy_or_offline_is_a_deferral_not_a_failed_unit() {
        for (kind, why) in [
            (ErrorKind::Busy, "another update operation is running"),
            (ErrorKind::Network, "the image registry cannot be reached"),
        ] {
            let fake = Fake::new(status(State::Available));
            *fake.fail_with.borrow_mut() = Some(kind);
            let out = run(automatic(&fake)).unwrap();
            assert_eq!(
                out,
                format!(
                    "Automatic update run skipped: {why} (refused). The next timer run tries \
                     again.\n"
                )
            );
        }
        // Real failures still fail the unit.
        for kind in [
            ErrorKind::Bootc,
            ErrorKind::Internal,
            ErrorKind::NotAuthorized,
        ] {
            let fake = Fake::new(status(State::Available));
            *fake.fail_with.borrow_mut() = Some(kind);
            assert_eq!(
                run(automatic(&fake)).unwrap_err().exit(),
                Exit::Error,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn automatic_needs_root_not_the_desktop_session() {
        let refused = ClientError::Daemon(ApexError::new(
            ErrorKind::NotAuthorized,
            "Only the system can call this method (caller uid 1000).",
        ));
        let text = hint(&UpdateCommand::Automatic, &refused).unwrap();
        assert!(text.contains("sudo apexctl update automatic"), "{text}");
        assert!(!text.contains("desktop session"), "{text}");
        // Every other command keeps the polkit advice.
        let other = hint(&UpdateCommand::Download { force: false }, &refused).unwrap();
        assert!(other.contains("desktop session"), "{other}");
    }

    #[test]
    fn check_names_a_newer_build_than_the_staged_one() {
        let mut s = status(State::Staged);
        s.available = Deployment {
            digest: Some(digest('e')),
            version: Some("45.20260926.120".into()),
            ..Deployment::default()
        };
        let out = run(check(&Fake::new(s.clone()))).unwrap();
        assert_eq!(
            out,
            "An update is downloaded and waiting.\n\
             A newer update is available: 45.20260926.120 (sha256:eeeeeeeeeeee).\n\
             Download it in place of 45.20260925.118 with: apexctl update download\n"
        );
        // Applied already: a download is refused until the applied update is cancelled.
        s.state = State::ReadyForReboot;
        s.staged.download_only = Some(false);
        let out = run(check(&Fake::new(s))).unwrap();
        assert!(
            out.ends_with(
                "To install it instead, first cancel the applied update: apexctl update discard\n"
            ),
            "{out}"
        );
        // Nothing newer: the usual next step.
        let out = run(check(&Fake::new(status(State::Staged)))).unwrap();
        assert!(out.ends_with("Use it at the next restart with: apexctl update apply\n"));
    }

    #[test]
    fn rollback_while_a_rollback_is_queued_explains_how_to_keep_the_running_version() {
        // The user started the older version from the boot menu: bootc's "rollback"
        // deployment is the newer one, which the daemon will not queue again.
        let mut s = status(State::RollbackQueued);
        s.booted = Deployment {
            digest: Some(digest('c')),
            version: Some("45.20260920.110".into()),
            ..Deployment::default()
        };
        s.rollback = Deployment {
            digest: Some(digest('a')),
            version: Some("45.20260924.117".into()),
            ..Deployment::default()
        };
        s.staged = Deployment::default();
        let fake = Fake::new(s);
        *fake.fail_with.borrow_mut() = Some(ErrorKind::NothingToDo);
        let error = run(rollback(&fake, false)).unwrap_err();
        assert_eq!(error.exit(), Exit::NothingToDo);
        let text = error.to_string();
        assert!(
            text.starts_with(
                "The running 45.20260920.110 is not the default: the newer version \
                 45.20260924.117 will start at the next boot (rollback queued).\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("\napexctl update rollback cannot undo a queued rollback.\n"),
            "{text}"
        );
        // The newer version may still be armed, which the service does not publish: the
        // safe path (RECOVERY.md §1), not a bare `bootc rollback`.
        assert!(
            text.contains(
                "To keep running 45.20260920.110, first check whether 45.20260924.117 is an \
                 update whose boot check is still running (apex_target is set):\n\
                 sudo grub2-editenv /boot/grub2/grubenv list\n\
                 Then follow docs/RECOVERY.md §1"
            ),
            "{text}"
        );
        assert!(!text.contains("bootc rollback"), "{text}");
        // Any other refusal is passed through unchanged.
        let fake = Fake::new(status(State::UpToDate));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::NothingToDo);
        assert_eq!(
            run(rollback(&fake, false)).unwrap_err().to_string(),
            "refused"
        );
    }

    #[test]
    fn rollback_after_a_rollback_says_a_restart_finishes_it() {
        // `apexctl update rollback` ran already: the older version is queued.
        let mut s = status(State::RollbackQueued);
        s.staged = Deployment::default();
        let fake = Fake::new(s);
        *fake.fail_with.borrow_mut() = Some(ErrorKind::NothingToDo);
        let error = run(rollback(&fake, true)).unwrap_err();
        assert_eq!(error.exit(), Exit::NothingToDo);
        assert_eq!(
            error.to_string(),
            "The previous version 45.20260920.110 will start at the next boot (rollback queued).\n\
             Reboot to finish the rollback. Automatic updates are paused until then."
        );
    }

    #[test]
    fn check_while_the_service_is_busy_exits_0_and_says_nothing_was_checked() {
        // Check is Busy while a download runs: the download fetches from the channel.
        let mut downloading = status(State::Downloading);
        downloading.staged = Deployment::default();
        downloading.progress = 0.42;
        let fake = Fake::new(downloading);
        *fake.fail_with.borrow_mut() = Some(ErrorKind::Busy);
        let out = run(check(&fake)).unwrap();
        assert_eq!(fake.calls.borrow().as_slice(), ["check"]);
        assert_eq!(
            out,
            "Not checked: an update is being downloaded now.\n\
             Downloading  42 %\n\
             Check again when the download has finished; apexctl status shows its progress.\n"
        );
        // Busy for another reason (a discard re-locking, a full queue): the service's
        // reason, still exit 0.
        let fake = Fake::new(status(State::ReadyForReboot));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::Busy);
        let out = run(check(&fake)).unwrap();
        assert_eq!(
            out,
            "Not checked: the update service is busy. refused\n\
             apexctl status shows what it is doing.\n"
        );
        // Other failures are still failures.
        for kind in [
            ErrorKind::Network,
            ErrorKind::Internal,
            ErrorKind::NotAuthorized,
        ] {
            let fake = Fake::new(status(State::UpToDate));
            *fake.fail_with.borrow_mut() = Some(kind);
            assert_eq!(
                run(check(&fake)).unwrap_err().exit(),
                Exit::Error,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn changes_before_the_state_is_known_fail_gracefully() {
        // GetChanges fails with Busy until the service's first reading of the system, and
        // with Internal when bootc cannot be used.
        let mut unknown = status(State::Unknown);
        unknown.staged = Deployment::default();
        let fake = Fake::new(unknown.clone());
        *fake.fail_with.borrow_mut() = Some(ErrorKind::Busy);
        let error = run(changes(&fake)).unwrap_err();
        assert!(matches!(error, ClientError::NotReady(_)), "{error:?}");
        assert_eq!(error.exit(), Exit::Error);
        assert_eq!(
            error.to_string(),
            "Cannot list the package changes now. refused"
        );
        assert!(!error.to_string().contains("another update operation"));
        assert_eq!(hint(&UpdateCommand::Changes, &error), None);

        let fake = Fake::new(unknown);
        *fake.fail_with.borrow_mut() = Some(ErrorKind::Internal);
        let error = run(changes(&fake)).unwrap_err();
        assert_eq!(error.exit(), Exit::Error);
        assert_eq!(
            error.to_string(),
            "internal error in apex-updated: cannot list the package changes: refused"
        );
        assert!(
            hint(&UpdateCommand::Changes, &error)
                .is_some_and(|h| h.contains("journalctl -u apex-updated")),
            "{error}"
        );
        // Other failures keep their own text.
        let fake = Fake::new(status(State::Staged));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::NotAuthorized);
        let error = run(changes(&fake)).unwrap_err();
        assert_eq!(error.to_string(), "not authorized: refused");
    }

    #[test]
    fn changes_name_the_staged_digest_and_how_to_apply_exactly_it() {
        let mut fake = Fake::new(status(State::Staged));
        fake.changes = vec![(
            "bash".into(),
            "x86_64".into(),
            "upgraded".into(),
            "5.3.0-1".into(),
            "5.3.0-2".into(),
        )];
        let out = run(changes(&fake)).unwrap();
        assert_eq!(
            out,
            format!(
                "Changes from 45.20260924.117 to 45.20260925.118 ({d})\n\
                 \n\
                 1 package changed\n\
                 \n  upgraded    bash  5.3.0-1 → 5.3.0-2\n\
                 \n\
                 Apply exactly this update with: apexctl update apply --digest {d}\n",
                d = digest('b')
            )
        );
        // Already applied: nothing to apply.
        let mut applied = status(State::ReadyForReboot);
        applied.staged.download_only = Some(false);
        let out = run(changes(&Fake::new(applied))).unwrap();
        assert!(
            out.ends_with("This update is already set to install at the next restart.\n"),
            "{out}"
        );
        // A testing build staged under a stable system says so.
        let mut testing = stable_booted(status(State::Staged));
        testing.staged = testing_image("45.20260925.7", 'f');
        let out = run(changes(&Fake::new(testing))).unwrap();
        assert!(
            out.contains("It comes from the testing channel (signed with the CI key"),
            "{out}"
        );
    }

    #[test]
    fn changes_refuse_a_list_that_raced_with_a_restage() {
        // The timer replaced the stage while GetChanges ran.
        let mut restaged = status(State::Staged);
        restaged.staged.digest = Some(digest('e'));
        let fake = Fake::changing(status(State::Staged), restaged);
        let error = run(changes(&fake)).unwrap_err();
        assert!(matches!(error, ClientError::Changed(_)));
        assert_eq!(error.exit(), Exit::Error);
        assert!(error.to_string().contains("apexctl update changes again"));
    }

    #[test]
    fn apply_and_download_name_another_channel() {
        let mut s = stable_booted(status(State::Staged));
        s.staged = testing_image("45.20260925.7", 'f');
        let out = run(apply(&Fake::new(s.clone()), None)).unwrap();
        assert_eq!(
            out,
            "Applied 45.20260925.7 (sha256:ffffffffffff). It will be used at the next boot.\n\
             It comes from the testing channel (signed with the CI key, not qualified for \
             stable).\n\
             Reboot to finish the update. apexctl never reboots the computer by itself.\n"
        );
        let out = run(download(&Fake::new(s), false)).unwrap();
        assert!(out.contains("It comes from the testing channel"), "{out}");
        // Same repository: no note.
        let out = run(apply(
            &Fake::new(stable_booted(status(State::Staged))),
            None,
        ))
        .unwrap();
        assert!(!out.contains("comes from"), "{out}");
    }

    #[test]
    fn automatic_reports_a_deferral() {
        let mut s = status(State::Available);
        s.deferred_reason = "On battery below 50 %".into();
        let out = run(automatic(&Fake::new(s))).unwrap();
        assert!(out.contains("Download deferred: On battery below 50 %"));
    }
}
