use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::command::BootcCommand;
use crate::error::{BootcError, ExitState};
use crate::progress::ProgressDecoder;
use crate::runner::{CommandOutput, CommandRunner, Invocation, forward};

#[derive(Debug)]
pub struct Scripted {
    expect: BootcCommand,
    steps: Vec<Step>,
    output: CommandOutput,
    error: Option<BootcError>,
}

#[derive(Debug)]
enum Step {
    Sleep(Duration),
    ProgressLine(String),
}

impl Scripted {
    /// By default the command exits 0 at once with empty output.
    pub fn new(expect: BootcCommand) -> Self {
        Self {
            expect,
            steps: Vec::new(),
            output: CommandOutput::new(ExitState::SUCCESS, "", ""),
            error: None,
        }
    }

    /// Replaces a scripted error with an exit.
    #[must_use]
    pub fn stdout(mut self, stdout: impl Into<Vec<u8>>) -> Self {
        self.exit_output().stdout = stdout.into();
        self
    }

    /// Replaces a scripted error with an exit.
    #[must_use]
    pub fn stderr(mut self, stderr: impl Into<Vec<u8>>) -> Self {
        self.exit_output().stderr = stderr.into();
        self
    }

    #[must_use]
    pub fn exit_code(mut self, code: i32) -> Self {
        self.exit_output().exit = ExitState::Code(code);
        self
    }

    #[must_use]
    pub fn killed_by(mut self, signal: i32) -> Self {
        self.exit_output().exit = ExitState::Signal(signal);
        self
    }

    #[must_use]
    pub fn error(mut self, error: BootcError) -> Self {
        self.error = Some(error);
        self
    }

    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.steps.push(Step::Sleep(delay));
        self
    }

    /// Delivered only to commands that use the progress fd, as with real bootc.
    #[must_use]
    pub fn progress_line(mut self, line: impl Into<String>) -> Self {
        self.steps.push(Step::ProgressLine(line.into()));
        self
    }

    #[must_use]
    pub fn progress_bytes(self, done: u64, total: u64) -> Self {
        self.progress_line(format!(
            r#"{{"bytes":{done},"bytesCached":0,"bytesTotal":{total},"description":"Pulling Image: sha256:fake","id":"sha256:fake","steps":0,"stepsCached":0,"stepsTotal":1,"subtasks":[],"task":"pulling","type":"ProgressBytes"}}"#
        ))
    }

    fn exit_output(&mut self) -> &mut CommandOutput {
        self.error = None;
        &mut self.output
    }
}

#[derive(Debug, Default)]
struct FakeState {
    script: VecDeque<Scripted>,
    calls: Vec<BootcCommand>,
    unexpected: Vec<String>,
}

/// Replays [`Scripted`] commands in order. A run that does not match the head of the queue
/// consumes nothing, fails, and is reported by [`FakeRunner::assert_finished`].
///
/// ```
/// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
/// use apex_bootc::{Bootc, BootcCommand, CancelToken, FakeRunner, Scripted};
///
/// let fake = FakeRunner::new();
/// fake.push(Scripted::new(BootcCommand::UpgradeCheck).stdout("No changes in: x\n"));
/// let bootc = Bootc::new(&fake);
/// assert_eq!(
///     bootc.check(&CancelToken::new()).await.unwrap(),
///     apex_bootc::CheckOutcome::UpToDate
/// );
/// fake.assert_finished();
/// # });
/// ```
#[derive(Debug, Default)]
pub struct FakeRunner {
    state: Mutex<FakeState>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, scripted: Scripted) -> &Self {
        self.lock().script.push_back(scripted);
        self
    }

    #[must_use]
    pub fn with(self, scripted: Scripted) -> Self {
        self.push(scripted);
        self
    }

    /// Only the runs that matched the script.
    pub fn calls(&self) -> Vec<BootcCommand> {
        self.lock().calls.clone()
    }

    pub fn remaining(&self) -> usize {
        self.lock().script.len()
    }

    pub fn unexpected(&self) -> Vec<String> {
        self.lock().unexpected.clone()
    }

    #[track_caller]
    pub fn assert_finished(&self) {
        let state = self.lock();
        let pending: Vec<&BootcCommand> = state.script.iter().map(|s| &s.expect).collect();
        assert!(
            state.unexpected.is_empty() && pending.is_empty(),
            "FakeRunner: unexpected runs {:?}; expected but not run {:?}",
            state.unexpected,
            pending
        );
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pop_if_expected(&self, command: &BootcCommand) -> Result<Scripted, String> {
        let mut state = self.lock();
        let Some(head) = state.script.pop_front() else {
            let msg = format!("unexpected {command:?}: script is empty");
            state.unexpected.push(msg.clone());
            return Err(msg);
        };
        if head.expect != *command {
            let msg = format!("unexpected {command:?}: expected {:?}", head.expect);
            state.script.push_front(head);
            state.unexpected.push(msg.clone());
            return Err(msg);
        }
        state.calls.push(command.clone());
        Ok(head)
    }
}

impl CommandRunner for FakeRunner {
    async fn run(&self, invocation: Invocation) -> Result<CommandOutput, BootcError> {
        let Invocation {
            command,
            progress,
            cancel,
        } = invocation;
        let kind = command.kind();
        let scripted = self
            .pop_if_expected(&command)
            .map_err(|detail| BootcError::Other {
                command: kind,
                exit: ExitState::Code(-1),
                detail: format!("FakeRunner: {detail}"),
            })?;
        if cancel.is_cancelled() {
            return Err(BootcError::Cancelled { command: kind });
        }
        let timeout = command.timeout();
        let deadline = Instant::now() + timeout;
        let mut sink = if command.uses_progress_fd() {
            progress
        } else {
            None
        };
        let mut decoder = ProgressDecoder::new();
        for step in scripted.steps {
            match step {
                Step::Sleep(delay) => {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(BootcError::Cancelled { command: kind }),
                        () = tokio::time::sleep_until(deadline) => {
                            return Err(BootcError::Timeout { command: kind, timeout });
                        }
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                Step::ProgressLine(line) => {
                    for update in decoder.push(format!("{line}\n").as_bytes()) {
                        forward(&mut sink, update);
                    }
                }
            }
        }
        match scripted.error {
            Some(error) => Err(error),
            None => Ok(scripted.output),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CancelToken;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn mismatch_is_not_consumed_and_is_recorded() {
        let fake = FakeRunner::new().with(Scripted::new(BootcCommand::Status));
        let err = fake
            .run(Invocation::new(BootcCommand::Rollback))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("expected Status"), "{err}");
        assert_eq!(fake.remaining(), 1);
        assert_eq!(fake.unexpected().len(), 1);
        fake.run(Invocation::new(BootcCommand::Status))
            .await
            .unwrap();
        assert_eq!(fake.calls(), vec![BootcCommand::Status]);
        let empty = fake.run(Invocation::new(BootcCommand::Status)).await;
        assert!(empty.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn delays_honour_timeout_and_cancel() {
        let fake = FakeRunner::new()
            .with(Scripted::new(BootcCommand::Status).delay(Duration::from_secs(31)))
            .with(Scripted::new(BootcCommand::Rollback).delay(Duration::from_secs(60)));
        let err = fake
            .run(Invocation::new(BootcCommand::Status))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BootcError::Timeout { timeout, .. } if timeout == Duration::from_secs(30))
        );

        let cancel = CancelToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            canceller.cancel();
        });
        let err = fake
            .run(Invocation::new(BootcCommand::Rollback).with_cancel(cancel))
            .await
            .unwrap_err();
        assert!(matches!(err, BootcError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn progress_only_for_progress_commands() {
        let (tx, mut rx) = mpsc::channel(8);
        let fake = FakeRunner::new()
            .with(
                Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: false })
                    .progress_bytes(1, 2),
            )
            .with(
                Scripted::new(BootcCommand::UpgradeDownloadOnly { progress: true })
                    .progress_bytes(1, 2),
            );
        fake.run(
            Invocation::new(BootcCommand::UpgradeDownloadOnly { progress: false })
                .with_progress(tx.clone()),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        fake.run(
            Invocation::new(BootcCommand::UpgradeDownloadOnly { progress: true }).with_progress(tx),
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap().fraction, Some(0.5));
        fake.assert_finished();
    }

    #[test]
    fn builder_switches_between_exit_and_error() {
        let s = Scripted::new(BootcCommand::Status)
            .error(BootcError::Cancelled {
                command: crate::CommandKind::Status,
            })
            .stdout("x")
            .exit_code(3)
            .stderr("y");
        assert!(s.error.is_none());
        assert_eq!(s.output, CommandOutput::new(ExitState::Code(3), "x", "y"));
        let s = s.error(BootcError::Cancelled {
            command: crate::CommandKind::Status,
        });
        assert!(s.error.is_some());
    }
}
