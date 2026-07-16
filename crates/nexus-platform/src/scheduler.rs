use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;

type Job = Box<dyn FnOnce(CancellationToken) + Send + 'static>;

/// Priority values visible at the legacy Clockwork call sites.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TaskPriority {
    /// Background maintenance work.
    Low,
    /// Ordinary runtime work.
    Normal,
    /// Latency-sensitive work reserved for Rust-owned services.
    High,
}

/// A process-local task identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(u64);

impl TaskId {
    /// Returns the numeric identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The terminal state of a submitted task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskOutcome {
    /// The task returned without a cancellation request.
    Completed,
    /// The task was cancelled before running or observed cancellation and returned.
    Cancelled,
    /// The task panicked; the worker contained the unwind.
    Panicked,
}

/// Cooperative cancellation passed to scheduled work.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
    wait_lock: Mutex<()>,
    wait_cv: Condvar,
}

impl CancellationToken {
    fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                wait_lock: Mutex::new(()),
                wait_cv: Condvar::new(),
            }),
        }
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(AtomicOrdering::Acquire)
    }

    /// Waits until cancellation is requested or the timeout elapses.
    ///
    /// Returns `true` when cancelled and `false` on timeout.
    #[must_use]
    pub fn wait_cancelled_timeout(&self, timeout: Duration) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let guard = lock_unpoison(&self.state.wait_lock);
        let result = self
            .state
            .wait_cv
            .wait_timeout_while(guard, timeout, |_| !self.is_cancelled());
        match result {
            Ok((_guard, _timeout)) => self.is_cancelled(),
            Err(poisoned) => {
                let (_guard, _timeout) = poisoned.into_inner();
                self.is_cancelled()
            }
        }
    }

    fn cancel(&self) {
        self.state.cancelled.store(true, AtomicOrdering::Release);
        self.state.wait_cv.notify_all();
    }
}

/// An owned handle for cancellation and completion observation.
#[derive(Clone, Debug)]
pub struct TaskHandle {
    id: TaskId,
    control: Arc<TaskControl>,
}

impl TaskHandle {
    /// Returns the task identifier.
    #[must_use]
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// Requests cooperative cancellation.
    pub fn cancel(&self) {
        self.control.token.cancel();
    }

    /// Returns the terminal outcome if the task has finished.
    #[must_use]
    pub fn outcome(&self) -> Option<TaskOutcome> {
        *lock_unpoison(&self.control.outcome)
    }

    /// Blocks until the task reaches a terminal state.
    #[must_use]
    pub fn wait(&self) -> TaskOutcome {
        let mut outcome = lock_unpoison(&self.control.outcome);
        while outcome.is_none() {
            outcome = wait_unpoison(&self.control.done, outcome);
        }
        outcome.expect("loop exits only with a task outcome")
    }

    /// Waits for a bounded duration and returns `None` on timeout.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> Option<TaskOutcome> {
        let outcome = lock_unpoison(&self.control.outcome);
        if outcome.is_some() {
            return *outcome;
        }
        let result = self
            .control
            .done
            .wait_timeout_while(outcome, timeout, |outcome| outcome.is_none());
        match result {
            Ok((outcome, _timeout)) => *outcome,
            Err(poisoned) => {
                let (outcome, _timeout) = poisoned.into_inner();
                *outcome
            }
        }
    }
}

#[derive(Debug)]
struct TaskControl {
    token: CancellationToken,
    outcome: Mutex<Option<TaskOutcome>>,
    done: Condvar,
}

/// A small owned scheduler implementing only contracts proven by visible
/// Clockwork call sites: prioritized one-shot work, interval work, cooperative
/// cancellation, panic containment, and deterministic drain.
///
/// Interval tasks occupy one worker while alive. This intentionally explicit
/// limitation avoids claiming behavioral equivalence with the unavailable
/// Clockwork implementation.
pub struct MinimalScheduler {
    shared: Arc<Shared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for MinimalScheduler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock_unpoison(&self.shared.state);
        formatter
            .debug_struct("MinimalScheduler")
            .field("accepting", &state.accepting)
            .field("queued", &state.queue.len())
            .field("tracked", &state.controls.len())
            .finish()
    }
}

struct Shared {
    state: Mutex<SchedulerState>,
    available: Condvar,
}

struct SchedulerState {
    accepting: bool,
    next_id: u64,
    next_sequence: u64,
    queue: BinaryHeap<QueuedTask>,
    controls: BTreeMap<TaskId, Weak<TaskControl>>,
}

struct QueuedTask {
    id: TaskId,
    priority: TaskPriority,
    sequence: u64,
    control: Arc<TaskControl>,
    job: Option<Job>,
}

impl PartialEq for QueuedTask {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl Eq for QueuedTask {}

impl PartialOrd for QueuedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueuedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl MinimalScheduler {
    /// Creates a scheduler sized to the host's available parallelism.
    ///
    /// # Errors
    ///
    /// Returns an error only if the resolved worker count is zero.
    pub fn new() -> Result<Self, MinimalSchedulerError> {
        let workers = thread::available_parallelism().map_or(1, usize::from);
        Self::with_worker_count(workers)
    }

    /// Creates a scheduler with an explicit worker count.
    ///
    /// # Errors
    ///
    /// Returns [`MinimalSchedulerError::ZeroWorkers`] for zero workers or
    /// [`MinimalSchedulerError::WorkerSpawn`] if a worker cannot be created.
    pub fn with_worker_count(worker_count: usize) -> Result<Self, MinimalSchedulerError> {
        if worker_count == 0 {
            return Err(MinimalSchedulerError::ZeroWorkers);
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(SchedulerState {
                accepting: true,
                next_id: 1,
                next_sequence: 1,
                queue: BinaryHeap::new(),
                controls: BTreeMap::new(),
            }),
            available: Condvar::new(),
        });
        let mut workers: Vec<JoinHandle<()>> = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let worker_shared = Arc::clone(&shared);
            let handle = thread::Builder::new()
                .name(format!("nexus-platform-worker-{index}"))
                .spawn(move || worker_loop(&worker_shared))
                .map_err(|source| {
                    {
                        let mut state = lock_unpoison(&shared.state);
                        state.accepting = false;
                    }
                    shared.available.notify_all();
                    for worker in workers.drain(..) {
                        let _ = worker.join();
                    }
                    MinimalSchedulerError::WorkerSpawn { source }
                })?;
            workers.push(handle);
        }
        Ok(Self {
            shared,
            workers: Mutex::new(workers),
        })
    }

    /// Submits a one-shot task.
    ///
    /// # Errors
    ///
    /// Returns [`MinimalSchedulerError::Closed`] after shutdown begins.
    pub fn submit<F>(
        &self,
        priority: TaskPriority,
        job: F,
    ) -> Result<TaskHandle, MinimalSchedulerError>
    where
        F: FnOnce(CancellationToken) + Send + 'static,
    {
        let control = Arc::new(TaskControl {
            token: CancellationToken::new(),
            outcome: Mutex::new(None),
            done: Condvar::new(),
        });
        let id = {
            let mut state = lock_unpoison(&self.shared.state);
            if !state.accepting {
                return Err(MinimalSchedulerError::Closed);
            }
            let id = TaskId(state.next_id);
            state.next_id = state.next_id.saturating_add(1);
            let sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.saturating_add(1);
            state.controls.insert(id, Arc::downgrade(&control));
            state.queue.push(QueuedTask {
                id,
                priority,
                sequence,
                control: Arc::clone(&control),
                job: Some(Box::new(job)),
            });
            id
        };
        self.shared.available.notify_one();
        Ok(TaskHandle { id, control })
    }

    /// Schedules a callback after every interval until cancelled or shutdown.
    /// The first callback runs after one complete interval.
    ///
    /// # Errors
    ///
    /// Returns [`MinimalSchedulerError::ZeroInterval`] for a zero interval or
    /// [`MinimalSchedulerError::Closed`] after shutdown begins.
    pub fn schedule_every<F>(
        &self,
        interval: Duration,
        priority: TaskPriority,
        callback: F,
    ) -> Result<TaskHandle, MinimalSchedulerError>
    where
        F: Fn(CancellationToken) + Send + 'static,
    {
        if interval.is_zero() {
            return Err(MinimalSchedulerError::ZeroInterval);
        }
        self.submit(priority, move |token| {
            while !token.wait_cancelled_timeout(interval) {
                callback(token.clone());
                if token.is_cancelled() {
                    break;
                }
            }
        })
    }

    /// Stops accepting work and requests cancellation for every tracked task.
    pub fn shutdown(&self) {
        let controls = {
            let mut state = lock_unpoison(&self.shared.state);
            state.accepting = false;
            state
                .controls
                .values()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        for control in controls {
            control.token.cancel();
        }
        self.shared.available.notify_all();
    }

    /// Requests cancellation and joins every worker.
    ///
    /// Tasks are cooperative: a task that ignores its token can delay drain.
    /// Calling this method a second time is harmless.
    ///
    /// # Errors
    ///
    /// Returns [`MinimalSchedulerError::WorkerPanicked`] if an internal worker
    /// panicked outside the per-task containment boundary.
    pub fn shutdown_and_drain(&self) -> Result<(), MinimalSchedulerError> {
        self.shutdown();
        let workers = {
            let mut workers = lock_unpoison(&self.workers);
            std::mem::take(&mut *workers)
        };
        let mut panicked = false;
        for worker in workers {
            panicked |= worker.join().is_err();
        }
        if panicked {
            Err(MinimalSchedulerError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}

impl Drop for MinimalScheduler {
    fn drop(&mut self) {
        let _ = self.shutdown_and_drain();
    }
}

/// A closed scheduler error.
#[derive(Debug, Error)]
pub enum MinimalSchedulerError {
    /// A scheduler requires at least one worker.
    #[error("minimal scheduler requires at least one worker")]
    ZeroWorkers,
    /// An interval task requires a non-zero duration.
    #[error("minimal scheduler interval must be non-zero")]
    ZeroInterval,
    /// Shutdown has begun and new work is rejected.
    #[error("minimal scheduler is closed")]
    Closed,
    /// An operating-system worker thread could not be created.
    #[error("minimal scheduler worker could not be created")]
    WorkerSpawn {
        /// The operating-system error, which contains no task payload.
        #[source]
        source: std::io::Error,
    },
    /// An internal worker panicked outside task containment.
    #[error("minimal scheduler worker panicked")]
    WorkerPanicked,
}

fn worker_loop(shared: &Shared) {
    loop {
        let task = {
            let mut state = lock_unpoison(&shared.state);
            loop {
                if let Some(task) = state.queue.pop() {
                    break task;
                }
                if !state.accepting {
                    return;
                }
                state = wait_unpoison(&shared.available, state);
            }
        };
        run_task(shared, task);
    }
}

fn run_task(shared: &Shared, mut task: QueuedTask) {
    let outcome = if task.control.token.is_cancelled() {
        TaskOutcome::Cancelled
    } else {
        let job = task
            .job
            .take()
            .expect("queued tasks always contain exactly one job");
        match panic::catch_unwind(AssertUnwindSafe(|| job(task.control.token.clone()))) {
            Ok(()) if task.control.token.is_cancelled() => TaskOutcome::Cancelled,
            Ok(()) => TaskOutcome::Completed,
            Err(_) => TaskOutcome::Panicked,
        }
    };
    {
        let mut stored = lock_unpoison(&task.control.outcome);
        *stored = Some(outcome);
    }
    task.control.done.notify_all();
    lock_unpoison(&shared.state).controls.remove(&task.id);
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_unpoison<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    condvar
        .wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    #[test]
    fn one_worker_honors_priority_then_fifo() {
        let scheduler = MinimalScheduler::with_worker_count(1).expect("worker should start");
        let (block_tx, block_rx) = mpsc::channel();
        let blocker = scheduler
            .submit(TaskPriority::Normal, move |_| {
                block_rx.recv().expect("test should release blocker");
            })
            .expect("blocker should submit");

        let order = Arc::new(Mutex::new(Vec::new()));
        let low_order = Arc::clone(&order);
        let low = scheduler
            .submit(TaskPriority::Low, move |_| {
                lock_unpoison(&low_order).push("low");
            })
            .expect("low task should submit");
        let high_order = Arc::clone(&order);
        let high = scheduler
            .submit(TaskPriority::High, move |_| {
                lock_unpoison(&high_order).push("high");
            })
            .expect("high task should submit");

        block_tx.send(()).expect("blocker should release");
        assert_eq!(blocker.wait(), TaskOutcome::Completed);
        assert_eq!(high.wait(), TaskOutcome::Completed);
        assert_eq!(low.wait(), TaskOutcome::Completed);
        assert_eq!(*lock_unpoison(&order), ["high", "low"]);
        scheduler
            .shutdown_and_drain()
            .expect("workers should drain");
    }

    #[test]
    fn queued_cancellation_prevents_callback_execution() {
        let scheduler = MinimalScheduler::with_worker_count(1).expect("worker should start");
        let (block_tx, block_rx) = mpsc::channel();
        let blocker = scheduler
            .submit(TaskPriority::Normal, move |_| {
                block_rx.recv().expect("test should release blocker");
            })
            .expect("blocker should submit");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let cancelled = scheduler
            .submit(TaskPriority::Normal, move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
            })
            .expect("task should submit");
        cancelled.cancel();
        block_tx.send(()).expect("blocker should release");

        assert_eq!(blocker.wait(), TaskOutcome::Completed);
        assert_eq!(cancelled.wait(), TaskOutcome::Cancelled);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        scheduler
            .shutdown_and_drain()
            .expect("workers should drain");
    }

    #[test]
    fn interval_tasks_start_after_interval_and_cancel_responsively() {
        let scheduler = MinimalScheduler::with_worker_count(1).expect("worker should start");
        let (tx, rx) = mpsc::channel();
        let recurring = scheduler
            .schedule_every(Duration::from_millis(5), TaskPriority::Low, move |_| {
                let _ = tx.send(());
            })
            .expect("interval task should submit");
        rx.recv_timeout(Duration::from_secs(2))
            .expect("interval callback should run");
        recurring.cancel();
        assert_eq!(
            recurring.wait_timeout(Duration::from_secs(2)),
            Some(TaskOutcome::Cancelled)
        );
        scheduler
            .shutdown_and_drain()
            .expect("workers should drain");
    }

    #[test]
    fn task_panics_are_contained_and_worker_continues() {
        let scheduler = MinimalScheduler::with_worker_count(1).expect("worker should start");
        let panicked = scheduler
            .submit(TaskPriority::Normal, |_| panic!("contained task panic"))
            .expect("panic task should submit");
        let next = scheduler
            .submit(TaskPriority::Normal, |_| {})
            .expect("next task should submit");
        assert_eq!(panicked.wait(), TaskOutcome::Panicked);
        assert_eq!(next.wait(), TaskOutcome::Completed);
        scheduler
            .shutdown_and_drain()
            .expect("workers should drain");
    }

    #[test]
    fn shutdown_cancels_running_cooperative_work_and_rejects_new_work() {
        let scheduler = MinimalScheduler::with_worker_count(1).expect("worker should start");
        let running = scheduler
            .submit(TaskPriority::Normal, |token| {
                let _ = token.wait_cancelled_timeout(Duration::from_secs(10));
            })
            .expect("task should submit");
        scheduler
            .shutdown_and_drain()
            .expect("workers should drain");
        assert_eq!(running.wait(), TaskOutcome::Cancelled);
        assert!(matches!(
            scheduler.submit(TaskPriority::Normal, |_| {}),
            Err(MinimalSchedulerError::Closed)
        ));
    }
}
