//! Resource accounting (amendment section 6).
//!
//! Every scheduled task records the same nine fields, and they go into the
//! existing `ls-perf` registry rather than a second metrics system, so a task's
//! cost shows up in the same report as an interactive operation's.

use crate::task::{CostEstimate, SubsystemId, TaskId, TaskState, WorkspaceRef};
use std::time::Duration;

/// Metric and counter names, so producers and readers cannot drift apart.
pub mod names {
    /// Creation to admission.
    pub const QUEUE_WAIT: &str = "scheduler.queue_wait";
    /// Start of execution to terminal state.
    pub const WALL_TIME: &str = "scheduler.wall_time";
    /// CPU consumed by the worker while running the task, when measurable.
    pub const CPU_TIME: &str = "scheduler.cpu_time";

    pub const SUBMITTED: &str = "scheduler.submitted";
    pub const REJECTED: &str = "scheduler.rejected";
    pub const COMPLETED: &str = "scheduler.completed";
    pub const FAILED: &str = "scheduler.failed";
    pub const CANCELLED: &str = "scheduler.cancelled";
    pub const BYTES_READ: &str = "scheduler.bytes_read";
    pub const BYTES_WRITTEN: &str = "scheduler.bytes_written";
    pub const COMPLETIONS_DROPPED: &str = "scheduler.completions_dropped";

    pub const QUEUE_DEPTH: &str = "scheduler.queue_depth";
    pub const RUNNING: &str = "scheduler.running";
}

/// What one task cost (amendment section 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRecord {
    pub task_id: TaskId,
    pub subsystem: SubsystemId,
    pub workspace: Option<WorkspaceRef>,
    pub queue_wait: Duration,
    pub wall_time: Duration,
    /// `None` where the platform cannot report per-thread CPU time.
    pub cpu_time: Option<Duration>,
    pub bytes_read: u64,
    pub bytes_written: u64,
    /// `None`: per-task peak memory is not measurable on the supported
    /// platforms today. The field exists because the contract requires it and
    /// because a future allocator hook can fill it in.
    pub peak_memory: Option<u64>,
    /// What the submitter predicted, kept so estimates can be compared against
    /// reality and improved.
    pub estimated_cost: CostEstimate,
    pub outcome: TaskState,
}

impl TaskRecord {
    /// Ratio of measured bytes to estimated bytes, when both are known. Above
    /// 1.0 means the task cost more than the submitter predicted.
    pub fn cost_ratio(&self) -> Option<f64> {
        if self.estimated_cost.bytes == 0 {
            return None;
        }
        let measured = (self.bytes_read + self.bytes_written) as f64;
        Some(measured / self.estimated_cost.bytes as f64)
    }
}

/// Publishes a record into the shared performance registry.
pub fn publish(record: &TaskRecord) {
    ls_perf::record(names::QUEUE_WAIT, record.queue_wait);
    ls_perf::record(names::WALL_TIME, record.wall_time);
    if let Some(cpu) = record.cpu_time {
        ls_perf::record(names::CPU_TIME, cpu);
    }
    if record.bytes_read > 0 {
        ls_perf::counter(names::BYTES_READ).add(record.bytes_read as i64);
    }
    if record.bytes_written > 0 {
        ls_perf::counter(names::BYTES_WRITTEN).add(record.bytes_written as i64);
    }
    match record.outcome {
        TaskState::Completed => ls_perf::counter(names::COMPLETED).inc(),
        TaskState::Failed => ls_perf::counter(names::FAILED).inc(),
        TaskState::Cancelled => ls_perf::counter(names::CANCELLED).inc(),
        // Only terminal states are recorded; anything else is a bug upstream.
        other => ls_log::warn!(
            "scheduler",
            "non_terminal_record",
            "task record published in state {other}"
        ),
    }
}

/// A bounded window of recent records, for the overlay and for benchmarks.
///
/// Bounded because an editor that runs for a day must not accumulate a day of
/// task records (amendment section 4: no unbounded queues).
pub(crate) struct RecordLog {
    records: std::collections::VecDeque<TaskRecord>,
    capacity: usize,
}

impl RecordLog {
    pub fn new(capacity: usize) -> Self {
        RecordLog { records: std::collections::VecDeque::new(), capacity: capacity.max(1) }
    }

    pub fn push(&mut self, record: TaskRecord) {
        if self.records.len() == self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    pub fn recent(&self) -> Vec<TaskRecord> {
        self.records.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64) -> TaskRecord {
        TaskRecord {
            task_id: TaskId::new(id),
            subsystem: SubsystemId::TEST,
            workspace: None,
            queue_wait: Duration::from_millis(1),
            wall_time: Duration::from_millis(2),
            cpu_time: Some(Duration::from_millis(1)),
            bytes_read: 100,
            bytes_written: 0,
            peak_memory: None,
            estimated_cost: CostEstimate::bytes(50),
            outcome: TaskState::Completed,
        }
    }

    #[test]
    fn the_record_carries_every_required_field() {
        // Amendment section 6 lists nine fields; this asserts each is reachable.
        let record = record(1);
        assert_eq!(record.task_id, TaskId::new(1));
        assert_eq!(record.subsystem, SubsystemId::TEST);
        assert_eq!(record.workspace, None);
        assert_eq!(record.queue_wait, Duration::from_millis(1));
        assert_eq!(record.wall_time, Duration::from_millis(2));
        assert_eq!(record.cpu_time, Some(Duration::from_millis(1)));
        assert_eq!(record.bytes_read, 100);
        assert_eq!(record.bytes_written, 0);
        assert_eq!(record.peak_memory, None);
    }

    #[test]
    fn cost_ratio_compares_measurement_against_estimate() {
        let record = record(1);
        assert_eq!(record.cost_ratio(), Some(2.0), "100 bytes read against a 50 byte estimate");

        let mut unestimated = record;
        unestimated.estimated_cost = CostEstimate::NEGLIGIBLE;
        assert_eq!(unestimated.cost_ratio(), None);
    }

    #[test]
    fn the_record_log_is_bounded_and_keeps_the_newest() {
        let mut log = RecordLog::new(3);
        for id in 0..10 {
            log.push(record(id));
        }
        let recent = log.recent();
        assert_eq!(recent.len(), 3, "the log never grows past its capacity");
        assert_eq!(recent.first().unwrap().task_id, TaskId::new(7));
        assert_eq!(recent.last().unwrap().task_id, TaskId::new(9));
    }

    #[test]
    fn publishing_reaches_the_shared_registry() {
        let before = ls_perf::metric(names::WALL_TIME).stats().count;
        publish(&record(1));
        let after = ls_perf::metric(names::WALL_TIME).stats().count;
        assert_eq!(after, before + 1, "records go into ls-perf, not a second system");
    }
}
