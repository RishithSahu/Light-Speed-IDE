//! Worker threads.
//!
//! **This module is the only place in LightSpeed that may create a worker
//! thread** (amendment section 3.5). The architecture test
//! `no_subsystem_creates_its_own_workers` allow-lists exactly this file; every
//! other module in the workspace fails the build if it spawns anything.
//!
//! A worker does one thing in a loop: claim the highest-priority admissible
//! task, run it, publish what it cost. It never touches editor state — a task
//! body returns a value, and the interactive thread applies it.

use crate::accounting::TaskRecord;
use crate::queue::Pending;
use crate::task::{TaskId, TaskOutcome, TaskProduct, TaskSpec, TaskState};
use crate::{CompletionOutcome, Shared, TaskCompletion};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SUBSYSTEM: &str = "scheduler";

/// A task this worker has been admitted to run.
struct Claim {
    id: TaskId,
    spec: TaskSpec,
    body: crate::task::TaskBody,
    cpu_at_start: Option<Duration>,
}

/// Starts the configured number of workers.
pub(crate) fn spawn_all(shared: &Arc<Shared>) -> Vec<JoinHandle<()>> {
    (0..shared.config.workers)
        .filter_map(|index| {
            let shared = Arc::clone(shared);
            std::thread::Builder::new()
                .name(format!("lightspeed-worker-{index}"))
                .spawn(move || run(shared))
                .map_err(|error| {
                    ls_log::error!(
                        SUBSYSTEM,
                        "worker_spawn_failed",
                        "could not start worker {index}: {error}"
                    );
                })
                .ok()
        })
        .collect()
}

fn run(shared: Arc<Shared>) {
    while let Some(claim) = claim_next(&shared) {
        let token = claim.spec.cancellation.clone();
        // Cancelled between admission and execution: do not start the work.
        let outcome =
            if token.is_cancelled() { TaskOutcome::Cancelled } else { (claim.body)(&token) };
        finish(&shared, claim.id, claim.spec, claim.cpu_at_start, outcome);
    }
}

/// Blocks until a task can run, or until shutdown.
fn claim_next(shared: &Arc<Shared>) -> Option<Claim> {
    let mut state = shared.lock_state();
    loop {
        if state.shutdown {
            return None;
        }

        let running = state.running_by_class;
        let budgets = shared.config.max_concurrent;
        let now = Instant::now();
        let picked = state.pending.take_best(now, &shared.config.policy, |class| {
            running[class.index()] < budgets[class.index()]
        });

        if let Some(pending) = picked {
            return Some(admit(shared, &mut state, pending, now));
        }

        // Nothing runnable: either the queue is empty or every queued task is
        // blocked by its resource budget. Both are resolved by another thread
        // submitting, resuming, or finishing, and all three notify.
        state = shared.wait_for_work(state);
    }
}

/// Moves a picked task `Queued -> Admitted -> Running` and starts its clocks.
fn admit(shared: &Arc<Shared>, state: &mut crate::State, pending: Pending, now: Instant) -> Claim {
    let Pending { id, spec, body, .. } = pending;

    if let Some(lifecycle) = state.lifecycles.get_mut(&id) {
        // Admission is where queue waiting stops; execution starts immediately
        // after, under the same lock, so a task is never observably stuck in
        // `Admitted`.
        if let Err(error) = lifecycle.admit(now) {
            ls_log::error!(SUBSYSTEM, "illegal_transition", "{error}");
        }
        if let Err(error) = lifecycle.start(now) {
            ls_log::error!(SUBSYSTEM, "illegal_transition", "{error}");
        }
    }
    state.running_by_class[spec.resource_class.index()] += 1;
    state.publish_gauges();

    let cpu_at_start = shared.config.cpu_time_source.and_then(|source| source());
    Claim { id, spec, body, cpu_at_start }
}

/// Records what the task cost and publishes its completion.
fn finish(
    shared: &Arc<Shared>,
    id: TaskId,
    spec: TaskSpec,
    cpu_at_start: Option<Duration>,
    outcome: TaskOutcome,
) {
    let now = Instant::now();
    let cpu_time = match (shared.config.cpu_time_source.and_then(|source| source()), cpu_at_start) {
        (Some(end), Some(start)) => Some(end.saturating_sub(start)),
        _ => None,
    };

    // A task whose token was set reports Cancelled whatever its body returned:
    // cancellation is never reported as a failure (amendment section 3.2).
    let cancelled = spec.cancellation.is_cancelled();
    let terminal = if cancelled { TaskState::Cancelled } else { outcome.terminal_state() };

    let (completion_outcome, bytes_read, bytes_written) = match (cancelled, outcome) {
        (true, _) => (CompletionOutcome::Cancelled, 0, 0),
        (false, TaskOutcome::Completed(product)) => {
            let TaskProduct { value, bytes_read, bytes_written } = product;
            (
                CompletionOutcome::Completed(TaskProduct { value, bytes_read, bytes_written }),
                bytes_read,
                bytes_written,
            )
        }
        (false, TaskOutcome::Failed(failure)) => (CompletionOutcome::Failed(failure), 0, 0),
        (false, TaskOutcome::Cancelled) => (CompletionOutcome::Cancelled, 0, 0),
    };

    let mut state = shared.lock_state();
    state.running_by_class[spec.resource_class.index()] -= 1;

    let (queue_wait, wall_time) = match state.lifecycles.get_mut(&id) {
        Some(lifecycle) => {
            let transition = match terminal {
                TaskState::Completed => lifecycle.complete(now),
                TaskState::Failed => lifecycle.fail(now),
                _ => lifecycle.cancel(now),
            };
            if let Err(error) = transition {
                ls_log::error!(SUBSYSTEM, "illegal_transition", "{error}");
            }
            (lifecycle.queue_wait().unwrap_or_default(), lifecycle.wall_time().unwrap_or_default())
        }
        None => (Duration::ZERO, Duration::ZERO),
    };

    let record = TaskRecord {
        task_id: id,
        subsystem: spec.subsystem,
        workspace: spec.workspace,
        queue_wait,
        wall_time,
        cpu_time,
        bytes_read,
        bytes_written,
        peak_memory: None,
        estimated_cost: spec.estimated_cost,
        outcome: terminal,
    };

    state.retire(id, terminal);
    state.publish_completion(TaskCompletion {
        task: id,
        subsystem: spec.subsystem,
        outcome: completion_outcome,
        record: record.clone(),
    });
    state.publish_gauges();
    drop(state);

    crate::accounting::publish(&record);
    // Freeing a resource-class slot can unblock a queued task, and the
    // completion needs the interactive thread's attention.
    shared.notify_all();
    shared.wake_consumer();
}
