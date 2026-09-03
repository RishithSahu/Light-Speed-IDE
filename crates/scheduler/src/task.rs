//! The task model and its state machine (amendment sections 3.1, 3.2).
//!
//! The state machine is the point of this module. `Created → Running` is not a
//! transition anyone forgot to write: it is rejected by
//! [`TaskState::can_transition_to`], and every mutation on [`TaskLifecycle`]
//! goes through that check, so there is no path through the API that skips
//! admission.

use crate::cancel::CancellationToken;
use std::any::Any;
use std::fmt;
use std::time::{Duration, Instant};

/// Identifier of one submitted task.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(u64);

impl TaskId {
    pub const fn new(value: u64) -> Self {
        TaskId(value)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task#{}", self.0)
    }
}

/// The subsystem that owns a task. The scheduler treats this as an opaque
/// label: it is the key for accounting and for the base-priority policy, and
/// carries no behaviour.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubsystemId(pub &'static str);

impl SubsystemId {
    pub const DOCUMENT_IO: SubsystemId = SubsystemId("document_io");
    pub const SEARCH: SubsystemId = SubsystemId("search");
    pub const GIT: SubsystemId = SubsystemId("git");
    pub const LANGUAGE: SubsystemId = SubsystemId("language");
    pub const INDEXING: SubsystemId = SubsystemId("indexing");
    /// For tests and benchmarks.
    pub const TEST: SubsystemId = SubsystemId("test");

    pub const fn name(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for SubsystemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Base priority. Larger runs sooner, all else being equal.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(i64);

impl Priority {
    pub const fn new(value: i64) -> Self {
        Priority(value)
    }
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// What a task will contend for. Admission limits concurrency per class, so
/// eight queued reads do not become eight simultaneous seeks.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResourceClass {
    Cpu,
    Io,
    Memory,
    Process,
}

pub const RESOURCE_CLASS_COUNT: usize = 4;

impl ResourceClass {
    pub const ALL: [ResourceClass; RESOURCE_CLASS_COUNT] =
        [ResourceClass::Cpu, ResourceClass::Io, ResourceClass::Memory, ResourceClass::Process];

    pub const fn index(self) -> usize {
        match self {
            ResourceClass::Cpu => 0,
            ResourceClass::Io => 1,
            ResourceClass::Memory => 2,
            ResourceClass::Process => 3,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            ResourceClass::Cpu => "cpu",
            ResourceClass::Io => "io",
            ResourceClass::Memory => "memory",
            ResourceClass::Process => "process",
        }
    }
}

/// The submitter's honest guess at what the task will cost. Compared against
/// the measured cost afterwards, which is what lets estimates improve.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CostEstimate {
    pub bytes: u64,
    pub items: u64,
}

impl CostEstimate {
    pub const NEGLIGIBLE: CostEstimate = CostEstimate { bytes: 0, items: 0 };

    pub const fn bytes(bytes: u64) -> Self {
        CostEstimate { bytes, items: 1 }
    }
}

/// Opaque workspace identifier, carried only so accounting can attribute a
/// task. The scheduler never interprets it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct WorkspaceRef(pub u64);

/// Everything the scheduler needs to know about a task before running it.
#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub subsystem: SubsystemId,
    pub priority: Priority,
    pub resource_class: ResourceClass,
    pub estimated_cost: CostEstimate,
    pub deadline: Option<Instant>,
    pub cancellation: CancellationToken,
    pub workspace: Option<WorkspaceRef>,
}

impl TaskSpec {
    /// A spec with no deadline, no workspace and a negligible cost estimate.
    pub fn new(subsystem: SubsystemId, priority: Priority, resource_class: ResourceClass) -> Self {
        TaskSpec {
            subsystem,
            priority,
            resource_class,
            estimated_cost: CostEstimate::NEGLIGIBLE,
            deadline: None,
            cancellation: CancellationToken::new(),
            workspace: None,
        }
    }

    pub fn with_cost(mut self, estimate: CostEstimate) -> Self {
        self.estimated_cost = estimate;
        self
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_workspace(mut self, workspace: WorkspaceRef) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = token;
        self
    }
}

/// Where a task is in its life (amendment section 3.2).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TaskState {
    Created,
    Submitted,
    Queued,
    Admitted,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub const ALL: [TaskState; 8] = [
        TaskState::Created,
        TaskState::Submitted,
        TaskState::Queued,
        TaskState::Admitted,
        TaskState::Running,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Cancelled,
    ];

    pub const fn is_terminal(self) -> bool {
        matches!(self, TaskState::Completed | TaskState::Failed | TaskState::Cancelled)
    }

    pub const fn name(self) -> &'static str {
        match self {
            TaskState::Created => "created",
            TaskState::Submitted => "submitted",
            TaskState::Queued => "queued",
            TaskState::Admitted => "admitted",
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    /// The complete transition table. Everything not listed here is illegal —
    /// in particular `Created → Running`, which is what admission exists to
    /// prevent.
    pub const fn can_transition_to(self, next: TaskState) -> bool {
        matches!(
            (self, next),
            (TaskState::Created, TaskState::Submitted)
                | (TaskState::Submitted, TaskState::Queued)
                | (TaskState::Queued, TaskState::Admitted)
                | (TaskState::Admitted, TaskState::Running)
                | (TaskState::Running, TaskState::Completed)
                | (TaskState::Running, TaskState::Failed)
                | (TaskState::Running, TaskState::Cancelled)
                // Cancellation is legal from every state before completion.
                | (TaskState::Created, TaskState::Cancelled)
                | (TaskState::Submitted, TaskState::Cancelled)
                | (TaskState::Queued, TaskState::Cancelled)
                | (TaskState::Admitted, TaskState::Cancelled)
        )
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An attempted transition the state machine does not allow.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IllegalTransition {
    pub task: TaskId,
    pub from: TaskState,
    pub to: TaskState,
}

impl fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} cannot move from {} to {}", self.task, self.from, self.to)
    }
}

impl std::error::Error for IllegalTransition {}

/// A task's state plus the timestamps accounting needs.
///
/// `queue_wait` runs from creation to admission; `wall_time` runs from the
/// start of execution to a terminal state (amendment section 3.2).
#[derive(Clone, Debug)]
pub struct TaskLifecycle {
    id: TaskId,
    state: TaskState,
    created_at: Instant,
    admitted_at: Option<Instant>,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
}

impl TaskLifecycle {
    pub fn new(id: TaskId, now: Instant) -> Self {
        TaskLifecycle {
            id,
            state: TaskState::Created,
            created_at: now,
            admitted_at: None,
            started_at: None,
            finished_at: None,
        }
    }

    pub fn state(&self) -> TaskState {
        self.state
    }

    fn move_to(&mut self, next: TaskState, now: Instant) -> Result<(), IllegalTransition> {
        if !self.state.can_transition_to(next) {
            return Err(IllegalTransition { task: self.id, from: self.state, to: next });
        }
        match next {
            TaskState::Admitted => self.admitted_at = Some(now),
            TaskState::Running => self.started_at = Some(now),
            state if state.is_terminal() => self.finished_at = Some(now),
            _ => {}
        }
        self.state = next;
        Ok(())
    }

    pub fn submit(&mut self, now: Instant) -> Result<(), IllegalTransition> {
        self.move_to(TaskState::Submitted, now)
    }

    pub fn queue(&mut self, now: Instant) -> Result<(), IllegalTransition> {
        self.move_to(TaskState::Queued, now)
    }

    /// Admission: the scheduler has decided resources exist for this task now.
    /// Queue waiting stops here.
    pub fn admit(&mut self, now: Instant) -> Result<(), IllegalTransition> {
        self.move_to(TaskState::Admitted, now)
    }

    pub fn start(&mut self, now: Instant) -> Result<(), IllegalTransition> {
        self.move_to(TaskState::Running, now)
    }

    pub fn complete(&mut self, now: Instant) -> Result<(), IllegalTransition> {
        self.move_to(TaskState::Completed, now)
    }

    pub fn fail(&mut self, now: Instant) -> Result<(), IllegalTransition> {
        self.move_to(TaskState::Failed, now)
    }

    pub fn cancel(&mut self, now: Instant) -> Result<(), IllegalTransition> {
        self.move_to(TaskState::Cancelled, now)
    }

    /// Time from creation to admission. `None` until the task is admitted.
    pub fn queue_wait(&self) -> Option<Duration> {
        self.admitted_at.map(|admitted| admitted.saturating_duration_since(self.created_at))
    }

    /// Time from the start of execution to a terminal state. `None` until the
    /// task has both started and finished.
    pub fn wall_time(&self) -> Option<Duration> {
        match (self.started_at, self.finished_at) {
            (Some(started), Some(finished)) => Some(finished.saturating_duration_since(started)),
            _ => None,
        }
    }

    pub fn created_at(&self) -> Instant {
        self.created_at
    }
}

/// What a caller can observe about a task.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TaskHandle {
    pub id: TaskId,
    pub state: TaskState,
}

/// What a successful task produced, plus the I/O it did.
///
/// The value is `Any` so the scheduler stays ignorant of what subsystems
/// compute; the submitter downcasts it on completion.
pub struct TaskProduct {
    pub value: Box<dyn Any + Send>,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

impl TaskProduct {
    pub fn new<T: Any + Send>(value: T) -> Self {
        TaskProduct { value: Box::new(value), bytes_read: 0, bytes_written: 0 }
    }

    pub fn with_io(mut self, bytes_read: u64, bytes_written: u64) -> Self {
        self.bytes_read = bytes_read;
        self.bytes_written = bytes_written;
        self
    }

    /// Recovers the concrete value, or gives the box back if it is another type.
    pub fn downcast<T: Any + Send>(self) -> Result<Box<T>, Box<dyn Any + Send>> {
        self.value.downcast::<T>()
    }
}

impl fmt::Debug for TaskProduct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskProduct")
            .field("bytes_read", &self.bytes_read)
            .field("bytes_written", &self.bytes_written)
            .finish_non_exhaustive()
    }
}

/// Why a task failed. Failure is a real outcome; cancellation is not a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskFailure {
    pub code: &'static str,
    pub message: String,
}

impl TaskFailure {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        TaskFailure { code, message: message.into() }
    }
}

impl fmt::Display for TaskFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// What a task body returns.
#[derive(Debug)]
pub enum TaskOutcome {
    Completed(TaskProduct),
    Failed(TaskFailure),
    Cancelled,
}

impl TaskOutcome {
    /// Convenience for a task with nothing to return.
    pub fn done() -> Self {
        TaskOutcome::Completed(TaskProduct::new(()))
    }

    pub fn terminal_state(&self) -> TaskState {
        match self {
            TaskOutcome::Completed(_) => TaskState::Completed,
            TaskOutcome::Failed(_) => TaskState::Failed,
            TaskOutcome::Cancelled => TaskState::Cancelled,
        }
    }
}

/// The work itself. The scheduler owns invocation; a body receives only its
/// cancellation token and must not touch editor or UI state.
pub type TaskBody = Box<dyn FnOnce(&CancellationToken) -> TaskOutcome + Send>;

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle() -> TaskLifecycle {
        TaskLifecycle::new(TaskId::new(1), Instant::now())
    }

    /// The transition table, written out independently of the implementation.
    fn legal_pairs() -> Vec<(TaskState, TaskState)> {
        use TaskState::*;
        vec![
            (Created, Submitted),
            (Submitted, Queued),
            (Queued, Admitted),
            (Admitted, Running),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Created, Cancelled),
            (Submitted, Cancelled),
            (Queued, Cancelled),
            (Admitted, Cancelled),
        ]
    }

    #[test]
    fn created_cannot_reach_running_directly() {
        assert!(!TaskState::Created.can_transition_to(TaskState::Running));
        let mut task = lifecycle();
        let error = task.start(Instant::now()).unwrap_err();
        assert_eq!(error.from, TaskState::Created);
        assert_eq!(error.to, TaskState::Running);
        assert_eq!(task.state(), TaskState::Created, "a rejected transition changes nothing");
    }

    #[test]
    fn every_state_pair_matches_the_transition_table() {
        let legal = legal_pairs();
        for from in TaskState::ALL {
            for to in TaskState::ALL {
                let expected = legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expected,
                    "{from} -> {to} should be {}",
                    if expected { "legal" } else { "illegal" }
                );
            }
        }
    }

    #[test]
    fn terminal_states_are_final() {
        for terminal in [TaskState::Completed, TaskState::Failed, TaskState::Cancelled] {
            assert!(terminal.is_terminal());
            for to in TaskState::ALL {
                assert!(!terminal.can_transition_to(to), "{terminal} -> {to} must be illegal");
            }
        }
    }

    #[test]
    fn the_happy_path_runs_in_order() {
        let now = Instant::now();
        let mut task = lifecycle();
        task.submit(now).unwrap();
        task.queue(now).unwrap();
        task.admit(now).unwrap();
        task.start(now).unwrap();
        task.complete(now).unwrap();
        assert_eq!(task.state(), TaskState::Completed);
    }

    #[test]
    fn queue_wait_stops_at_admission_and_wall_time_starts_at_running() {
        let start = Instant::now();
        let mut task = TaskLifecycle::new(TaskId::new(7), start);
        task.submit(start).unwrap();
        task.queue(start).unwrap();
        assert_eq!(task.queue_wait(), None, "not admitted yet");

        let admitted = start + Duration::from_millis(30);
        task.admit(admitted).unwrap();
        assert_eq!(task.queue_wait(), Some(Duration::from_millis(30)));
        assert_eq!(task.wall_time(), None, "not started yet");

        let started = start + Duration::from_millis(31);
        task.start(started).unwrap();
        assert_eq!(task.wall_time(), None, "not finished yet");

        let finished = start + Duration::from_millis(50);
        task.complete(finished).unwrap();
        assert_eq!(task.wall_time(), Some(Duration::from_millis(19)));
        // Admission froze the queue wait: later work does not extend it.
        assert_eq!(task.queue_wait(), Some(Duration::from_millis(30)));
    }

    #[test]
    fn cancellation_is_legal_from_every_pre_terminal_state() {
        let now = Instant::now();

        let mut from_created = lifecycle();
        assert!(from_created.cancel(now).is_ok());

        let mut from_submitted = lifecycle();
        from_submitted.submit(now).unwrap();
        assert!(from_submitted.cancel(now).is_ok());

        let mut from_queued = lifecycle();
        from_queued.submit(now).unwrap();
        from_queued.queue(now).unwrap();
        assert!(from_queued.cancel(now).is_ok());

        let mut from_admitted = lifecycle();
        from_admitted.submit(now).unwrap();
        from_admitted.queue(now).unwrap();
        from_admitted.admit(now).unwrap();
        assert!(from_admitted.cancel(now).is_ok());

        let mut from_running = lifecycle();
        from_running.submit(now).unwrap();
        from_running.queue(now).unwrap();
        from_running.admit(now).unwrap();
        from_running.start(now).unwrap();
        assert!(from_running.cancel(now).is_ok());
        assert_eq!(from_running.state(), TaskState::Cancelled);
    }

    #[test]
    fn a_cancelled_task_is_not_a_failed_task() {
        assert_ne!(TaskState::Cancelled, TaskState::Failed);
        let now = Instant::now();
        let mut task = lifecycle();
        task.submit(now).unwrap();
        task.queue(now).unwrap();
        task.cancel(now).unwrap();
        assert_eq!(task.state(), TaskState::Cancelled);
        assert!(task.fail(now).is_err(), "a cancelled task cannot later become failed");
    }

    #[test]
    fn outcomes_map_to_terminal_states() {
        assert_eq!(TaskOutcome::done().terminal_state(), TaskState::Completed);
        assert_eq!(
            TaskOutcome::Failed(TaskFailure::new("x.y", "boom")).terminal_state(),
            TaskState::Failed
        );
        assert_eq!(TaskOutcome::Cancelled.terminal_state(), TaskState::Cancelled);
    }

    #[test]
    fn a_product_round_trips_through_any() {
        let product = TaskProduct::new(42u32).with_io(10, 20);
        assert_eq!(product.bytes_read, 10);
        assert_eq!(product.bytes_written, 20);
        assert_eq!(*product.downcast::<u32>().expect("same type"), 42);

        let product = TaskProduct::new("text");
        assert!(product.downcast::<u32>().is_err(), "a wrong downcast returns the value");
    }

    #[test]
    fn resource_class_indices_are_distinct_and_in_range() {
        let mut seen = [false; RESOURCE_CLASS_COUNT];
        for class in ResourceClass::ALL {
            let index = class.index();
            assert!(index < RESOURCE_CLASS_COUNT);
            assert!(!seen[index], "duplicate index for {}", class.name());
            seen[index] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn spec_builders_set_what_they_say() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let spec = TaskSpec::new(SubsystemId::SEARCH, Priority::new(500), ResourceClass::Cpu)
            .with_cost(CostEstimate::bytes(4096))
            .with_deadline(deadline)
            .with_workspace(WorkspaceRef(3));
        assert_eq!(spec.subsystem, SubsystemId::SEARCH);
        assert_eq!(spec.estimated_cost.bytes, 4096);
        assert_eq!(spec.deadline, Some(deadline));
        assert_eq!(spec.workspace, Some(WorkspaceRef(3)));
        assert!(!spec.cancellation.is_cancelled());
    }
}
