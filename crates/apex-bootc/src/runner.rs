//! Running bootc: the [`CommandRunner`] I/O boundary and its tokio implementation.
//!
//! [`TokioRunner`] starts `/usr/bin/bootc` with the argv and environment of
//! [`BootcCommand`], in its own process group, with stdin at `/dev/null`:
//!
//! - stdout and stderr are captured, keeping only the last [`OUTPUT_LIMIT`] bytes each;
//! - for `--progress-fd 3` a pipe is mapped to fd 3 with the `command-fds` crate (the
//!   `pre_exec` hook that needs `unsafe` lives there, not in Apex), parsed with
//!   [`ProgressDecoder`] and forwarded to the caller's channel without ever blocking bootc;
//! - on timeout or cancellation the whole group gets `SIGTERM`, then `SIGKILL` once the
//!   leader has exited or [`KILL_GRACE`] has passed. The leader is not reaped before
//!   the `SIGKILL`, so its pid, which is the group id, cannot have been recycled.
//!   Dropping a run mid-way also kills the group.

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

/// Bytes of stdout and of stderr kept per run (the tail).
pub const OUTPUT_LIMIT: usize = 1 << 20;

/// Time between `SIGTERM` and `SIGKILL` when a run is stopped.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// After bootc exits, how long to keep reading pipes a leftover descendant holds open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// After `SIGKILL`, how long to wait for the kernel to let the leader be reaped.
const REAP_GRACE: Duration = Duration::from_secs(10);

/// Poll interval while waiting for the leader to exit after `SIGTERM`.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Read buffer size for the child's pipes.
const READ_CHUNK: usize = 16 * 1024;

/// Cancellation signal shared by a caller and running commands. Cloning shares it.
#[derive(Debug, Clone)]
pub struct CancelToken(Arc<watch::Sender<bool>>);

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    /// Cancels every run using this token (now or later).
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Whether [`CancelToken::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Completes once the token is cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        if rx.wait_for(|cancelled| *cancelled).await.is_err() {
            // The sender lives in `self`, so this cannot happen; never report a
            // cancellation that did not occur.
            std::future::pending::<()>().await;
        }
    }
}

/// One request to run bootc.
#[derive(Debug)]
pub struct Invocation {
    /// What to run.
    pub command: BootcCommand,
    /// Where progress updates go, for commands that report progress. Updates are
    /// dropped when the channel is full: progress is lossy by design.
    pub progress: Option<mpsc::Sender<Progress>>,
    /// Stops the run (killing bootc's process group) when cancelled.
    pub cancel: CancelToken,
}

impl Invocation {
    /// Runs `command` without a progress sink and with a fresh cancel token.
    pub fn new(command: BootcCommand) -> Self {
        Self {
            command,
            progress: None,
            cancel: CancelToken::new(),
        }
    }

    /// Sends progress updates to `sink`.
    #[must_use]
    pub fn with_progress(mut self, sink: mpsc::Sender<Progress>) -> Self {
        self.progress = Some(sink);
        self
    }

    /// Uses `cancel` to stop the run.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }
}

/// What a finished bootc run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// How bootc ended.
    pub exit: ExitState,
    /// Captured stdout (the last [`OUTPUT_LIMIT`] bytes).
    pub stdout: Vec<u8>,
    /// Captured stderr (the last [`OUTPUT_LIMIT`] bytes).
    pub stderr: Vec<u8>,
    /// stdout was longer than the limit and its beginning was dropped.
    pub stdout_truncated: bool,
    /// stderr was longer than the limit and its beginning was dropped.
    pub stderr_truncated: bool,
}

impl CommandOutput {
    /// An output with the given exit state and streams, nothing truncated.
    pub fn new(exit: ExitState, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            exit,
            stdout: stdout.into(),
            stderr: stderr.into(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    /// `Ok(self)` on exit status 0, otherwise the failure classified from stderr.
    pub fn into_result(self, command: CommandKind) -> Result<Self, BootcError> {
        if self.exit.success() {
            Ok(self)
        } else {
            Err(classify_failure(command, self.exit, &self.stderr))
        }
    }

    /// stdout as text, with invalid UTF-8 replaced.
    pub fn stdout_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }
}

/// Runs bootc commands. The daemon holds a [`TokioRunner`]; tests use `FakeRunner`
/// (cargo feature `testing`).
pub trait CommandRunner: Send + Sync {
    /// Runs `invocation` to completion.
    ///
    /// A non-zero exit is **not** an error at this level: it is returned as a
    /// [`CommandOutput`] and classified by [`CommandOutput::into_result`]. The errors
    /// are [`BootcError::Spawn`], [`BootcError::Timeout`], [`BootcError::Cancelled`],
    /// and [`BootcError::Busy`] for a timeout while another process held the ostree
    /// sysroot lock.
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

/// The real runner: bootc as a child process of the tokio runtime.
#[derive(Debug, Clone)]
pub struct TokioRunner {
    program: PathBuf,
    lock_path: PathBuf,
    kill_grace: Duration,
    /// Replaces the per-command timeouts; only tests set it.
    timeout_override: Option<Duration>,
}

impl Default for TokioRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioRunner {
    /// Runs [`command::program`] (`/usr/bin/bootc`; `APEX_BOOTC` in debug builds).
    pub fn new() -> Self {
        Self {
            program: command::program(),
            lock_path: PathBuf::from(SYSROOT_LOCK_PATH),
            kill_grace: KILL_GRACE,
            timeout_override: None,
        }
    }

    /// The executable this runner starts.
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
                // Our bootc is dead, so a lock held now belongs to someone else.
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

/// Everything needed to run one child process. Internal: [`TokioRunner`] builds it from
/// a [`BootcCommand`]; tests build it directly to run arbitrary programs.
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

/// Why [`run_process`] produced no output.
#[derive(Debug)]
pub(crate) enum ProcessError {
    /// The process could not be started or waited for.
    Spawn(io::Error),
    /// The timeout expired; the process group was killed.
    TimedOut,
    /// The cancel token fired; the process group was killed.
    Cancelled,
}

enum Ended {
    Exited(io::Result<ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Runs `spec` to completion, timeout or cancellation.
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
    // `cmd` owns our copy of the progress pipe's write end. Close it now, so the reader
    // sees end-of-file once bootc and its descendants are gone.
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
        if matches!(ended, Ended::Exited(_)) && !io_done {
            // bootc exited; take what is still buffered in the pipes, unless a leftover
            // descendant keeps them open.
            if tokio::time::timeout(DRAIN_GRACE, &mut io).await.is_err() {
                tracing::warn!("a process left behind by bootc keeps its output open");
            }
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

/// A child that leads its own process group, killed as a group unless reaped.
#[derive(Debug)]
struct GroupChild {
    child: Child,
    /// The group id, which is the leader's pid (`process_group(0)`).
    pgid: Option<Pid>,
    reaped: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Leader {
    Running,
    /// Exited but not reaped: its pid (the group id) is still reserved.
    Exited,
    /// Already reaped; the group id may have been recycled.
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

    /// Waits for the leader and records that it was reaped. Cancel-safe.
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.reaped = true;
        }
        status
    }

    /// `SIGTERM` to the group, up to `grace` for the leader to exit, `SIGKILL` to the
    /// group, then reap the leader.
    async fn terminate(&mut self, grace: Duration) {
        let Some(pgid) = self.pgid else {
            // No pid means tokio already reaped the leader; nothing safe to signal.
            return;
        };
        signal_group(pgid, Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && leader_state(pgid) == Leader::Running {
            tokio::time::sleep(POLL_INTERVAL).await;
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

/// Whether the leader has exited, without reaping it (`waitid(WNOWAIT)`).
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

/// Drains the progress pipe until end-of-file, forwarding parsed updates. It keeps
/// reading when the caller's channel is full or closed, so bootc never blocks on it.
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

/// The last `limit` bytes of a stream.
///
/// A `VecDeque` is a ring buffer: appending a chunk and evicting the oldest bytes costs
/// O(chunk) without moving the rest, and memory stays bounded by `limit` however much
/// the child writes.
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
