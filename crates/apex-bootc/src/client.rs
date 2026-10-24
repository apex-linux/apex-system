use tokio::sync::mpsc;

use crate::check::{CheckOutcome, parse_check_output};
use crate::command::{BootcCommand, ImageRef};
use crate::download::{DownloadOutcome, parse_download_output};
use crate::error::{BootcError, summarize_stderr};
use crate::ostree_status::{OstreeDeployment, parse_ostree_status};
use crate::progress::Progress;
use crate::runner::{CancelToken, CommandOutput, CommandRunner, Invocation};
use crate::status::Host;

#[derive(Debug, Clone, Default)]
pub struct Bootc<R> {
    runner: R,
}

impl<R: CommandRunner> Bootc<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }

    pub async fn run(&self, invocation: Invocation) -> Result<CommandOutput, BootcError> {
        let kind = invocation.command.kind();
        self.runner.run(invocation).await?.into_result(kind)
    }

    /// A host bootc does not manage is no error here (bootc exits 0 with an empty status):
    /// see [`Host::require_bootc_host`].
    pub async fn status(&self, cancel: &CancelToken) -> Result<Host, BootcError> {
        let output = self.run(invoke(BootcCommand::Status, cancel)).await?;
        Ok(Host::from_json(&output.stdout)?)
    }

    pub async fn check(&self, cancel: &CancelToken) -> Result<CheckOutcome, BootcError> {
        let output = self.run(invoke(BootcCommand::UpgradeCheck, cancel)).await?;
        Ok(parse_check_output(&output.stdout_lossy()))
    }

    /// Needs the registry: bootc pulls the channel tag again. A caller that needs the stage
    /// locked must check [`DownloadOutcome::stage_locked`], then confirm with [`Self::status`].
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
                    // A usage error: bootc did nothing, so retrying cannot download twice.
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

    /// Never use it to re-lock an applied stage: `Ok(())` does not mean the stage is locked.
    pub async fn download(
        &self,
        progress: Option<mpsc::Sender<Progress>>,
        cancel: &CancelToken,
    ) -> Result<(), BootcError> {
        self.upgrade_download_only(progress, cancel).await.map(drop)
    }

    pub async fn apply_downloaded(&self, cancel: &CancelToken) -> Result<(), BootcError> {
        self.run(invoke(BootcCommand::UpgradeFromDownloaded, cancel))
            .await
            .map(drop)
    }

    pub async fn rollback(&self, cancel: &CancelToken) -> Result<(), BootcError> {
        self.run(invoke(BootcCommand::Rollback, cancel))
            .await
            .map(drop)
    }

    pub async fn pin_rollback(&self, cancel: &CancelToken) -> Result<(), BootcError> {
        self.run(invoke(BootcCommand::PinRollback, cancel))
            .await
            .map(drop)
    }

    pub async fn ostree_status(
        &self,
        cancel: &CancelToken,
    ) -> Result<Vec<OstreeDeployment>, BootcError> {
        let output = self.run(invoke(BootcCommand::OstreeStatus, cancel)).await?;
        Ok(parse_ostree_status(&output.stdout_lossy()))
    }

    /// Only with an index just read by [`Self::ostree_status`].
    pub async fn unpin(&self, index: u32, cancel: &CancelToken) -> Result<(), BootcError> {
        self.run(invoke(BootcCommand::Unpin { index }, cancel))
            .await
            .map(drop)
    }

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
