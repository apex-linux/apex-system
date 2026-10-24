use std::fmt::Write as _;

use apex_bootc::Digest;
use apex_dbus::{ApexError, Deployment, ErrorKind, State};

use crate::cli::UpdateCommand;
use crate::exit::Exit;
use crate::service::{ClientError, ServiceStatus, UpdateService};
use crate::status::{
    other_origin, progress_line, queued_rollback_goes_back, render_changes, short_digest,
    status_sentence,
};

const AUTOMATIC_ROOT_HINT: &str = "This command is for the system timer \
(apex-update-auto.service) and runs as root: sudo apexctl update automatic";

pub fn hint(command: &UpdateCommand, error: &ClientError) -> Option<&'static str> {
    // Root-only by uid, so the polkit advice of the generic hint would be wrong.
    if matches!(command, UpdateCommand::Automatic)
        && daemon_kind(error) == Some(ErrorKind::NotAuthorized)
    {
        return Some(AUTOMATIC_ROOT_HINT);
    }
    let force_advice_applies = matches!(command, UpdateCommand::Download { force: false });
    if error.exit() == Exit::PolicyDenied && !force_advice_applies {
        return None;
    }
    error.hint()
}

fn daemon_kind(error: &ClientError) -> Option<ErrorKind> {
    match error {
        ClientError::Daemon(error) => error.kind(),
        _ => None,
    }
}

fn digest_of(deployment: &Deployment) -> &str {
    deployment.digest.as_deref().unwrap_or("unknown digest")
}

fn origin_sentence(status: &ServiceStatus, deployment: &Deployment) -> Option<String> {
    other_origin(&status.booted, deployment).map(|origin| format!("It comes from {origin}."))
}

/// The `staged` and `ready-for-reboot` states rank before `available`, so the state alone
/// hides a newer build.
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
                            "Download it after the restart that installs {}.",
                            version_of(&status.staged)
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
    Ok(out)
}

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
    if staged_is_target && let Some(origin) = origin_sentence(&before, &before.staged) {
        let _ = writeln!(out, "{origin}");
    }
    out.push_str("Reboot to finish the update. apexctl never reboots the computer by itself.\n");
    Ok(out)
}

pub async fn rollback<S: UpdateService>(
    service: &S,
    mark_bad: bool,
) -> Result<String, ClientError> {
    let before = service.status().await?;
    if let Err(error) = service.rollback(mark_bad).await {
        // The daemon's refusal assumes the queued deployment is the older one.
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

/// The timer may replace a locked stage at any time, so the status is read on both sides of
/// `GetChanges`, and the exact digest is printed for `apply --digest`.
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
        return Err(ClientError::ChangedWhileReading(
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

/// `Busy` from `GetChanges` means the service has not read the system yet, not that another
/// operation runs.
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

/// `AcknowledgeHealthFailure` clears both notices: a failed health check and a fallback.
pub async fn acknowledge<S: UpdateService>(service: &S) -> Result<String, ClientError> {
    let before = service.status().await?;
    let failed = before.health_failure.as_str();
    let fell_back = before.last_fallback.as_str();
    if failed.is_empty() && fell_back.is_empty() {
        return Err(ClientError::NothingToDo(
            "No update has failed its health checks or failed to start, so there is nothing \
             to acknowledge."
                .to_owned(),
        ));
    }
    service.acknowledge_health_failure().await?;
    let mut out = String::new();
    if !fell_back.is_empty() {
        let _ = writeln!(
            out,
            "Acknowledged: update {} did not start and was set aside. The notice is cleared.",
            short_digest(fell_back)
        );
    }
    if failed.is_empty() {
        return Ok(out);
    }
    let _ = writeln!(
        out,
        "Acknowledged: update {} failed its health checks. The notice is cleared; nothing was \
         rolled back.",
        short_digest(failed)
    );
    let failed_update_is_running = before.booted.digest.as_deref() == Some(failed);
    if failed_update_is_running {
        let _ = writeln!(
            out,
            "It is still the running version. To go back to the previous version instead: \
             apexctl update rollback --mark-bad"
        );
    }
    Ok(out)
}

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

/// `Busy` and `Network` are deferrals like the daemon's own, so the timer unit does not fail.
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
    use apex_dbus::{ApexError, Change, Channel, Deployment, ErrorKind};
    use std::cell::RefCell;
    use std::future::{Future, ready};

    struct Fake {
        status: RefCell<ServiceStatus>,
        status_after_next_call: RefCell<Option<ServiceStatus>>,
        fail_with: RefCell<Option<ErrorKind>>,
        changes: Vec<Change>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(status: ServiceStatus) -> Self {
            Self {
                status: RefCell::new(status),
                status_after_next_call: RefCell::new(None),
                fail_with: RefCell::new(None),
                changes: Vec::new(),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn changing(before: ServiceStatus, after: ServiceStatus) -> Self {
            let fake = Self::new(before);
            *fake.status_after_next_call.borrow_mut() = Some(after);
            fake
        }

        fn call(&self, name: String) -> Result<(), ClientError> {
            self.calls.borrow_mut().push(name);
            if let Some(kind) = self.fail_with.borrow_mut().take() {
                return Err(ClientError::Daemon(ApexError::new(kind, "refused")));
            }
            if let Some(after) = self.status_after_next_call.borrow_mut().take() {
                *self.status.borrow_mut() = after;
            }
            Ok(())
        }
    }

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
        fn rollback(&self, mark_bad: bool) -> impl Future<Output = Result<(), ClientError>> {
            ready(self.call(format!("rollback({mark_bad})")))
        }
        fn changes(&self) -> impl Future<Output = Result<Vec<Change>, ClientError>> {
            ready(self.call("changes".into()).map(|()| self.changes.clone()))
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
            deferred_reason: String::new(),
            last_error: String::new(),
            health_failure: String::new(),
            last_fallback: String::new(),
            automatic_downloads: Some(true),
            kept: Vec::new(),
        }
    }

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
        assert_eq!(
            run(apply(&fake, None)).unwrap_err().exit(),
            Exit::NothingToDo
        );
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

        let mut s = status(State::UpToDate);
        s.health_failure = digest('f');
        let out = run(acknowledge(&Fake::new(s))).unwrap();
        assert!(out.contains("sha256:ffffffffffff"), "{out}");
        assert!(!out.contains("rollback"), "{out}");
    }

    #[test]
    fn acknowledge_says_when_it_also_clears_a_fallback() {
        let mut s = status(State::UpToDate);
        s.last_fallback = digest('e');
        let fake = Fake::new(s);
        let out = run(acknowledge(&fake)).unwrap();
        assert_eq!(fake.calls.borrow().as_slice(), ["acknowledge"]);
        assert_eq!(
            out,
            "Acknowledged: update sha256:eeeeeeeeeeee did not start and was set aside. The \
             notice is cleared.\n"
        );

        let mut s = status(State::HealthFailure);
        s.health_failure = digest('a');
        s.last_fallback = digest('e');
        let out = run(acknowledge(&Fake::new(s))).unwrap();
        assert!(
            out.starts_with("Acknowledged: update sha256:eeeeeeeeeeee did not start"),
            "{out}"
        );
        assert!(
            out.contains("update sha256:aaaaaaaaaaaa failed its health checks"),
            "{out}"
        );
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
            UpdateCommand::Apply { digest: None },
            UpdateCommand::Acknowledge,
            UpdateCommand::Automatic,
        ] {
            assert_eq!(hint(&command, &denied), None, "{command:?}");
        }
        let busy = ClientError::Daemon(ApexError::new(ErrorKind::Busy, "downloading"));
        assert_eq!(hint(&UpdateCommand::Check, &busy), busy.hint());
        assert!(busy.hint().is_some());
    }

    fn testing_image(version: &str, c: char) -> Deployment {
        Deployment {
            image: Some("ghcr.io/apex-linux/apex-testing:45".to_owned()),
            digest: Some(digest(c)),
            version: Some(version.to_owned()),
            download_only: Some(true),
            ..Deployment::default()
        }
    }

    fn stable_booted(mut status: ServiceStatus) -> ServiceStatus {
        status.booted.image = Some("ghcr.io/apex-linux/apex:45".to_owned());
        status
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
        s.state = State::ReadyForReboot;
        s.staged.download_only = Some(false);
        let out = run(check(&Fake::new(s))).unwrap();
        assert!(
            out.ends_with("Download it after the restart that installs 45.20260925.118.\n"),
            "{out}"
        );
        let out = run(check(&Fake::new(status(State::Staged)))).unwrap();
        assert!(out.ends_with("Use it at the next restart with: apexctl update apply\n"));
    }

    #[test]
    fn rollback_while_a_rollback_is_queued_explains_how_to_keep_the_running_version() {
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
        let fake = Fake::new(status(State::UpToDate));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::NothingToDo);
        assert_eq!(
            run(rollback(&fake, false)).unwrap_err().to_string(),
            "refused"
        );
    }

    #[test]
    fn rollback_after_a_rollback_says_a_restart_finishes_it() {
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
        let fake = Fake::new(status(State::ReadyForReboot));
        *fake.fail_with.borrow_mut() = Some(ErrorKind::Busy);
        let out = run(check(&fake)).unwrap();
        assert_eq!(
            out,
            "Not checked: the update service is busy. refused\n\
             apexctl status shows what it is doing.\n"
        );
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
        let mut applied = status(State::ReadyForReboot);
        applied.staged.download_only = Some(false);
        let out = run(changes(&Fake::new(applied))).unwrap();
        assert!(
            out.ends_with("This update is already set to install at the next restart.\n"),
            "{out}"
        );
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
        let mut restaged = status(State::Staged);
        restaged.staged.digest = Some(digest('e'));
        let fake = Fake::changing(status(State::Staged), restaged);
        let error = run(changes(&fake)).unwrap_err();
        assert!(matches!(error, ClientError::ChangedWhileReading(_)));
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
