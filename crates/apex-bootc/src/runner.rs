use std::collections::VecDeque;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use command_fds::{CommandFdExt, FdMapping};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::command::{self, BootcCommand, CommandKind, ENVIRONMENT, PROGRESS_FD};
use crate::error::{BootcError, ExitState, classify_failure};
use crate::lock::{SYSROOT_LOCK_PATH, sysroot_lock_held};
use crate::progress::{Progress, ProgressDecoder};

/// Per stream; the tail is kept.
pub const OUTPUT_LIMIT: usize = 1 << 20;

pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// A descendant bootc left behind can hold its output pipes open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

const REAP_GRACE: Duration = Duration::from_secs(10);

const LEADER_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

const READ_CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct CancelToken(Arc<watch::Sender<bool>>);

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    /// Also cancels runs that start later with this token.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        if rx.wait_for(|cancelled| *cancelled).await.is_err() {
            // `self` holds the sender, so this is unreachable; never invent a cancellation.
            std::future::pending::<()>().await;
        }
    }
}

#[derive(Debug)]
pub struct Invocation {
    pub command: BootcCommand,
    /// Lossy: updates are dropped while the channel is full.
    pub progress: Option<mpsc::Sender<Progress>>,
    pub cancel: CancelToken,
}

impl Invocation {
    pub fn new(command: BootcCommand) -> Self {
        Self {
            command,
            progress: None,
            cancel: CancelToken::new(),
        }
    }

    #[must_use]
    pub fn with_progress(mut self, sink: mpsc::Sender<Progress>) -> Self {
        self.progress = Some(sink);
        self
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit: ExitState,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl CommandOutput {
    pub fn new(exit: ExitState, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            exit,
            stdout: stdout.into(),
            stderr: stderr.into(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    pub fn into_result(self, command: CommandKind) -> Result<Self, BootcError> {
        if self.exit.success() {
            Ok(self)
        } else {
            Err(classify_failure(command, self.exit, &self.stderr))
        }
    }

    pub fn stdout_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
}

pub trait CommandRunner: Send + Sync {
    /// A non-zero exit is no error here: [`CommandOutput::into_result`] classifies it.
    fn run(
        &self,
        invocation: Invocation,
    ) -> impl Future<Output = Result<CommandOutput, BootcError>> + Send;
}

impl<R: CommandRunner> CommandRunner for Arc<R> {
    fn run(
        &self,
        invocation: Invocation,
    ) -> impl Future<Output = Result<CommandOutput, BootcError>> + Send {
        (**self).run(invocation)
    }
}

impl<R: CommandRunner> CommandRunner for &R {
    fn run(
        &self,
        invocation: Invocation,
    ) -> impl Future<Output = Result<CommandOutput, BootcError>> + Send {
        (**self).run(invocation)
    }
}

#[derive(Debug, Clone)]
pub struct TokioRunner {
    program: PathBuf,
    lock_path: PathBuf,
    kill_grace: Duration,
    timeout_override: Option<Duration>,
}

impl Default for TokioRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioRunner {
    pub fn new() -> Self {
        Self {
            program: command::program(),
            lock_path: PathBuf::from(SYSROOT_LOCK_PATH),
            kill_grace: KILL_GRACE,
            timeout_override: None,
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    #[cfg(test)]
    fn for_test(program: &Path, lock_path: &Path, timeout: Option<Duration>) -> Self {
        Self {
            program: program.to_owned(),
            lock_path: lock_path.to_owned(),
            kill_grace: Duration::from_millis(200),
            timeout_override: timeout,
        }
    }
}

impl CommandRunner for TokioRunner {
    async fn run(&self, invocation: Invocation) -> Result<CommandOutput, BootcError> {
        let Invocation {
            command,
            progress,
            cancel,
        } = invocation;
        let kind = command.kind();
        let spec = ProcessSpec {
            program: self.program.clone(),
            args: command.argv(),
            env: ENVIRONMENT
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            timeout: self.timeout_override.unwrap_or_else(|| command.timeout()),
            kill_grace: self.kill_grace,
            progress_fd: command.uses_progress_fd(),
            output_limit: OUTPUT_LIMIT,
        };
        tracing::debug!(command = %kind, program = %spec.program.display(), "running bootc");
        match run_process(&spec, progress, &cancel).await {
            Ok(output) => {
                tracing::debug!(command = %kind, exit = %output.exit, "bootc finished");
                Ok(output)
            }
            Err(ProcessError::Spawn(source)) => Err(BootcError::Spawn {
                program: spec.program,
                source,
            }),
            Err(ProcessError::TimedOut) => {
                // Our bootc is dead, so a lock held now is someone else's.
                if sysroot_lock_held(&self.lock_path).unwrap_or(false) {
                    Err(BootcError::Busy {
                        detail: format!(
                            "`bootc {kind}` waited {} s for {}, which another process holds",
                            spec.timeout.as_secs(),
                            self.lock_path.display()
                        ),
                    })
                } else {
                    Err(BootcError::Timeout {
                        command: kind,
                        timeout: spec.timeout,
                    })
                }
            }
            Err(ProcessError::Cancelled) => Err(BootcError::Cancelled { command: kind }),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProcessSpec {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) timeout: Duration,
    pub(crate) kill_grace: Duration,
    pub(crate) progress_fd: bool,
    pub(crate) output_limit: usize,
}

#[derive(Debug)]
pub(crate) enum ProcessError {
    Spawn(io::Error),
    TimedOut,
    Cancelled,
}

enum Ended {
    Exited(io::Result<ExitStatus>),
    TimedOut,
    Cancelled,
}

pub(crate) async fn run_process(
    spec: &ProcessSpec,
    progress: Option<mpsc::Sender<Progress>>,
    cancel: &CancelToken,
) -> Result<CommandOutput, ProcessError> {
    if cancel.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let progress_rx = if spec.progress_fd {
        let (reader, writer) = io::pipe().map_err(ProcessError::Spawn)?;
        cmd.fd_mappings(vec![FdMapping {
            parent_fd: OwnedFd::from(writer),
            child_fd: PROGRESS_FD,
        }])
        .map_err(|e| ProcessError::Spawn(io::Error::other(e)))?;
        Some(pipe::Receiver::from_owned_fd(OwnedFd::from(reader)).map_err(ProcessError::Spawn)?)
    } else {
        None
    };
    let child = cmd.spawn().map_err(ProcessError::Spawn)?;
    // Closes our copy of the progress pipe's write end, so the reader sees end-of-file
    // once bootc and its descendants are gone.
    drop(cmd);
    let mut child = GroupChild::new(child);

    let stdout = child.child.stdout.take();
    let stderr = child.child.stderr.take();
    let mut out = TailBuffer::new(spec.output_limit);
    let mut err = TailBuffer::new(spec.output_limit);
    let deadline = Instant::now() + spec.timeout;

    let ended = {
        let io = async {
            tokio::join!(
                read_into(stdout, &mut out),
                read_into(stderr, &mut err),
                pump_progress(progress_rx, progress),
            );
        };
        tokio::pin!(io);
        let mut io_done = false;
        let ended = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break Ended::Cancelled,
                () = tokio::time::sleep_until(deadline) => break Ended::TimedOut,
                () = &mut io, if !io_done => io_done = true,
                status = child.wait() => break Ended::Exited(status),
            }
        };
        if matches!(ended, Ended::Exited(_))
            && !io_done
            && tokio::time::timeout(DRAIN_GRACE, &mut io).await.is_err()
        {
            tracing::warn!("a process left behind by bootc keeps its output open");
        }
        ended
    };

    match ended {
        Ended::Exited(status) => {
            let status = status.map_err(ProcessError::Spawn)?;
            let (stdout, stdout_truncated) = out.into_parts();
            let (stderr, stderr_truncated) = err.into_parts();
            Ok(CommandOutput {
                exit: status.into(),
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            })
        }
        Ended::TimedOut => {
            tracing::warn!(
                timeout_s = spec.timeout.as_secs(),
                "bootc timed out; killing its process group"
            );
            child.terminate(spec.kill_grace).await;
            Err(ProcessError::TimedOut)
        }
        Ended::Cancelled => {
            tracing::info!("bootc cancelled; killing its process group");
            child.terminate(spec.kill_grace).await;
            Err(ProcessError::Cancelled)
        }
    }
}

/// Leads its own process group. The leader is not reaped before the group's `SIGKILL`, so
/// its pid, which is the group id, cannot have been recycled by then.
#[derive(Debug)]
struct GroupChild {
    child: Child,
    pgid: Option<Pid>,
    reaped: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Leader {
    Running,
    /// Not reaped yet, so the group id is still ours.
    Exited,
    /// The group id may have been recycled.
    Gone,
}

impl GroupChild {
    fn new(child: Child) -> Self {
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(Pid::from_raw);
        Self {
            child,
            pgid,
            reaped: false,
        }
    }

    /// Cancel-safe.
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.reaped = true;
        }
        status
    }

    async fn terminate(&mut self, grace: Duration) {
        let Some(pgid) = self.pgid else {
            // No pid means tokio already reaped the leader; nothing safe to signal.
            return;
        };
        signal_group(pgid, Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && leader_state(pgid) == Leader::Running {
            tokio::time::sleep(LEADER_EXIT_POLL_INTERVAL).await;
        }
        // The leader is at most a zombie here (only `wait` reaps it), so the group id
        // still names our group; this reaches stragglers that ignored SIGTERM.
        if leader_state(pgid) != Leader::Gone {
            signal_group(pgid, Signal::SIGKILL);
        }
        match tokio::time::timeout(REAP_GRACE, self.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not reap bootc"),
            Err(_) => tracing::warn!(
                "bootc survived SIGKILL (uninterruptible sleep?); leaving it to the reaper"
            ),
        }
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(pgid) = self.pgid
            && leader_state(pgid) != Leader::Gone
        {
            signal_group(pgid, Signal::SIGKILL);
        }
    }
}

fn leader_state(pid: Pid) -> Leader {
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG;
    match waitid(Id::Pid(pid), flags) {
        Ok(WaitStatus::StillAlive) => Leader::Running,
        Ok(_) => Leader::Exited,
        Err(_) => Leader::Gone,
    }
}

fn signal_group(pgid: Pid, signal: Signal) {
    match killpg(pgid, signal) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => tracing::warn!(%error, ?signal, "could not signal bootc's process group"),
    }
}

async fn read_into<R: AsyncRead + Unpin>(reader: Option<R>, sink: &mut TailBuffer) {
    let Some(mut reader) = reader else { return };
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => sink.push(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::debug!(%error, "reading bootc output failed");
                break;
            }
        }
    }
}

/// Keeps reading while the caller's channel is full or closed, so bootc never blocks on it.
async fn pump_progress(rx: Option<pipe::Receiver>, mut sink: Option<mpsc::Sender<Progress>>) {
    let Some(mut rx) = rx else { return };
    let mut decoder = ProgressDecoder::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    loop {
        match rx.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                for update in decoder.push(&chunk[..n]) {
                    forward(&mut sink, update);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                tracing::debug!(%error, "reading bootc progress failed");
                break;
            }
        }
    }
    if let Some(update) = decoder.finish() {
        forward(&mut sink, update);
    }
}

pub(crate) fn forward(sink: &mut Option<mpsc::Sender<Progress>>, update: Progress) {
    if let Some(tx) = sink.as_ref() {
        match tx.try_send(update) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Closed(_)) => *sink = None,
        }
    }
}

#[derive(Debug)]
struct TailBuffer {
    buf: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl TailBuffer {
    fn new(limit: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, mut chunk: &[u8]) {
        if chunk.len() > self.limit {
            self.truncated = true;
            self.buf.clear();
            chunk = &chunk[chunk.len() - self.limit..];
        }
        let overflow = (self.buf.len() + chunk.len()).saturating_sub(self.limit);
        if overflow > 0 {
            self.truncated = true;
            self.buf.drain(..overflow);
        }
        self.buf.extend(chunk);
    }

    fn into_parts(self) -> (Vec<u8>, bool) {
        (Vec::from(self.buf), self.truncated)
    }
}

#[cfg(test)]
mod tests;
