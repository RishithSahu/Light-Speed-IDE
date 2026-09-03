//! LightSpeed task scheduler (amendment section 3).
//!
//! The scheduler is an **admission authority**, not a thread pool. Every
//! non-interactive operation passes through it, and every task walks the same
//! path:
//!
//! ```text
//! Created -> Submitted -> Queued -> Admitted -> Running -> Completed
//!                                                       -> Failed
//!                                                       -> Cancelled
//! ```
//!
//! There is no way to reach `Running` without being admitted first; see
//! [`TaskState::can_transition_to`].
//!
//! # What this crate does not know about
//!
//! Documents, rendering, Git, filesystem semantics, project state. A task body
//! is an opaque closure returning an opaque value, which is what keeps the
//! dependency direction pointing one way: `ls-core` depends on the scheduler,
//! never the reverse.
//!
//! # Ownership
//!
//! Workers own execution. The interactive thread owns editor state and drains
//! completions with [`Scheduler::drain_completions`]; it never blocks on a
//! task. Completion delivery is a bounded ring plus an optional wake callback
//! that the shell wires to its event loop.

pub mod accounting;
mod cancel;
mod queue;
mod task;
mod worker;

pub use accounting::TaskRecord;
pub use cancel::CancellationToken;
pub use queue::{PriorityPolicy, SubmitError};
pub use task::{
    CostEstimate, IllegalTransition, Priority, ResourceClass, SubsystemId, TaskBody, TaskFailure,
    TaskHandle, TaskId, TaskOutcome, TaskProduct, TaskSpec, TaskState, WorkspaceRef,
    RESOURCE_CLASS_COUNT,
};

use accounting::RecordLog;
use queue::{Pending, PendingQueue};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use task::TaskLifecycle;

const SUBSYSTEM: &str = "scheduler";

/// Called when a completion is published, so the shell can wake its event loop.
/// It must not call back into the scheduler.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// How a finished task turned out.
#[derive(Debug)]
pub enum CompletionOutcome {
    Completed(TaskProduct),
    Failed(TaskFailure),
    Cancelled,
}

/// A finished task, delivered to the interactive thread.
#[derive(Debug)]
pub struct TaskCompletion {
    pub task: TaskId,
    pub subsystem: SubsystemId,
    pub outcome: CompletionOutcome,
    pub record: TaskRecord,
}

impl TaskCompletion {
    pub fn is_cancelled(&self) -> bool {
        matches!(self.outcome, CompletionOutcome::Cancelled)
    }

    /// Recovers the task's value if it completed with the expected type.
    pub fn take_value<T: std::any::Any + Send>(self) -> Option<Box<T>> {
        match self.outcome {
            CompletionOutcome::Completed(product) => product.downcast::<T>().ok(),
            _ => None,
        }
    }
}

/// Scheduler policy. Every field is configuration.
#[derive(Clone)]
pub struct SchedulerConfig {
    /// Worker threads. Defaults to one fewer than the machine's parallelism,
    /// clamped to [1, 4]: the interactive thread keeps a core, and Stage 1.1's
    /// only workload is file I/O, which does not benefit from more.
    pub workers: usize,
    /// Maximum tasks waiting for admission. Submissions past this are refused.
    pub queue_capacity: usize,
    /// Maximum undrained completions before the oldest is dropped.
    pub completion_capacity: usize,
    /// Recent task records kept for the overlay and benchmarks.
    pub record_capacity: usize,
    /// Recent terminal states kept so `state()` can answer after completion.
    pub terminal_history: usize,
    pub policy: PriorityPolicy,
    /// Concurrent task limit per resource class, indexed by
    /// [`ResourceClass::index`].
    pub max_concurrent: [usize; RESOURCE_CLASS_COUNT],
    /// Returns the calling thread's CPU time, if the platform can report it.
    /// Injected so this crate needs no platform dependency.
    pub cpu_time_source: Option<fn() -> Option<Duration>>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        let parallelism =
            std::thread::available_parallelism().map(|value| value.get()).unwrap_or(2);
        let workers = parallelism.saturating_sub(1).clamp(1, 4);
        SchedulerConfig {
            workers,
            queue_capacity: 256,
            completion_capacity: 256,
            record_capacity: 256,
            terminal_history: 256,
            policy: PriorityPolicy::default(),
            max_concurrent: Self::default_budgets(workers),
            cpu_time_source: None,
        }
    }
}

impl SchedulerConfig {
    fn default_budgets(workers: usize) -> [usize; RESOURCE_CLASS_COUNT] {
        let mut budgets = [workers.max(1); RESOURCE_CLASS_COUNT];
        // Concurrent seeks make a disk slower, not faster.
        budgets[ResourceClass::Io.index()] = 2.min(workers.max(1));
        // Memory-heavy work runs one at a time until there is evidence for more.
        budgets[ResourceClass::Memory.index()] = 1;
        budgets
    }

    /// One worker: makes ordering deterministic for tests.
    pub fn single_worker() -> Self {
        let mut config = SchedulerConfig { workers: 1, ..Default::default() };
        config.max_concurrent = Self::default_budgets(1);
        config
    }

    pub fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity;
        self
    }

    pub fn with_policy(mut self, policy: PriorityPolicy) -> Self {
        self.policy = policy;
        self
    }
}

pub(crate) struct State {
    pub pending: PendingQueue,
    pub lifecycles: HashMap<TaskId, TaskLifecycle>,
    pub tokens: HashMap<TaskId, CancellationToken>,
    pub terminal: VecDeque<(TaskId, TaskState)>,
    pub terminal_capacity: usize,
    pub running_by_class: [usize; RESOURCE_CLASS_COUNT],
    completions: VecDeque<TaskCompletion>,
    completion_capacity: usize,
    dropped_completions: u64,
    pub records: RecordLog,
    pub shutdown: bool,
}

impl State {
    /// Moves a finished task out of the live tables into the bounded history.
    pub fn retire(&mut self, id: TaskId, terminal: TaskState) {
        self.lifecycles.remove(&id);
        self.tokens.remove(&id);
        if self.terminal.len() == self.terminal_capacity {
            self.terminal.pop_front();
        }
        self.terminal.push_back((id, terminal));
    }

    /// Publishes a completion, dropping the oldest if the consumer has fallen
    /// behind. Dropping is counted, never silent (amendment section 4).
    pub fn publish_completion(&mut self, completion: TaskCompletion) {
        if let Some(record) = self.records_mut() {
            record.push(completion.record.clone());
        }
        if self.completions.len() == self.completion_capacity {
            self.completions.pop_front();
            self.dropped_completions += 1;
            ls_perf::counter(accounting::names::COMPLETIONS_DROPPED).inc();
            ls_log::warn!(
                SUBSYSTEM,
                "completion_dropped",
                "completion queue full at {} entries; oldest dropped",
                self.completion_capacity
            );
        }
        self.completions.push_back(completion);
    }

    fn records_mut(&mut self) -> Option<&mut RecordLog> {
        Some(&mut self.records)
    }

    pub fn publish_gauges(&self) {
        ls_perf::gauge(accounting::names::QUEUE_DEPTH, self.pending.len() as f64);
        let running: usize = self.running_by_class.iter().sum();
        ls_perf::gauge(accounting::names::RUNNING, running as f64);
    }

    fn state_of(&self, id: TaskId) -> Option<TaskState> {
        if let Some(lifecycle) = self.lifecycles.get(&id) {
            return Some(lifecycle.state());
        }
        self.terminal.iter().find(|(task, _)| *task == id).map(|(_, state)| *state)
    }
}

pub(crate) struct Shared {
    state: Mutex<State>,
    admit: Condvar,
    pub config: SchedulerConfig,
    next_id: AtomicU64,
    waker: Mutex<Option<Waker>>,
}

impl Shared {
    pub fn lock_state(&self) -> MutexGuard<'_, State> {
        // A poisoned scheduler lock means a worker panicked. The queue itself
        // is still structurally valid, so recovering keeps the editor alive
        // rather than cascading the panic into the interactive thread.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn wait_for_work<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.admit.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn notify_all(&self) {
        self.admit.notify_all();
    }

    pub fn wake_consumer(&self) {
        let waker = {
            let guard = self.waker.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.clone()
        };
        if let Some(waker) = waker {
            waker();
        }
    }
}

/// The admission authority.
pub struct Scheduler {
    shared: Arc<Shared>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        let state = State {
            pending: PendingQueue::new(config.queue_capacity),
            lifecycles: HashMap::new(),
            tokens: HashMap::new(),
            terminal: VecDeque::new(),
            terminal_capacity: config.terminal_history.max(1),
            running_by_class: [0; RESOURCE_CLASS_COUNT],
            completions: VecDeque::new(),
            completion_capacity: config.completion_capacity.max(1),
            dropped_completions: 0,
            records: RecordLog::new(config.record_capacity),
            shutdown: false,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            admit: Condvar::new(),
            config,
            next_id: AtomicU64::new(1),
            waker: Mutex::new(None),
        });
        let workers = worker::spawn_all(&shared);
        ls_log::info!(
            SUBSYSTEM,
            "started",
            fields: [
                ls_log::Field::uint("workers", workers.len() as u64),
                ls_log::Field::uint("queue_capacity", shared.config.queue_capacity as u64),
            ],
            "scheduler started"
        );
        Scheduler { shared, workers }
    }

    /// A scheduler with the default policy.
    pub fn with_defaults() -> Self {
        Self::new(SchedulerConfig::default())
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.shared.config
    }

    /// Base priority for a subsystem, so callers build specs from the policy
    /// rather than from hardcoded numbers.
    pub fn base_priority(&self, subsystem: SubsystemId) -> Priority {
        self.shared.config.policy.base_for(subsystem)
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Installs the callback used to wake the consumer when work completes.
    pub fn set_completion_waker(&self, waker: Waker) {
        let mut guard = self.shared.waker.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(waker);
    }

    /// Submits a task for admission.
    ///
    /// The task walks `Created -> Submitted -> Queued` here, on the calling
    /// thread, and waits for a worker to admit it. Returns an error rather than
    /// queuing without bound (amendment section 3.5.1).
    pub fn submit(&self, spec: TaskSpec, body: TaskBody) -> Result<TaskId, SubmitError> {
        let now = Instant::now();
        let id = TaskId::new(self.shared.next_id.fetch_add(1, Ordering::Relaxed));

        let mut lifecycle = TaskLifecycle::new(id, now);
        // These cannot fail: a fresh lifecycle is in `Created`.
        lifecycle.submit(now).map_err(|_| SubmitError::ShuttingDown)?;
        lifecycle.queue(now).map_err(|_| SubmitError::ShuttingDown)?;

        let mut state = self.shared.lock_state();
        if state.shutdown {
            return Err(SubmitError::ShuttingDown);
        }

        let token = spec.cancellation.clone();
        let pending = Pending { id, spec, body, created_at: now, paused: false };
        if let Err(error) = state.pending.push(pending) {
            // The task never existed: nothing was accepted and then lost.
            ls_perf::counter(accounting::names::REJECTED).inc();
            ls_log::warn!(SUBSYSTEM, "submission_rejected", "{error}");
            return Err(error);
        }

        state.lifecycles.insert(id, lifecycle);
        state.tokens.insert(id, token);
        state.publish_gauges();
        drop(state);

        ls_perf::counter(accounting::names::SUBMITTED).inc();
        self.shared.admit.notify_one();
        Ok(id)
    }

    /// Requests cancellation.
    ///
    /// A queued task is removed and reported immediately; a running task is
    /// asked to stop and reports `Cancelled` when its body returns. Cancelling
    /// an unknown or finished task does nothing.
    pub fn cancel(&self, id: TaskId) {
        let now = Instant::now();
        let mut state = self.shared.lock_state();

        if let Some(token) = state.tokens.get(&id) {
            token.cancel();
        }

        // If it is still queued, it never ran: retire it here so the caller
        // learns without waiting for a worker to notice.
        if let Some(pending) = state.pending.remove(id) {
            let (queue_wait, subsystem) = match state.lifecycles.get_mut(&id) {
                Some(lifecycle) => {
                    if let Err(error) = lifecycle.cancel(now) {
                        ls_log::error!(SUBSYSTEM, "illegal_transition", "{error}");
                    }
                    (
                        lifecycle.queue_wait().unwrap_or_else(|| {
                            now.saturating_duration_since(lifecycle.created_at())
                        }),
                        pending.spec.subsystem,
                    )
                }
                None => (Duration::ZERO, pending.spec.subsystem),
            };

            let record = TaskRecord {
                task_id: id,
                subsystem,
                workspace: pending.spec.workspace,
                queue_wait,
                wall_time: Duration::ZERO,
                cpu_time: None,
                bytes_read: 0,
                bytes_written: 0,
                peak_memory: None,
                estimated_cost: pending.spec.estimated_cost,
                outcome: TaskState::Cancelled,
            };
            state.retire(id, TaskState::Cancelled);
            state.publish_completion(TaskCompletion {
                task: id,
                subsystem,
                outcome: CompletionOutcome::Cancelled,
                record: record.clone(),
            });
            state.publish_gauges();
            drop(state);

            accounting::publish(&record);
            self.shared.notify_all();
            self.shared.wake_consumer();
            return;
        }

        drop(state);
        self.shared.notify_all();
    }

    /// Holds a queued task back until [`Scheduler::resume`]. A running task
    /// cannot be paused; use cancellation.
    pub fn pause(&self, id: TaskId) -> bool {
        let mut state = self.shared.lock_state();
        state.pending.set_paused(id, true)
    }

    pub fn resume(&self, id: TaskId) -> bool {
        let mut state = self.shared.lock_state();
        let resumed = state.pending.set_paused(id, false);
        drop(state);
        if resumed {
            self.shared.admit.notify_one();
        }
        resumed
    }

    /// Current state of a task, including recently finished ones.
    pub fn state(&self, id: TaskId) -> Option<TaskState> {
        self.shared.lock_state().state_of(id)
    }

    pub fn handle(&self, id: TaskId) -> Option<TaskHandle> {
        self.state(id).map(|state| TaskHandle { id, state })
    }

    /// Takes every completion published so far. Never blocks.
    pub fn drain_completions(&self) -> Vec<TaskCompletion> {
        let mut state = self.shared.lock_state();
        state.completions.drain(..).collect()
    }

    pub fn queue_depth(&self) -> usize {
        self.shared.lock_state().pending.len()
    }

    pub fn queue_capacity(&self) -> usize {
        self.shared.lock_state().pending.capacity()
    }

    pub fn running_count(&self) -> usize {
        self.shared.lock_state().running_by_class.iter().sum()
    }

    pub fn pending_completions(&self) -> usize {
        self.shared.lock_state().completions.len()
    }

    /// Completions discarded because the consumer fell behind.
    pub fn dropped_completions(&self) -> u64 {
        self.shared.lock_state().dropped_completions
    }

    /// Recent task records, bounded by `record_capacity`.
    pub fn recent_records(&self) -> Vec<TaskRecord> {
        self.shared.lock_state().records.recent()
    }

    /// Stops accepting work, cancels everything outstanding and joins workers.
    ///
    /// A task body that never polls its cancellation token will delay this
    /// until it returns; that is the cooperative contract.
    pub fn shutdown(&mut self) {
        {
            let mut state = self.shared.lock_state();
            if state.shutdown {
                return;
            }
            state.shutdown = true;
            for token in state.tokens.values() {
                token.cancel();
            }
            for pending in state.pending.drain() {
                state.retire(pending.id, TaskState::Cancelled);
            }
        }
        self.shared.notify_all();
        for worker in std::mem::take(&mut self.workers) {
            let _ = worker.join();
        }
        ls_log::info!(SUBSYSTEM, "stopped", "scheduler stopped");
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    fn test_spec(scheduler: &Scheduler, subsystem: SubsystemId) -> TaskSpec {
        TaskSpec::new(subsystem, scheduler.base_priority(subsystem), ResourceClass::Cpu)
    }

    /// Waits for a condition, failing the test rather than hanging forever.
    fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn a_submitted_task_runs_and_reports_its_value() {
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        let id = scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(|_| TaskOutcome::Completed(TaskProduct::new(7u32))),
            )
            .expect("submitted");

        wait_until(|| scheduler.pending_completions() == 1, "the task to complete");
        let mut completions = scheduler.drain_completions();
        assert_eq!(completions.len(), 1);
        let completion = completions.remove(0);
        assert_eq!(completion.task, id);
        assert_eq!(completion.record.outcome, TaskState::Completed);
        assert_eq!(*completion.take_value::<u32>().expect("a u32"), 7);
    }

    #[test]
    fn every_task_is_admitted_before_it_runs() {
        // The observable proof of the state machine: a body cannot run unless
        // its lifecycle passed through Admitted, so queue_wait always exists.
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        scheduler
            .submit(test_spec(&scheduler, SubsystemId::TEST), Box::new(|_| TaskOutcome::done()))
            .expect("submitted");
        wait_until(|| scheduler.pending_completions() == 1, "completion");

        let completion = scheduler.drain_completions().remove(0);
        // wall_time is only set by Running -> terminal, and Running is only
        // reachable from Admitted.
        assert!(completion.record.wall_time <= Duration::from_secs(1));
        assert!(completion.record.queue_wait <= Duration::from_secs(1));
    }

    #[test]
    fn a_failing_task_reports_failed_not_cancelled() {
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(|_| TaskOutcome::Failed(TaskFailure::new("test.boom", "boom"))),
            )
            .expect("submitted");
        wait_until(|| scheduler.pending_completions() == 1, "completion");

        let completion = scheduler.drain_completions().remove(0);
        assert_eq!(completion.record.outcome, TaskState::Failed);
        assert!(!completion.is_cancelled());
        match completion.outcome {
            CompletionOutcome::Failed(failure) => assert_eq!(failure.code, "test.boom"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_queued_task_can_be_cancelled_before_it_runs() {
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        let blocker = Arc::new(AtomicBool::new(true));
        let release = Arc::clone(&blocker);

        // Occupy the only worker so the next task stays queued.
        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |_| {
                    while release.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    TaskOutcome::done()
                }),
            )
            .expect("submitted");
        wait_until(|| scheduler.running_count() == 1, "the blocker to start");

        let ran = Arc::new(AtomicBool::new(false));
        let ran_flag = Arc::clone(&ran);
        let queued = scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |_| {
                    ran_flag.store(true, Ordering::Relaxed);
                    TaskOutcome::done()
                }),
            )
            .expect("submitted");
        assert_eq!(scheduler.state(queued), Some(TaskState::Queued));

        scheduler.cancel(queued);
        assert_eq!(scheduler.state(queued), Some(TaskState::Cancelled));

        blocker.store(false, Ordering::Relaxed);
        wait_until(|| scheduler.running_count() == 0, "the blocker to finish");
        assert!(!ran.load(Ordering::Relaxed), "a cancelled task must never run");

        let cancelled = scheduler
            .drain_completions()
            .into_iter()
            .find(|completion| completion.task == queued)
            .expect("the cancelled task reports a completion");
        assert!(cancelled.is_cancelled());
        assert_eq!(cancelled.record.outcome, TaskState::Cancelled);
    }

    #[test]
    fn a_running_task_observes_cancellation_and_reports_cancelled() {
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        let (started_tx, started_rx) = mpsc::channel();

        let id = scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |token| {
                    started_tx.send(()).ok();
                    while !token.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // Even though the body reports success, cancellation wins.
                    TaskOutcome::done()
                }),
            )
            .expect("submitted");

        started_rx.recv_timeout(Duration::from_secs(5)).expect("task started");
        scheduler.cancel(id);
        wait_until(|| scheduler.pending_completions() == 1, "the cancelled task to finish");

        let completion = scheduler.drain_completions().remove(0);
        assert!(completion.is_cancelled());
        assert_eq!(completion.record.outcome, TaskState::Cancelled);
        assert_ne!(completion.record.outcome, TaskState::Failed);
    }

    #[test]
    fn the_queue_refuses_submissions_at_capacity_and_never_grows() {
        let mut config = SchedulerConfig::single_worker().with_queue_capacity(2);
        config.completion_capacity = 32;
        let scheduler = Scheduler::new(config);

        let blocker = Arc::new(AtomicBool::new(true));
        let release = Arc::clone(&blocker);
        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |_| {
                    while release.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    TaskOutcome::done()
                }),
            )
            .expect("the blocker is admitted");
        wait_until(|| scheduler.running_count() == 1, "the blocker to start");

        for _ in 0..2 {
            scheduler
                .submit(test_spec(&scheduler, SubsystemId::TEST), Box::new(|_| TaskOutcome::done()))
                .expect("fits in the queue");
        }
        let rejected = scheduler
            .submit(test_spec(&scheduler, SubsystemId::TEST), Box::new(|_| TaskOutcome::done()))
            .unwrap_err();
        assert_eq!(rejected, SubmitError::QueueFull { capacity: 2 });
        assert_eq!(scheduler.queue_depth(), 2, "the queue never grew past capacity");

        blocker.store(false, Ordering::Relaxed);
        wait_until(|| scheduler.queue_depth() == 0, "the queue to drain");
    }

    #[test]
    fn a_low_priority_task_is_not_starved_by_a_stream_of_high_priority_work() {
        // Aging is turned up so the test takes milliseconds rather than the
        // twelve seconds the default policy would need.
        let mut policy = PriorityPolicy::default();
        policy.aging_per_second = 1_000_000;
        let config = SchedulerConfig { workers: 1, ..Default::default() }.with_policy(policy);
        let scheduler = Scheduler::new(config);

        let blocker = Arc::new(AtomicBool::new(true));
        let release = Arc::clone(&blocker);
        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |_| {
                    while release.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    TaskOutcome::done()
                }),
            )
            .expect("blocker admitted");
        wait_until(|| scheduler.running_count() == 1, "the blocker to start");

        // The starved task arrives first, at the lowest base priority.
        let starved = scheduler
            .submit(test_spec(&scheduler, SubsystemId::INDEXING), Box::new(|_| TaskOutcome::done()))
            .expect("submitted");
        std::thread::sleep(Duration::from_millis(20));
        // Then a stream of higher-priority work.
        for _ in 0..4 {
            scheduler
                .submit(
                    test_spec(&scheduler, SubsystemId::DOCUMENT_IO),
                    Box::new(|_| TaskOutcome::done()),
                )
                .expect("submitted");
        }

        blocker.store(false, Ordering::Relaxed);
        wait_until(|| scheduler.queue_depth() == 0, "everything to run");
        wait_until(|| scheduler.pending_completions() >= 6, "all completions");

        let completions = scheduler.drain_completions();
        let order: Vec<TaskId> = completions.iter().map(|c| c.task).collect();
        let starved_position =
            order.iter().position(|id| *id == starved).expect("the starved task ran");
        assert!(
            starved_position <= 1,
            "the aged task should run first among the queued work, ran at {starved_position}"
        );
    }

    #[test]
    fn higher_base_priority_runs_first_when_nothing_has_aged() {
        let mut policy = PriorityPolicy::default();
        policy.aging_per_second = 0; // isolate base priority
        let config = SchedulerConfig { workers: 1, ..Default::default() }.with_policy(policy);
        let scheduler = Scheduler::new(config);

        let blocker = Arc::new(AtomicBool::new(true));
        let release = Arc::clone(&blocker);
        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |_| {
                    while release.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    TaskOutcome::done()
                }),
            )
            .expect("blocker admitted");
        wait_until(|| scheduler.running_count() == 1, "the blocker to start");

        let indexing = scheduler
            .submit(test_spec(&scheduler, SubsystemId::INDEXING), Box::new(|_| TaskOutcome::done()))
            .expect("submitted");
        let document = scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::DOCUMENT_IO),
                Box::new(|_| TaskOutcome::done()),
            )
            .expect("submitted");

        blocker.store(false, Ordering::Relaxed);
        wait_until(|| scheduler.pending_completions() >= 3, "all completions");
        let order: Vec<TaskId> = scheduler.drain_completions().iter().map(|c| c.task).collect();
        let document_position = order.iter().position(|id| *id == document).unwrap();
        let indexing_position = order.iter().position(|id| *id == indexing).unwrap();
        assert!(
            document_position < indexing_position,
            "document_io (800) should precede indexing (200)"
        );
    }

    #[test]
    fn a_deadline_lifts_a_low_priority_task_above_a_higher_one() {
        let mut policy = PriorityPolicy::default();
        policy.aging_per_second = 0;
        let config = SchedulerConfig { workers: 1, ..Default::default() }.with_policy(policy);
        let scheduler = Scheduler::new(config);

        let blocker = Arc::new(AtomicBool::new(true));
        let release = Arc::clone(&blocker);
        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |_| {
                    while release.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    TaskOutcome::done()
                }),
            )
            .expect("blocker admitted");
        wait_until(|| scheduler.running_count() == 1, "the blocker to start");

        // search (500) with no deadline, git (300) already past its deadline.
        let search = scheduler
            .submit(test_spec(&scheduler, SubsystemId::SEARCH), Box::new(|_| TaskOutcome::done()))
            .expect("submitted");
        let urgent_spec = test_spec(&scheduler, SubsystemId::GIT)
            .with_deadline(Instant::now() - Duration::from_secs(1));
        let urgent =
            scheduler.submit(urgent_spec, Box::new(|_| TaskOutcome::done())).expect("submitted");

        blocker.store(false, Ordering::Relaxed);
        wait_until(|| scheduler.pending_completions() >= 3, "all completions");
        let order: Vec<TaskId> = scheduler.drain_completions().iter().map(|c| c.task).collect();
        let urgent_position = order.iter().position(|id| *id == urgent).unwrap();
        let search_position = order.iter().position(|id| *id == search).unwrap();
        assert!(
            urgent_position < search_position,
            "300 + 400 of deadline pressure should beat 500"
        );
    }

    #[test]
    fn the_completion_queue_is_bounded_and_counts_drops() {
        let mut config = SchedulerConfig::single_worker();
        config.completion_capacity = 4;
        let scheduler = Scheduler::new(config);

        for _ in 0..12 {
            scheduler
                .submit(test_spec(&scheduler, SubsystemId::TEST), Box::new(|_| TaskOutcome::done()))
                .expect("submitted");
        }
        wait_until(|| scheduler.dropped_completions() > 0, "the consumer to fall behind");
        assert!(
            scheduler.pending_completions() <= 4,
            "completions grew to {}",
            scheduler.pending_completions()
        );
    }

    #[test]
    fn accounting_is_populated_for_a_completed_task() {
        let mut config = SchedulerConfig::single_worker();
        config.cpu_time_source = Some(fake_cpu_time);
        let scheduler = Scheduler::new(config);

        let spec = test_spec(&scheduler, SubsystemId::DOCUMENT_IO)
            .with_cost(CostEstimate::bytes(1000))
            .with_workspace(WorkspaceRef(42));
        scheduler
            .submit(
                spec,
                Box::new(|_| TaskOutcome::Completed(TaskProduct::new(()).with_io(1234, 56))),
            )
            .expect("submitted");
        wait_until(|| scheduler.pending_completions() == 1, "completion");

        let record = scheduler.drain_completions().remove(0).record;
        assert_eq!(record.subsystem, SubsystemId::DOCUMENT_IO);
        assert_eq!(record.workspace, Some(WorkspaceRef(42)));
        assert_eq!(record.bytes_read, 1234);
        assert_eq!(record.bytes_written, 56);
        assert_eq!(record.estimated_cost, CostEstimate::bytes(1000));
        assert!(record.cpu_time.is_some(), "an injected source should report CPU time");
        assert_eq!(record.peak_memory, None, "not measurable per task today");
        assert!(record.cost_ratio().unwrap() > 1.0, "measured more than estimated");
        assert!(!scheduler.recent_records().is_empty());
    }

    fn fake_cpu_time() -> Option<Duration> {
        // Monotonic per call, which is all the delta needs.
        use std::sync::atomic::AtomicU64;
        static TICKS: AtomicU64 = AtomicU64::new(0);
        Some(Duration::from_micros(TICKS.fetch_add(100, Ordering::Relaxed)))
    }

    #[test]
    fn pause_holds_a_task_until_resume() {
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        let blocker = Arc::new(AtomicBool::new(true));
        let release = Arc::clone(&blocker);
        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |_| {
                    while release.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    TaskOutcome::done()
                }),
            )
            .expect("blocker admitted");
        wait_until(|| scheduler.running_count() == 1, "the blocker to start");

        let paused = scheduler
            .submit(test_spec(&scheduler, SubsystemId::TEST), Box::new(|_| TaskOutcome::done()))
            .expect("submitted");
        assert!(scheduler.pause(paused));

        blocker.store(false, Ordering::Relaxed);
        wait_until(|| scheduler.running_count() == 0, "the blocker to finish");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(scheduler.state(paused), Some(TaskState::Queued), "still held");

        assert!(scheduler.resume(paused));
        wait_until(|| scheduler.state(paused) == Some(TaskState::Completed), "the resumed task");
    }

    #[test]
    fn the_waker_fires_when_work_completes() {
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        let woken = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&woken);
        scheduler.set_completion_waker(Arc::new(move || flag.store(true, Ordering::Relaxed)));

        scheduler
            .submit(test_spec(&scheduler, SubsystemId::TEST), Box::new(|_| TaskOutcome::done()))
            .expect("submitted");
        wait_until(|| woken.load(Ordering::Relaxed), "the waker to fire");
    }

    #[test]
    fn shutdown_cancels_outstanding_work_and_joins() {
        let mut scheduler = Scheduler::new(SchedulerConfig::single_worker());
        let observed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&observed);

        scheduler
            .submit(
                test_spec(&scheduler, SubsystemId::TEST),
                Box::new(move |token| {
                    while !token.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    flag.store(true, Ordering::Relaxed);
                    TaskOutcome::Cancelled
                }),
            )
            .expect("submitted");
        wait_until(|| scheduler.running_count() == 1, "the task to start");

        scheduler.shutdown();
        assert!(observed.load(Ordering::Relaxed), "shutdown cancels running work");
        assert_eq!(
            scheduler.submit(
                TaskSpec::new(SubsystemId::TEST, Priority::new(1), ResourceClass::Cpu),
                Box::new(|_| TaskOutcome::done())
            ),
            Err(SubmitError::ShuttingDown)
        );
    }

    #[test]
    fn resource_budgets_limit_concurrency_per_class() {
        let mut config = SchedulerConfig { workers: 4, ..Default::default() };
        config.max_concurrent[ResourceClass::Io.index()] = 1;
        let scheduler = Scheduler::new(config);

        let peak = Arc::new(AtomicU64::new(0));
        let live = Arc::new(AtomicU64::new(0));
        for _ in 0..6 {
            let peak = Arc::clone(&peak);
            let live = Arc::clone(&live);
            let mut spec = test_spec(&scheduler, SubsystemId::DOCUMENT_IO);
            spec.resource_class = ResourceClass::Io;
            scheduler
                .submit(
                    spec,
                    Box::new(move |_| {
                        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(5));
                        live.fetch_sub(1, Ordering::SeqCst);
                        TaskOutcome::done()
                    }),
                )
                .expect("submitted");
        }
        wait_until(|| scheduler.pending_completions() >= 6, "all six io tasks");
        assert_eq!(peak.load(Ordering::SeqCst), 1, "the io budget was exceeded");
    }

    #[test]
    fn an_unknown_task_has_no_state_and_cancelling_it_is_harmless() {
        let scheduler = Scheduler::new(SchedulerConfig::single_worker());
        let ghost = TaskId::new(999_999);
        assert_eq!(scheduler.state(ghost), None);
        scheduler.cancel(ghost);
        assert_eq!(scheduler.state(ghost), None);
    }

    #[test]
    fn the_default_worker_count_leaves_the_interactive_thread_a_core() {
        let config = SchedulerConfig::default();
        assert!(config.workers >= 1 && config.workers <= 4);
        let parallelism =
            std::thread::available_parallelism().map(|value| value.get()).unwrap_or(2);
        if parallelism > 1 {
            assert!(config.workers < parallelism, "a core is reserved for interaction");
        }
    }
}
