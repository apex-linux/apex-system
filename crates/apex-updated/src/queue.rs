use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use apex_bootc::CancelToken;
use apex_dbus::{ApexError, ErrorKind};
use tokio::sync::{Notify, oneshot};

pub const MAX_PENDING: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Refresh,
    Check,
    Download {
        /// The soft gates are power and the metered network.
        override_soft_gates: bool,
    },
    Apply {
        expected_digest: String,
    },
    Rollback {
        /// Denylist the booted digest.
        mark_bad: bool,
    },
    Automatic,
    Fallback {
        /// The target GRUB fell back from.
        digest: String,
    },
    MajorUpgrade {
        target_major: u32,
        /// As for [`Op::Download`].
        override_soft_gates: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Refresh,
    Check,
    Download,
    Apply,
    Rollback,
    Automatic,
    Fallback,
    MajorUpgrade,
}

impl Op {
    pub fn kind(&self) -> OpKind {
        match self {
            Self::Refresh => OpKind::Refresh,
            Self::Check => OpKind::Check,
            Self::Download { .. } => OpKind::Download,
            Self::Apply { .. } => OpKind::Apply,
            Self::Rollback { .. } => OpKind::Rollback,
            Self::Automatic => OpKind::Automatic,
            Self::Fallback { .. } => OpKind::Fallback,
            Self::MajorUpgrade { .. } => OpKind::MajorUpgrade,
        }
    }
}

impl OpKind {
    /// Up to bootc's two-hour download timeout each.
    pub const fn is_download_class(self) -> bool {
        matches!(self, Self::Download | Self::Automatic | Self::MajorUpgrade)
    }

    pub const fn changes_next_boot(self) -> bool {
        matches!(self, Self::Apply | Self::Rollback | Self::MajorUpgrade)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Check => "check",
            Self::Download => "download",
            Self::Apply => "apply",
            Self::Rollback => "rollback",
            Self::Automatic => "automatic",
            Self::Fallback => "fallback",
            Self::MajorUpgrade => "major-upgrade",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinTarget {
    Running,
    Pending(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Enqueue,
    Join(JoinTarget),
    /// Every operation refreshes, so a refresh is answered by any work already queued.
    Covered,
    Busy(&'static str),
}

const BUSY_DOWNLOADING: &str =
    "An update is being downloaded. Try again when the download has finished.";
const BUSY_CHANGING: &str = "Another change to the version that starts next is in progress. Try again when it has finished.";
const BUSY_FULL: &str = "Too many requests are waiting. Try again in a moment.";

pub fn admit(op: &Op, running: Option<&Op>, pending: &[&Op]) -> Admission {
    let kind = op.kind();
    let active = || running.into_iter().chain(pending.iter().copied());
    let downloading = active().any(|o| o.kind().is_download_class());
    let find = |wanted: &dyn Fn(&Op) -> bool| {
        if running.is_some_and(wanted) {
            Some(JoinTarget::Running)
        } else {
            pending
                .iter()
                .position(|p| wanted(p))
                .map(JoinTarget::Pending)
        }
    };
    match kind {
        // Never Busy: apex-boot-assess must get its answer within its 30 s timeout.
        OpKind::Fallback => {
            return find(&|o| o == op).map_or(Admission::Enqueue, Admission::Join);
        }
        OpKind::Refresh if running.is_some() || !pending.is_empty() => {
            return Admission::Covered;
        }
        OpKind::Check => {
            if let Some(target) = find(&|o| o.kind() == OpKind::Check) {
                return Admission::Join(target);
            }
            // A download-class operation need not contact the registry for this channel.
            if downloading {
                return Admission::Busy(BUSY_DOWNLOADING);
            }
        }
        k if k.is_download_class() && downloading => return Admission::Busy(BUSY_DOWNLOADING),
        k if k.changes_next_boot() => {
            if downloading {
                return Admission::Busy(BUSY_DOWNLOADING);
            }
            if let Some(target) = find(&|o| o == op) {
                return Admission::Join(target);
            }
            if active().any(|o| o.kind().changes_next_boot()) {
                return Admission::Busy(BUSY_CHANGING);
            }
        }
        _ => {}
    }
    // No burst of calls may crowd out the daily automatic run.
    if pending.len() >= MAX_PENDING && kind != OpKind::Automatic {
        Admission::Busy(BUSY_FULL)
    } else {
        Admission::Enqueue
    }
}

pub type OpResult = Result<(), ApexError>;

/// [`ApexError`] is not `Clone`; anything but a contract error becomes `Internal`, as it
/// would on the wire.
pub fn duplicate(result: &OpResult) -> OpResult {
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(ApexError::new(
            error.kind().unwrap_or(ErrorKind::Internal),
            error.message(),
        )),
    }
}

#[derive(Debug)]
pub struct Job {
    pub op: Op,
    pub cancel: CancelToken,
    replies: Vec<oneshot::Sender<OpResult>>,
}

#[derive(Debug)]
pub enum Submission {
    Queued(oneshot::Receiver<OpResult>),
    Done(OpResult),
}

#[derive(Debug)]
struct Running {
    op: Op,
    cancel: CancelToken,
    joined: Vec<oneshot::Sender<OpResult>>,
}

#[derive(Debug, Default)]
struct QueueState {
    running: Option<Running>,
    pending: VecDeque<Job>,
    closed: bool,
}

#[derive(Debug, Default)]
pub struct Queue {
    state: Mutex<QueueState>,
    wake: Notify,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One critical section, so two concurrent calls cannot both pass the admission check.
    pub fn submit(&self, op: Op) -> Submission {
        let mut state = self.lock();
        if state.closed {
            return Submission::Done(Err(stopping_error()));
        }
        let admission = {
            let pending: Vec<&Op> = state.pending.iter().map(|job| &job.op).collect();
            admit(&op, state.running.as_ref().map(|r| &r.op), &pending)
        };
        match admission {
            Admission::Covered => Submission::Done(Ok(())),
            Admission::Busy(message) => Submission::Done(Err(ApexError::Busy(message.to_owned()))),
            Admission::Join(target) => {
                let (reply, receiver) = oneshot::channel();
                let waiters = match target {
                    JoinTarget::Running => state.running.as_mut().map(|r| &mut r.joined),
                    JoinTarget::Pending(index) => {
                        state.pending.get_mut(index).map(|job| &mut job.replies)
                    }
                };
                match waiters {
                    Some(waiters) => waiters.push(reply),
                    // `admit` only names operations it was shown.
                    None => {
                        return Submission::Done(Err(ApexError::Internal(
                            "The operation to wait for is gone.".to_owned(),
                        )));
                    }
                }
                Submission::Queued(receiver)
            }
            Admission::Enqueue => {
                let (reply, receiver) = oneshot::channel();
                let job = Job {
                    op,
                    cancel: CancelToken::new(),
                    replies: vec![reply],
                };
                if job.op.kind() == OpKind::Fallback {
                    let at = state
                        .pending
                        .iter()
                        .take_while(|queued| queued.op.kind() == OpKind::Fallback)
                        .count();
                    state.pending.insert(at, job);
                } else {
                    state.pending.push_back(job);
                }
                drop(state);
                self.wake.notify_one();
                Submission::Queued(receiver)
            }
        }
    }

    pub async fn next(&self) -> Option<Job> {
        loop {
            {
                let mut state = self.lock();
                if state.closed {
                    return None;
                }
                if let Some(job) = state.pending.pop_front() {
                    state.running = Some(Running {
                        op: job.op.clone(),
                        cancel: job.cancel.clone(),
                        joined: Vec::new(),
                    });
                    return Some(job);
                }
            }
            // `notify_one` stores a permit when nobody waits, so a job pushed between the
            // check above and this await is not missed.
            self.wake.notified().await;
        }
    }

    pub fn finish(&self, job: Job, result: OpResult) {
        let joined = self
            .lock()
            .running
            .take()
            .map(|running| running.joined)
            .unwrap_or_default();
        let mut replies: Vec<_> = job.replies.into_iter().chain(joined).collect();
        // The callers may have gone away; the operation still ran.
        if let Some(last) = replies.pop() {
            for reply in replies {
                let _ = reply.send(duplicate(&result));
            }
            let _ = last.send(result);
        }
    }

    pub fn running(&self) -> Option<OpKind> {
        self.lock().running.as_ref().map(|r| r.op.kind())
    }

    pub fn is_idle(&self) -> bool {
        let state = self.lock();
        state.running.is_none() && state.pending.is_empty()
    }

    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        if let Some(running) = &state.running {
            running.cancel.cancel();
        }
        for job in state.pending.drain(..) {
            for reply in job.replies {
                let _ = reply.send(Err(stopping_error()));
            }
        }
        drop(state);
        self.wake.notify_one();
    }

    pub fn worker_stopped(&self) {
        self.close();
        // Nothing will finish it: dropping it releases the callers that joined it.
        let running = self.lock().running.take();
        drop(running);
    }
}

fn stopping_error() -> ApexError {
    ApexError::Busy("The update service is stopping. Try again in a moment.".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample(kind: OpKind) -> Op {
        match kind {
            OpKind::Refresh => Op::Refresh,
            OpKind::Check => Op::Check,
            OpKind::Download => Op::Download {
                override_soft_gates: false,
            },
            OpKind::Apply => Op::Apply {
                expected_digest: "sha256:d".into(),
            },
            OpKind::Rollback => Op::Rollback { mark_bad: false },
            OpKind::Automatic => Op::Automatic,
            OpKind::Fallback => Op::Fallback {
                digest: "sha256:f".into(),
            },
            OpKind::MajorUpgrade => Op::MajorUpgrade {
                target_major: 46,
                override_soft_gates: false,
            },
        }
    }

    const ALL: [OpKind; 8] = [
        OpKind::Refresh,
        OpKind::Check,
        OpKind::Download,
        OpKind::Apply,
        OpKind::Rollback,
        OpKind::Automatic,
        OpKind::Fallback,
        OpKind::MajorUpgrade,
    ];

    #[test]
    fn admission_table_snapshot() {
        use std::fmt::Write as _;
        let mut table = String::new();
        for running in std::iter::once(None).chain(ALL.map(Some)) {
            let running_op = running.map(sample);
            for kind in ALL {
                let decision = match admit(&sample(kind), running_op.as_ref(), &[]) {
                    Admission::Enqueue => "queue",
                    Admission::Join(_) => "join",
                    Admission::Covered => "covered",
                    Admission::Busy(_) => "busy",
                };
                writeln!(
                    table,
                    "{:<14} while {:<14} -> {decision}",
                    kind.as_str(),
                    running.map_or("idle", OpKind::as_str)
                )
                .unwrap();
            }
        }
        insta::assert_snapshot!(table);
    }

    #[test]
    fn a_full_queue_is_busy_except_for_fallback_and_automatic() {
        let checks: Vec<Op> = (0..MAX_PENDING).map(|_| Op::Refresh).collect();
        let pending: Vec<&Op> = checks.iter().collect();
        let running = sample(OpKind::Check);
        assert_eq!(
            admit(&sample(OpKind::Rollback), Some(&running), &pending),
            Admission::Busy(BUSY_FULL)
        );
        assert_eq!(
            admit(&sample(OpKind::Fallback), Some(&running), &pending),
            Admission::Enqueue
        );
        assert_eq!(
            admit(&Op::Automatic, Some(&running), &pending),
            Admission::Enqueue,
            "a burst of calls must not suppress the daily automatic run"
        );
    }

    #[test]
    fn identical_calls_join_and_conflicting_ones_are_busy() {
        let apply = |digest: &str| Op::Apply {
            expected_digest: digest.into(),
        };
        let waiting = [apply("sha256:a")];
        let pending: Vec<&Op> = waiting.iter().collect();
        let running = Op::Check;
        assert_eq!(
            admit(&apply("sha256:a"), Some(&running), &pending),
            Admission::Join(JoinTarget::Pending(0))
        );
        assert_eq!(
            admit(&apply("sha256:b"), Some(&running), &pending),
            Admission::Busy(BUSY_CHANGING)
        );
        assert_eq!(
            admit(&Op::Rollback { mark_bad: true }, Some(&running), &pending),
            Admission::Busy(BUSY_CHANGING)
        );
        let rollback = Op::Rollback { mark_bad: true };
        assert_eq!(
            admit(&rollback.clone(), Some(&rollback), &[]),
            Admission::Join(JoinTarget::Running)
        );
        assert_eq!(
            admit(&Op::Rollback { mark_bad: false }, Some(&rollback), &[]),
            Admission::Busy(BUSY_CHANGING)
        );
    }

    #[test]
    fn a_check_is_answered_only_by_a_check() {
        let pending_check = [Op::Check];
        let pending: Vec<&Op> = pending_check.iter().collect();
        assert_eq!(
            admit(&Op::Check, Some(&sample(OpKind::Apply)), &pending),
            Admission::Join(JoinTarget::Pending(0))
        );
        for kind in ALL.into_iter().filter(|k| k.is_download_class()) {
            assert_eq!(
                admit(&Op::Check, Some(&sample(kind)), &[]),
                Admission::Busy(BUSY_DOWNLOADING),
                "{}",
                kind.as_str()
            );
            let waiting = [sample(kind)];
            let pending: Vec<&Op> = waiting.iter().collect();
            assert_eq!(
                admit(&Op::Check, Some(&sample(OpKind::Apply)), &pending),
                Admission::Busy(BUSY_DOWNLOADING)
            );
        }
    }

    proptest! {
        #[test]
        fn admission_invariants(ops in proptest::collection::vec((0usize..8, 0u8..3), 0..40)) {
            let mut running: Option<Op> = None;
            let mut pending: Vec<Op> = Vec::new();
            for (index, variant) in ops {
                let mut op = sample(ALL[index]);
                match &mut op {
                    Op::Apply { expected_digest } => {
                        *expected_digest = format!("sha256:{variant}");
                    }
                    Op::Rollback { mark_bad } => *mark_bad = variant == 0,
                    Op::Fallback { digest } => *digest = format!("sha256:{variant}"),
                    Op::MajorUpgrade { target_major, .. } => *target_major = 46 + u32::from(variant),
                    _ => {}
                }
                let decision = {
                    let queued: Vec<&Op> = pending.iter().collect();
                    admit(&op, running.as_ref(), &queued)
                };
                if decision == Admission::Enqueue {
                    if running.is_none() { running = Some(op) } else { pending.push(op) }
                }
                let active = || running.iter().chain(pending.iter()).map(Op::kind);
                prop_assert!(active().filter(|k| k.is_download_class()).count() <= 1);
                prop_assert!(active().filter(|k| k.changes_next_boot()).count() <= 1);
                prop_assert!(active().filter(|k| *k == OpKind::Check).count() <= 1);
                let limited = pending
                    .iter()
                    .filter(|o| !matches!(o.kind(), OpKind::Fallback | OpKind::Automatic))
                    .count();
                prop_assert!(limited <= MAX_PENDING);
            }
        }
    }

    #[tokio::test]
    async fn queue_runs_in_order_joins_and_closes() {
        let queue = Queue::new();
        let Submission::Queued(first) = queue.submit(Op::Check) else {
            panic!("queued")
        };
        let Submission::Queued(second) = queue.submit(Op::Apply {
            expected_digest: "d".into(),
        }) else {
            panic!("queued")
        };
        let Submission::Queued(joined) = queue.submit(Op::Check) else {
            panic!("joins the queued check")
        };
        let job = queue.next().await.unwrap();
        assert_eq!(job.op, Op::Check);
        assert_eq!(queue.running(), Some(OpKind::Check));
        let Submission::Queued(late) = queue.submit(Op::Check) else {
            panic!("joins the running check")
        };
        queue.finish(
            job,
            Err(ApexError::Network("the registry is unreachable".into())),
        );
        for reply in [first, joined, late] {
            let error = reply.await.unwrap().unwrap_err();
            assert_eq!(error.kind(), Some(ErrorKind::Network));
            assert_eq!(error.message(), "the registry is unreachable");
        }
        let job = queue.next().await.unwrap();
        assert_eq!(job.op.kind(), OpKind::Apply);
        let cancel = job.cancel.clone();
        assert!(matches!(
            queue.submit(Op::Refresh),
            Submission::Done(Ok(()))
        ));
        assert!(matches!(
            queue.submit(Op::Rollback { mark_bad: false }),
            Submission::Done(Err(ApexError::Busy(_)))
        ));
        let Submission::Queued(third) = queue.submit(Op::Automatic) else {
            panic!("automatic queues behind apply")
        };
        queue.close();
        assert!(cancel.is_cancelled());
        assert!(third.await.unwrap().is_err());
        queue.finish(job, Err(ApexError::Internal("cancelled".into())));
        assert!(second.await.unwrap().is_err());
        assert!(queue.next().await.is_none());
        assert!(matches!(queue.submit(Op::Check), Submission::Done(Err(_))));
    }

    #[tokio::test]
    async fn fallback_reports_go_to_the_front() {
        let queue = Queue::new();
        let running = queue.submit(Op::Check);
        let _job = queue.next().await.unwrap();
        let rest = [
            queue.submit(Op::Apply {
                expected_digest: "sha256:a".into(),
            }),
            queue.submit(Op::Automatic),
            queue.submit(Op::Fallback {
                digest: "sha256:1".into(),
            }),
            queue.submit(Op::Fallback {
                digest: "sha256:2".into(),
            }),
        ];
        assert!(
            rest.iter()
                .chain([&running])
                .all(|s| matches!(s, Submission::Queued(_)))
        );
        assert!(matches!(
            queue.submit(Op::Fallback {
                digest: "sha256:1".into(),
            }),
            Submission::Queued(_)
        ));
        let order: Vec<Op> = {
            let state = queue.lock();
            state.pending.iter().map(|job| job.op.clone()).collect()
        };
        assert_eq!(
            order,
            [
                Op::Fallback {
                    digest: "sha256:1".into()
                },
                Op::Fallback {
                    digest: "sha256:2".into()
                },
                Op::Apply {
                    expected_digest: "sha256:a".into()
                },
                Op::Automatic,
            ]
        );
    }

    #[tokio::test]
    async fn a_stopped_worker_releases_everyone_waiting() {
        let queue = Queue::new();
        let Submission::Queued(first) = queue.submit(Op::Check) else {
            panic!("queued")
        };
        let job = queue.next().await.unwrap();
        let Submission::Queued(joined) = queue.submit(Op::Check) else {
            panic!("joined")
        };
        let Submission::Queued(waiting) = queue.submit(Op::Automatic) else {
            panic!("queued")
        };
        // As when the worker panics: the job is dropped, never finished.
        drop(job);
        queue.worker_stopped();
        assert!(first.await.is_err(), "the job's own caller is released");
        assert!(joined.await.is_err(), "so is the one that joined it");
        assert!(waiting.await.unwrap().is_err());
        assert!(queue.is_idle());
        assert!(matches!(
            queue.submit(Op::Check),
            Submission::Done(Err(ApexError::Busy(_)))
        ));
    }

    #[test]
    fn duplicated_results_keep_kind_and_message() {
        assert!(duplicate(&Ok(())).is_ok());
        let error = duplicate(&Err(ApexError::DigestMismatch("changed".into()))).unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::DigestMismatch));
        assert_eq!(error.message(), "changed");
        let foreign = duplicate(&Err(ApexError::ZBus(zbus::Error::Failure("x".into()))));
        assert_eq!(foreign.unwrap_err().kind(), Some(ErrorKind::Internal));
    }
}
