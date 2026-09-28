//! [`Bootc`]: one method per bootc operation, with failures classified.

use tokio::sync::mpsc;

use crate::check::{CheckOutcome, parse_check_output};
use crate::command::{BootcCommand, ImageRef};
use crate::download::{DownloadOutcome, parse_download_output};
use crate::error::{BootcError, summarize_stderr};
use crate::progress::Progress;
use crate::runner::{CancelToken, CommandOutput, CommandRunner, Invocation};
use crate::status::Host;

/// Typed bootc operations over a [`CommandRunner`].
///
/// Every method returns `Err` for any failure: runner errors unchanged, and non-zero
/// exits classified by [`crate::classify_failure`].
#[derive(Debug, Clone, Default)]
pub struct Bootc<R> {
    runner: R,
}

impl<R: CommandRunner> Bootc<R> {
    /// Wraps a runner.
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// The wrapped runner.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// Runs an invocation and classifies a non-zero exit.
    pub async fn run(&self, invocation: Invocation) -> Result<CommandOutput, BootcError> {
        let kind = invocation.command.kind();
        self.runner.run(invocation).await?.into_result(kind)
    }

    /// `bootc status --format=json --format-version=1`, parsed.
    ///
    /// A host bootc does not manage is **not** an error here (bootc exits 0 with an
    /// empty status); use [`Host::require_bootc_host`] or [`Host::is_bootc_host`].
    pub async fn status(&self, cancel: &CancelToken) -> Result<Host, BootcError> {
        let output = self.run(invoke(BootcCommand::Status, cancel)).await?;
        Ok(Host::from_json(&output.stdout)?)
    }

    /// `bootc upgrade --check`: fetches the manifest and config only.
    pub async fn check(&self, cancel: &CancelToken) -> Result<CheckOutcome, BootcError> {
        let output = self.run(invoke(BootcCommand::UpgradeCheck, cancel)).await?;
        Ok(parse_check_output(&output.stdout_lossy()))
    }

    /// `bootc upgrade --download-only`, with `--progress-fd 3` when `progress` is given,
    /// and what bootc reports it did.
    ///
    /// bootc always pulls the channel tag, so this needs the registry. Then it stages
    /// that image locked, re-locks or keeps the stage when the tag still points at the
    /// staged image, or changes nothing when the tag points at the booted image, in which
    /// case an applied stage stays applied (see [`DownloadOutcome`]). A caller that needs
    /// the stage locked afterwards must check [`DownloadOutcome::stage_locked`] and then
    /// confirm with [`Self::status`].
    ///
    /// If the installed bootc rejects `--progress-fd` (a clap usage error, so nothing
    /// was done), the download is retried once without progress.
    pub async fn upgrade_download_only(
        &self,
        progress: Option<mpsc::Sender<Progress>>,
        cancel: &CancelToken,
    ) -> Result<DownloadOutcome, BootcError> {
        let output = match progress {
            None => {
                let command = BootcCommand::UpgradeDownloadOnly { progress: false };
                self.run(invoke(command, cancel)).await?
            }
            Some(sink) => {
                let command = BootcCommand::UpgradeDownloadOnly { progress: true };
                match self.run(invoke(command, cancel).with_progress(sink)).await {
                    Err(BootcError::Unsupported { detail, .. }) => {
                        tracing::warn!(%detail, "bootc rejected --progress-fd; downloading without progress");
                        let command = BootcCommand::UpgradeDownloadOnly { progress: false };
                        self.run(invoke(command, cancel)).await?
                    }
                    other => other?,
                }
            }
        };
        let stdout = output.stdout_lossy();
        let outcome = parse_download_output(&stdout);
        if outcome == DownloadOutcome::Unknown {
            tracing::warn!(
                last_line = %summarize_stderr(&stdout),
                "bootc upgrade --download-only succeeded, but its output was not recognised"
            );
        }
        Ok(outcome)
    }

    /// [`Self::upgrade_download_only`] without the outcome, for a caller that reads the
    /// status again afterwards and does not care which case happened.
    ///
    /// Do not use it to re-lock an applied stage: `Ok(())` does not mean the stage is
    /// locked (see [`DownloadOutcome::NoUpdate`]).
    pub async fn download(
        &self,
        progress: Option<mpsc::Sender<Progress>>,
        cancel: &CancelToken,
    ) -> Result<(), BootcError> {
        self.upgrade_download_only(progress, cancel).await.map(drop)
    }

    /// `bootc upgrade --from-downloaded`: unlocks the staged deployment for the next boot.
    pub async fn apply_downloaded(&self, cancel: &CancelToken) -> Result<(), BootcError> {
        self.run(invoke(BootcCommand::UpgradeFromDownloaded, cancel))
            .await
            .map(drop)
    }

    /// `bootc rollback`: queues the rollback deployment for the next boot.
    pub async fn rollback(&self, cancel: &CancelToken) -> Result<(), BootcError> {
        self.run(invoke(BootcCommand::Rollback, cancel))
            .await
            .map(drop)
    }

    /// `bootc switch --enforce-container-sigpolicy --download-only <target>`.
    pub async fn switch_download_only(
        &self,
        target: ImageRef,
        cancel: &CancelToken,
    ) -> Result<(), BootcError> {
        self.run(invoke(BootcCommand::SwitchDownloadOnly { target }, cancel))
            .await
            .map(drop)
    }
}

fn invoke(command: BootcCommand, cancel: &CancelToken) -> Invocation {
    Invocation::new(command).with_cancel(cancel.clone())
}
