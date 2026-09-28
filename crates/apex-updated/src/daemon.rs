//! The engine, the queue and the activity tracker together, plus the worker task.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use apex_bootc::CommandRunner;
use apex_dbus::ApexError;
use tokio::task::JoinHandle;

use crate::engine::Engine;
use crate::queue::{Activity, Op, Queue, Submission};

/// A handle shared by the D-Bus interface, the worker and `main`. Cloning shares it.
#[derive(Debug)]
pub struct Daemon<R> {
    engine: Arc<Engine<R>>,
    queue: Arc<Queue>,
    activity: Arc<Activity>,
    worker_alive: Arc<AtomicBool>,
}

impl<R> Clone for Daemon<R> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            queue: Arc::clone(&self.queue),
            activity: Arc::clone(&self.activity),
            worker_alive: Arc::clone(&self.worker_alive),
        }
    }
}

/// Held by the worker task. Dropped when the task ends for any reason, a panic included
/// (the workspace builds with `panic = "unwind"`, and tokio catches a task's panic): it
/// records that no worker runs and stops the queue, so no caller waits for work nobody
/// will do.
struct WorkerGuard {
    alive: Arc<AtomicBool>,
    queue: Arc<Queue>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        self.queue.worker_stopped();
    }
}

impl<R: CommandRunner + 'static> Daemon<R> {
    /// Wraps an engine with an empty queue.
    pub fn new(engine: Engine<R>) -> Self {
        Self {
            engine: Arc::new(engine),
            queue: Arc::new(Queue::new()),
            activity: Arc::new(Activity::new()),
            worker_alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The engine.
    pub fn engine(&self) -> &Engine<R> {
        &self.engine
    }

    /// The operation queue.
    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// The activity tracker.
    pub fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Starts the worker: it executes queued operations one at a time until the queue is
    /// closed.
    ///
    /// If an operation panics, the worker ends: the queue is closed, every waiting caller
    /// gets an error, [`Self::worker_alive`] turns false (so `Ping` fails), and the returned
    /// handle completes, which `main` treats as fatal. The process then exits and D-Bus
    /// activation starts a fresh instance at the next call, instead of a service that keeps
    /// answering `Ping` while its queue never moves again.
    pub fn spawn_worker(&self) -> JoinHandle<()> {
        let this = self.clone();
        self.worker_alive.store(true, Ordering::SeqCst);
        tokio::spawn(async move {
            let _guard = WorkerGuard {
                alive: Arc::clone(&this.worker_alive),
                queue: Arc::clone(&this.queue),
            };
            while let Some(job) = this.queue.next().await {
                let kind = job.op.kind();
                tracing::debug!(op = kind.as_str(), "operation started");
                let result = this.engine.execute(&job.op, &job.cancel).await;
                match &result {
                    Ok(()) => tracing::debug!(op = kind.as_str(), "operation finished"),
                    Err(error) => {
                        tracing::info!(op = kind.as_str(), error = %error, "operation failed");
                    }
                }
                this.activity.touch();
                this.queue.finish(job, result);
            }
        })
    }

    /// Whether the worker started by [`Self::spawn_worker`] is still running.
    pub fn worker_alive(&self) -> bool {
        self.worker_alive.load(Ordering::SeqCst)
    }

    /// `Ping`: fails when the worker has stopped (the service can no longer run operations),
    /// otherwise [`Engine::ping`].
    pub async fn ping(&self) -> Result<(), ApexError> {
        if !self.worker_alive() {
            return Err(ApexError::Internal(
                "The update service can no longer run operations. It is restarted at the next request."
                    .to_owned(),
            ));
        }
        self.engine.ping().await
    }

    /// Runs `op` through the queue and waits for its result.
    ///
    /// A `Check` within the rate limit (60 s after the last successful check) returns
    /// success without queueing, so a client hammering `Check` never waits behind other work.
    pub async fn submit(&self, op: Op) -> Result<(), ApexError> {
        if op == Op::Check && self.engine.check_is_recent() {
            return Ok(());
        }
        match self.queue.submit(op) {
            Submission::Done(result) => result,
            Submission::Queued(reply) => reply.await.unwrap_or_else(|_| {
                Err(ApexError::Internal(
                    "The operation was dropped before it finished.".to_owned(),
                ))
            }),
        }
    }

    /// Whether a refresh asked for by a read (a property or `GetChanges`) may start now: the
    /// state is older than [`crate::REFRESH_MAX_AGE`], and nothing is queued or running.
    ///
    /// While an operation runs, its bootc command holds the sysroot lock and `bootc status`
    /// would wait for it (up to 30 s), so the read is answered from the last snapshot
    /// instead. The refresh is skipped, not deferred: every operation reads the status
    /// itself when its bootc command has finished.
    pub fn read_may_refresh(&self) -> bool {
        self.engine.refresh_due() && self.queue.is_idle() && !self.engine.bootc_busy()
    }

    /// Queues a refresh without waiting, if [`Self::read_may_refresh`].
    pub fn refresh_if_due(&self) {
        if self.read_may_refresh() {
            drop(self.queue.submit(Op::Refresh));
        }
    }

    /// Refreshes and waits for it, if [`Self::read_may_refresh`]. While other work runs, the
    /// last snapshot is used as it is.
    pub async fn ensure_fresh(&self) {
        if self.read_may_refresh()
            && let Err(error) = self.submit(Op::Refresh).await
        {
            tracing::debug!(error = %error, "refresh before the call failed");
        }
    }

    /// Whether the service may exit: nothing queued or running, no call in flight, and
    /// no activity for `timeout`.
    pub fn is_idle(&self, timeout: Duration) -> bool {
        self.queue.is_idle() && self.activity.is_quiet(timeout)
    }

    /// Stops the queue and cancels the running operation.
    pub fn shutdown(&self) {
        self.queue.close();
    }
}
