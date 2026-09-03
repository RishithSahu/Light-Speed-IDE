//! Admission queue and fairness (amendment sections 3.5.1, 5).
//!
//! ```text
//! effective_priority = base_priority + aging + deadline_pressure
//! ```
//!
//! The arithmetic is deliberately plain. Aging is what stops a low-priority
//! task from waiting forever behind a stream of higher-priority arrivals, and
//! deadline pressure is what lets a task with a real deadline move up as that
//! deadline approaches. Neither changes *entitlement*: resource budgets are
//! enforced separately, so an aged-up background task still cannot take
//! capacity that interactive latency depends on.

use crate::task::{Priority, ResourceClass, SubsystemId, TaskBody, TaskId, TaskSpec};
use std::time::{Duration, Instant};

/// Base priorities and the two adjustment terms. Policy, not system truth:
/// every value here is configuration (amendment section 5).
#[derive(Clone, Debug)]
pub struct PriorityPolicy {
    bases: Vec<(SubsystemId, Priority)>,
    default_base: Priority,
    /// Priority added per second spent waiting.
    pub aging_per_second: i64,
    /// How far ahead of a deadline pressure starts to build.
    pub deadline_horizon: Duration,
    /// Pressure added at (or past) the deadline.
    pub deadline_max_pressure: i64,
}

impl Default for PriorityPolicy {
    /// The amendment's table. `USER INPUT` (1000) and `RENDER` (900) are
    /// deliberately absent: interactive work never enters the queue, so giving
    /// it a queue priority would imply it might.
    fn default() -> Self {
        PriorityPolicy {
            bases: vec![
                (SubsystemId::DOCUMENT_IO, Priority::new(800)),
                (SubsystemId::LANGUAGE, Priority::new(700)),
                (SubsystemId::SEARCH, Priority::new(500)),
                (SubsystemId::GIT, Priority::new(300)),
                (SubsystemId::INDEXING, Priority::new(200)),
            ],
            default_base: Priority::new(400),
            aging_per_second: 50,
            deadline_horizon: Duration::from_secs(2),
            deadline_max_pressure: 400,
        }
    }
}

impl PriorityPolicy {
    /// Base priority for a subsystem, or the default for one with no entry.
    pub fn base_for(&self, subsystem: SubsystemId) -> Priority {
        self.bases
            .iter()
            .find(|(id, _)| *id == subsystem)
            .map(|(_, priority)| *priority)
            .unwrap_or(self.default_base)
    }

    /// Overrides or adds a base priority.
    pub fn set_base(&mut self, subsystem: SubsystemId, priority: Priority) {
        match self.bases.iter_mut().find(|(id, _)| *id == subsystem) {
            Some(entry) => entry.1 = priority,
            None => self.bases.push((subsystem, priority)),
        }
    }

    /// Priority added for having waited since `created_at`.
    pub fn aging(&self, created_at: Instant, now: Instant) -> i64 {
        let waited = now.saturating_duration_since(created_at).as_secs_f64();
        (waited * self.aging_per_second as f64) as i64
    }

    /// Priority added for approaching a deadline. Zero when there is no
    /// deadline, and zero while the deadline is further away than the horizon.
    pub fn deadline_pressure(&self, deadline: Option<Instant>, now: Instant) -> i64 {
        let Some(deadline) = deadline else { return 0 };
        let remaining = deadline.saturating_duration_since(now);
        if remaining >= self.deadline_horizon {
            return 0;
        }
        let horizon = self.deadline_horizon.as_secs_f64();
        if horizon <= 0.0 {
            return self.deadline_max_pressure;
        }
        let closeness = 1.0 - (remaining.as_secs_f64() / horizon);
        (closeness * self.deadline_max_pressure as f64) as i64
    }

    /// The whole formula.
    pub fn effective_priority(&self, spec: &TaskSpec, created_at: Instant, now: Instant) -> i64 {
        spec.priority.get()
            + self.aging(created_at, now)
            + self.deadline_pressure(spec.deadline, now)
    }
}

/// A task waiting for admission.
pub(crate) struct Pending {
    pub id: TaskId,
    pub spec: TaskSpec,
    pub body: TaskBody,
    pub created_at: Instant,
    pub paused: bool,
}

/// Hand-written because a task body is a closure and cannot be `Debug`.
impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pending")
            .field("id", &self.id)
            .field("subsystem", &self.spec.subsystem)
            .field("priority", &self.spec.priority)
            .field("resource_class", &self.spec.resource_class)
            .field("paused", &self.paused)
            .finish_non_exhaustive()
    }
}

/// Why a submission was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The queue is at capacity. The task was never created, so nothing was
    /// lost silently (amendment section 3.5.1).
    QueueFull { capacity: usize },
    /// The scheduler is shutting down.
    ShuttingDown,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::QueueFull { capacity } => {
                write!(f, "the scheduler queue is full ({capacity} tasks)")
            }
            SubmitError::ShuttingDown => f.write_str("the scheduler is shutting down"),
        }
    }
}

impl std::error::Error for SubmitError {}

/// A bounded set of pending tasks.
///
/// A `Vec` scanned linearly, deliberately: capacity is small by construction,
/// and a heap would have to be rebuilt on every tick anyway because effective
/// priority changes with time.
pub(crate) struct PendingQueue {
    entries: Vec<Pending>,
    capacity: usize,
}

impl PendingQueue {
    pub fn new(capacity: usize) -> Self {
        PendingQueue { entries: Vec::new(), capacity: capacity.max(1) }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Adds a task, or refuses when full. Never grows past capacity.
    ///
    /// A refused task is dropped here rather than handed back: the contract is
    /// that a rejected submission never became a task at all (amendment
    /// section 3.5.1), so there is nothing for the caller to own.
    pub fn push(&mut self, pending: Pending) -> Result<(), SubmitError> {
        if self.entries.len() >= self.capacity {
            return Err(SubmitError::QueueFull { capacity: self.capacity });
        }
        self.entries.push(pending);
        Ok(())
    }

    pub fn remove(&mut self, id: TaskId) -> Option<Pending> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(index))
    }

    pub fn set_paused(&mut self, id: TaskId, paused: bool) -> bool {
        match self.entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.paused = paused;
                true
            }
            None => false,
        }
    }

    /// Takes the highest-priority task that is runnable right now.
    ///
    /// `admissible` decides whether a resource class has capacity; that is the
    /// budget check, kept separate from priority so that fairness cannot spend
    /// capacity fairness is not entitled to.
    pub fn take_best(
        &mut self,
        now: Instant,
        policy: &PriorityPolicy,
        admissible: impl Fn(ResourceClass) -> bool,
    ) -> Option<Pending> {
        let mut best: Option<(usize, i64)> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.paused || !admissible(entry.spec.resource_class) {
                continue;
            }
            let score = policy.effective_priority(&entry.spec, entry.created_at, now);
            match best {
                Some((_, best_score)) if best_score >= score => {}
                _ => best = Some((index, score)),
            }
        }
        best.map(|(index, _)| self.entries.remove(index))
    }

    /// Drains everything, for shutdown.
    pub fn drain(&mut self) -> Vec<Pending> {
        std::mem::take(&mut self.entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{CostEstimate, TaskOutcome};

    fn spec(subsystem: SubsystemId, policy: &PriorityPolicy) -> TaskSpec {
        TaskSpec::new(subsystem, policy.base_for(subsystem), ResourceClass::Io)
    }

    fn pending(id: u64, spec: TaskSpec, created_at: Instant) -> Pending {
        Pending {
            id: TaskId::new(id),
            spec,
            body: Box::new(|_| TaskOutcome::done()),
            created_at,
            paused: false,
        }
    }

    #[test]
    fn the_default_policy_matches_the_amendment_table() {
        let policy = PriorityPolicy::default();
        assert_eq!(policy.base_for(SubsystemId::DOCUMENT_IO), Priority::new(800));
        assert_eq!(policy.base_for(SubsystemId::LANGUAGE), Priority::new(700));
        assert_eq!(policy.base_for(SubsystemId::SEARCH), Priority::new(500));
        assert_eq!(policy.base_for(SubsystemId::GIT), Priority::new(300));
        assert_eq!(policy.base_for(SubsystemId::INDEXING), Priority::new(200));
    }

    #[test]
    fn base_priorities_are_configurable() {
        let mut policy = PriorityPolicy::default();
        policy.set_base(SubsystemId::INDEXING, Priority::new(950));
        assert_eq!(policy.base_for(SubsystemId::INDEXING), Priority::new(950));

        policy.set_base(SubsystemId("plugins"), Priority::new(120));
        assert_eq!(policy.base_for(SubsystemId("plugins")), Priority::new(120));
    }

    #[test]
    fn an_unknown_subsystem_gets_the_default_base() {
        let policy = PriorityPolicy::default();
        assert_eq!(policy.base_for(SubsystemId("unheard_of")), Priority::new(400));
    }

    #[test]
    fn aging_grows_with_waiting() {
        let policy = PriorityPolicy::default();
        let start = Instant::now();
        assert_eq!(policy.aging(start, start), 0);
        assert_eq!(policy.aging(start, start + Duration::from_secs(1)), 50);
        assert_eq!(policy.aging(start, start + Duration::from_secs(10)), 500);
    }

    #[test]
    fn deadline_pressure_is_zero_without_a_deadline() {
        let policy = PriorityPolicy::default();
        assert_eq!(policy.deadline_pressure(None, Instant::now()), 0);
    }

    #[test]
    fn deadline_pressure_builds_inside_the_horizon() {
        let policy = PriorityPolicy::default();
        let now = Instant::now();

        // Far away: no pressure at all.
        assert_eq!(policy.deadline_pressure(Some(now + Duration::from_secs(60)), now), 0);
        // Half the horizon away: half the pressure.
        let half = policy.deadline_pressure(Some(now + Duration::from_secs(1)), now);
        assert!((190..=210).contains(&half), "expected about 200, got {half}");
        // Past due: full pressure.
        assert_eq!(policy.deadline_pressure(Some(now - Duration::from_secs(1)), now), 400);
    }

    #[test]
    fn effective_priority_is_the_sum_of_all_three_terms() {
        let policy = PriorityPolicy::default();
        let created = Instant::now();
        let now = created + Duration::from_secs(2);
        let mut task = spec(SubsystemId::INDEXING, &policy);
        task.deadline = Some(now); // past due at `now`: full pressure

        let expected = 200 + 100 + 400;
        assert_eq!(policy.effective_priority(&task, created, now), expected);
    }

    #[test]
    fn the_queue_refuses_to_grow_past_capacity() {
        let policy = PriorityPolicy::default();
        let now = Instant::now();
        let mut queue = PendingQueue::new(2);
        assert!(queue.push(pending(1, spec(SubsystemId::TEST, &policy), now)).is_ok());
        assert!(queue.push(pending(2, spec(SubsystemId::TEST, &policy), now)).is_ok());

        let error = queue.push(pending(3, spec(SubsystemId::TEST, &policy), now)).unwrap_err();
        assert_eq!(error, SubmitError::QueueFull { capacity: 2 });
        assert_eq!(queue.len(), 2, "a refused submission never enters the queue");
    }

    #[test]
    fn higher_priority_runs_first() {
        let policy = PriorityPolicy::default();
        let now = Instant::now();
        let mut queue = PendingQueue::new(8);
        queue.push(pending(1, spec(SubsystemId::INDEXING, &policy), now)).unwrap();
        queue.push(pending(2, spec(SubsystemId::DOCUMENT_IO, &policy), now)).unwrap();
        queue.push(pending(3, spec(SubsystemId::SEARCH, &policy), now)).unwrap();

        let first = queue.take_best(now, &policy, |_| true).unwrap();
        assert_eq!(first.id, TaskId::new(2), "document_io outranks search and indexing");
        let second = queue.take_best(now, &policy, |_| true).unwrap();
        assert_eq!(second.id, TaskId::new(3));
    }

    #[test]
    fn aging_eventually_beats_a_higher_base_priority() {
        // The starvation guarantee: a low-priority task that has waited long
        // enough outranks a high-priority task that has just arrived.
        let policy = PriorityPolicy::default();
        let long_ago = Instant::now();
        let now = long_ago + Duration::from_secs(20); // +1000 aging

        let mut queue = PendingQueue::new(8);
        queue.push(pending(1, spec(SubsystemId::INDEXING, &policy), long_ago)).unwrap();
        queue.push(pending(2, spec(SubsystemId::DOCUMENT_IO, &policy), now)).unwrap();

        let first = queue.take_best(now, &policy, |_| true).unwrap();
        assert_eq!(first.id, TaskId::new(1), "the starved task should win once it has aged");
    }

    #[test]
    fn a_deadline_changes_the_order() {
        let policy = PriorityPolicy::default();
        let now = Instant::now();
        let mut queue = PendingQueue::new(8);

        queue.push(pending(1, spec(SubsystemId::SEARCH, &policy), now)).unwrap();
        let mut urgent = spec(SubsystemId::GIT, &policy); // base 300, below search's 500
        urgent.deadline = Some(now);
        queue.push(pending(2, urgent, now)).unwrap();

        // 300 + 400 (past due) beats 500.
        let first = queue.take_best(now, &policy, |_| true).unwrap();
        assert_eq!(first.id, TaskId::new(2), "deadline pressure should lift the git task");
    }

    #[test]
    fn the_budget_check_can_veto_the_highest_priority_task() {
        let policy = PriorityPolicy::default();
        let now = Instant::now();
        let mut queue = PendingQueue::new(8);

        let mut io_task = spec(SubsystemId::DOCUMENT_IO, &policy);
        io_task.resource_class = ResourceClass::Io;
        queue.push(pending(1, io_task, now)).unwrap();

        let mut cpu_task = spec(SubsystemId::INDEXING, &policy);
        cpu_task.resource_class = ResourceClass::Cpu;
        queue.push(pending(2, cpu_task, now)).unwrap();

        // I/O is saturated: the lower-priority CPU task runs instead.
        let picked = queue.take_best(now, &policy, |class| class != ResourceClass::Io).unwrap();
        assert_eq!(picked.id, TaskId::new(2));
        assert_eq!(queue.len(), 1, "the vetoed task stays queued");
    }

    #[test]
    fn paused_tasks_are_skipped_until_resumed() {
        let policy = PriorityPolicy::default();
        let now = Instant::now();
        let mut queue = PendingQueue::new(8);
        queue.push(pending(1, spec(SubsystemId::DOCUMENT_IO, &policy), now)).unwrap();
        queue.push(pending(2, spec(SubsystemId::SEARCH, &policy), now)).unwrap();

        assert!(queue.set_paused(TaskId::new(1), true));
        let picked = queue.take_best(now, &policy, |_| true).unwrap();
        assert_eq!(picked.id, TaskId::new(2), "the paused task is skipped");

        assert!(queue.set_paused(TaskId::new(1), false));
        let picked = queue.take_best(now, &policy, |_| true).unwrap();
        assert_eq!(picked.id, TaskId::new(1), "and runs once resumed");
    }

    #[test]
    fn removing_and_draining_leave_the_queue_consistent() {
        let policy = PriorityPolicy::default();
        let now = Instant::now();
        let mut queue = PendingQueue::new(8);
        queue.push(pending(1, spec(SubsystemId::TEST, &policy), now)).unwrap();
        queue.push(pending(2, spec(SubsystemId::TEST, &policy), now)).unwrap();

        assert!(queue.remove(TaskId::new(1)).is_some());
        assert!(queue.remove(TaskId::new(1)).is_none());
        assert_eq!(queue.len(), 1);

        assert_eq!(queue.drain().len(), 1);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn an_empty_queue_yields_nothing() {
        let policy = PriorityPolicy::default();
        let mut queue = PendingQueue::new(4);
        assert!(queue.take_best(Instant::now(), &policy, |_| true).is_none());
    }

    #[test]
    fn cost_estimates_are_carried_but_not_interpreted() {
        let estimate = CostEstimate::bytes(1024);
        assert_eq!(estimate.bytes, 1024);
        assert_eq!(estimate.items, 1);
    }
}
