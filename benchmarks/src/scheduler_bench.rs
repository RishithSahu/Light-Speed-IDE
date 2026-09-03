//! Scheduler measurements (amendment sections 3, 5, 6).
//!
//! The scheduler's own cost matters in two places, and they are measured
//! separately because they have different consequences:
//!
//! * **on the interactive thread** — submitting a task and draining completions
//!   happen while the user is waiting, so they are part of the input-to-state
//!   budget and must stay in the microsecond range;
//! * **on a worker** — turnaround and queue wait describe how quickly work
//!   actually starts, which is a throughput property, not a latency contract.
//!
//! No new performance contract is declared here. The amendment declares the
//! interactive budgets; these numbers describe what the scheduler adds to them.

use crate::harness::{format_duration, measure, time, Measurement};
use ls_platform::ProcessSampler;
use ls_scheduler::{
    CostEstimate, Priority, ResourceClass, Scheduler, SchedulerConfig, SubsystemId, TaskOutcome,
    TaskProduct, TaskSpec,
};
use std::time::{Duration, Instant};

const WORKLOAD: &str = "S1_scheduler";

fn spec(scheduler: &Scheduler, subsystem: SubsystemId) -> TaskSpec {
    TaskSpec::new(subsystem, scheduler.base_priority(subsystem), ResourceClass::Cpu)
}

/// Spins until `condition`, or gives up so a broken scheduler fails the run
/// rather than hanging it.
fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::hint::spin_loop();
    }
    false
}

pub fn run(sampler: &mut ProcessSampler) -> Vec<Measurement> {
    let mut measurements = Vec::new();
    measurements.push(submit_cost(sampler));
    measurements.push(drain_cost(sampler));
    measurements.push(turnaround(sampler));
    measurements.extend(saturation(sampler));
    measurements
}

/// What submitting costs the interactive thread.
fn submit_cost(sampler: &mut ProcessSampler) -> Measurement {
    let scheduler = Scheduler::with_defaults();
    let samples = measure(64, 2000, |_| {
        let spec = spec(&scheduler, SubsystemId::DOCUMENT_IO);
        let (result, elapsed) = time(|| scheduler.submit(spec, Box::new(|_| TaskOutcome::done())));
        // A full queue would measure the rejection path instead; drain so it
        // cannot happen.
        if result.is_err() {
            scheduler.drain_completions();
        }
        elapsed
    });
    let _ = wait_until(|| scheduler.queue_depth() == 0);
    scheduler.drain_completions();

    Measurement {
        scenario: "scheduler.submit".to_string(),
        workload: WORKLOAD.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: None,
        note: Some(
            "interactive-thread cost of admission; part of the input-to-state budget".to_string(),
        ),
    }
}

/// What draining completions costs the interactive thread.
fn drain_cost(sampler: &mut ProcessSampler) -> Measurement {
    let scheduler = Scheduler::with_defaults();
    let samples = measure(16, 500, |_| {
        for _ in 0..8 {
            let spec = spec(&scheduler, SubsystemId::DOCUMENT_IO);
            let _ = scheduler.submit(spec, Box::new(|_| TaskOutcome::done()));
        }
        let _ = wait_until(|| scheduler.pending_completions() >= 8);
        let (completions, elapsed) = time(|| scheduler.drain_completions());
        debug_assert!(!completions.is_empty());
        elapsed
    });

    Measurement {
        scenario: "scheduler.drain_completions".to_string(),
        workload: WORKLOAD.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: None,
        note: Some("interactive-thread cost of collecting eight finished tasks".to_string()),
    }
}

/// Submit to completion for an empty task: the scheduler's own overhead.
fn turnaround(sampler: &mut ProcessSampler) -> Measurement {
    let scheduler = Scheduler::with_defaults();
    let samples = measure(32, 500, |_| {
        let spec = spec(&scheduler, SubsystemId::DOCUMENT_IO);
        let started = Instant::now();
        let submitted = scheduler.submit(spec, Box::new(|_| TaskOutcome::done())).is_ok();
        debug_assert!(submitted);
        let arrived = wait_until(|| scheduler.pending_completions() > 0);
        let elapsed = started.elapsed();
        debug_assert!(arrived);
        scheduler.drain_completions();
        elapsed
    });

    Measurement {
        scenario: "scheduler.turnaround_empty".to_string(),
        workload: WORKLOAD.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: None,
        note: Some("submit to completion for a no-op task, including worker wake".to_string()),
    }
}

/// A deep queue: does admission stay orderly, and what does waiting cost?
fn saturation(sampler: &mut ProcessSampler) -> Vec<Measurement> {
    const TASKS: usize = 512;
    let scheduler = Scheduler::new(SchedulerConfig {
        queue_capacity: TASKS,
        completion_capacity: TASKS,
        record_capacity: TASKS,
        ..Default::default()
    });

    let mut rejected = 0usize;
    let submit_started = Instant::now();
    for index in 0..TASKS {
        // A mix of priorities, so ordering has something to do.
        let subsystem = match index % 4 {
            0 => SubsystemId::DOCUMENT_IO,
            1 => SubsystemId::SEARCH,
            2 => SubsystemId::GIT,
            _ => SubsystemId::INDEXING,
        };
        let spec = spec(&scheduler, subsystem).with_cost(CostEstimate::bytes(64));
        let submitted = scheduler.submit(
            spec,
            Box::new(|_| {
                // Enough work that the queue actually backs up.
                let mut sink = 0u64;
                for value in 0..20_000u64 {
                    sink = sink.wrapping_add(value);
                }
                TaskOutcome::Completed(TaskProduct::new(sink))
            }),
        );
        if submitted.is_err() {
            rejected += 1;
        }
    }
    let submit_elapsed = submit_started.elapsed();

    let completed = wait_until(|| scheduler.queue_depth() == 0 && scheduler.running_count() == 0);
    let drain_elapsed = submit_started.elapsed();
    let records = scheduler.recent_records();

    let mut queue_waits = crate::harness::Samples::new();
    let mut wall_times = crate::harness::Samples::new();
    for record in &records {
        queue_waits.push(record.queue_wait);
        wall_times.push(record.wall_time);
    }

    let rss = sampler.sample().rss_bytes;
    let note = format!(
        "{TASKS} tasks, {} workers; submitted in {}, all finished in {}; {rejected} rejected{}",
        scheduler.worker_count(),
        format_duration(submit_elapsed),
        format_duration(drain_elapsed),
        if completed { "" } else { " (TIMED OUT)" }
    );

    vec![
        Measurement {
            scenario: "scheduler.queue_wait_saturated".to_string(),
            workload: WORKLOAD.to_string(),
            stats: queue_waits.stats(),
            rss_bytes: rss,
            budget: None,
            note: Some(note),
        },
        Measurement {
            scenario: "scheduler.task_wall_time".to_string(),
            workload: WORKLOAD.to_string(),
            stats: wall_times.stats(),
            rss_bytes: rss,
            budget: None,
            note: Some(format!("{} records retained for accounting", records.len())),
        },
    ]
}

/// Confirms the accounting a benchmark run depends on is actually populated.
pub fn verify_accounting() -> Result<(), String> {
    let mut config = SchedulerConfig::single_worker();
    config.cpu_time_source = Some(ls_platform::process::thread_cpu_time);
    let scheduler = Scheduler::new(config);

    let spec = TaskSpec::new(SubsystemId::DOCUMENT_IO, Priority::new(800), ResourceClass::Io)
        .with_cost(CostEstimate::bytes(4096));
    scheduler
        .submit(
            spec,
            Box::new(|_| {
                let mut sink = 0u64;
                for value in 0..200_000u64 {
                    sink = sink.wrapping_add(value);
                }
                TaskOutcome::Completed(TaskProduct::new(sink).with_io(4096, 0))
            }),
        )
        .map_err(|error| error.to_string())?;

    if !wait_until(|| scheduler.pending_completions() > 0) {
        return Err("the task never completed".to_string());
    }
    let record = scheduler.drain_completions().remove(0).record;
    if record.bytes_read != 4096 {
        return Err(format!("bytes_read was {}", record.bytes_read));
    }
    if cfg!(windows) && record.cpu_time.is_none() {
        return Err("cpu_time was not measured on a platform that supports it".to_string());
    }
    Ok(())
}
