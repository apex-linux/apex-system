use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use apex_bootc::CommandRunner;
use apex_dbus::ApexError;
use tokio::task::JoinHandle;

use crate::engine::Engine;
use crate::queue::{Activity, Op, Queue, Submission};

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

/// Dropped however the worker task ends, a panic included (the workspace builds with
/// `panic = "unwind"`), so no caller waits for work nobody will do.
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
    pub fn new(engine: Engine<R>) -> Self {
        Self {
            engine: Arc::new(engine),
            queue: Arc::new(Queue::new()),
            activity: Arc::new(Activity::new()),
            worker_alive: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn engine(&self) -> &Engine<R> {
        &self.engine
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    pub fn activity(&self) -> &Activity {
        &self.activity
    }

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

    pub fn worker_alive(&self) -> bool {
        self.worker_alive.load(Ordering::SeqCst)
    }

    pub async fn ping(&self) -> Result<(), ApexError> {
        if !self.worker_alive() {
            return Err(ApexError::Internal(
                "The update service can no longer run operations. It is restarted at the next request."
                    .to_owned(),
            ));
        }
        self.engine.ping().await
    }

    pub async fn submit(&self, op: Op) -> Result<(), ApexError> {
        // Before queueing, so a client hammering Check never waits behind other work.
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

    /// While an operation runs, `bootc status` would wait up to 30 s for its sysroot lock;
    /// skipping loses nothing, as every operation reads the status itself afterwards.
    pub fn read_may_refresh(&self) -> bool {
        self.engine.refresh_due() && self.queue.is_idle() && !self.engine.bootc_busy()
    }

    pub fn refresh_if_due(&self) {
        if self.read_may_refresh() {
            drop(self.queue.submit(Op::Refresh));
        }
    }

    pub async fn ensure_fresh(&self) {
        if self.read_may_refresh()
            && let Err(error) = self.submit(Op::Refresh).await
        {
            tracing::debug!(error = %error, "refresh before the call failed");
        }
    }

    pub fn is_idle(&self, timeout: Duration) -> bool {
        self.queue.is_idle() && self.activity.is_quiet(timeout)
    }

    pub fn shutdown(&self) {
        self.queue.close();
    }
}
