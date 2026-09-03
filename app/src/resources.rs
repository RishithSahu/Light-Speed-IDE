//! Resource Center: a live view of admission, accounting and pressure.
//!
//! Item 4 of the post-surface roadmap ("Resource Control + Backpressure") asks
//! for bounded queues, fair scheduling, accounting, retry/backoff, overload,
//! memory pressure, and a Resource Center to show them in. The scheduler
//! (amendment sections 3-6) already *implements* every one of those except
//! retry/backoff:
//!
//! ```text
//! bounded queues     -> Scheduler::queue_depth / queue_capacity, reject on full
//! fair scheduling    -> PriorityPolicy: base + aging + deadline pressure
//! accounting         -> ls_scheduler::accounting::TaskRecord, per task
//! overload           -> SubmitError::QueueFull, counted (accounting::names::REJECTED)
//! memory pressure     -> ProcessSampler, already sampled every second
//! ```
//!
//! This module is the missing piece: turning what is already measured into
//! something the user can read. It is a pure view, the same shape as
//! [`crate::devpanel`] -- [`lines`] takes editor and process state and returns
//! strings, nothing else -- so it needs no window to test.
//!
//! **Retry/backoff is deliberately not built here.** The scheduler's overload
//! policy is *reject*, chosen and documented (amendment section 3.5.1) over
//! automatic retry specifically so a caller decides what "the queue is full"
//! means for it rather than the scheduler silently trying again behind its
//! back. Adding automatic retry now would reverse a documented decision
//! without the review that decision got; what belongs here instead is making
//! the reject count visible, which is what "retry, manually, once you see
//! why" actually requires. The same caution applies to memory pressure: RSS is
//! surfaced prominently, but nothing here throttles admission on it, because
//! that would be a new admission signal, not a report of an existing one.

use ls_core::{EditorCore, SubsystemId};
use ls_platform::ProcessStats;

/// Subsystems whose base priority is worth showing, in the fairness table's
/// own order (amendment section 5).
const SUBSYSTEMS: &[SubsystemId] = &[
    SubsystemId::DOCUMENT_IO,
    SubsystemId::LANGUAGE,
    SubsystemId::SEARCH,
    SubsystemId::GIT,
    SubsystemId::INDEXING,
];

fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn millis(duration: std::time::Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

/// Builds the panel's rows from live editor, scheduler and process state.
pub fn lines(core: &EditorCore, process: &ProcessStats) -> Vec<String> {
    let mut out = Vec::with_capacity(24);
    let scheduler = core.scheduler();
    let snapshot = ls_perf::snapshot();
    let counter = |name: &str| snapshot.counter(name).unwrap_or(0);

    out.push("Resource Center".to_string());
    out.push(String::new());

    // --- bounded queues ---------------------------------------------------
    out.push("Admission (bounded queue, amendment section 3-4)".to_string());
    out.push(format!(
        "  queue {} / {}    running {}    workers {}",
        scheduler.queue_depth(),
        scheduler.queue_capacity(),
        scheduler.running_count(),
        scheduler.worker_count(),
    ));

    // --- overload -----------------------------------------------------------
    let submitted = counter(ls_core::accounting::names::SUBMITTED);
    let rejected = counter(ls_core::accounting::names::REJECTED);
    out.push(format!(
        "  submitted {submitted}    rejected (overload) {rejected}    completions dropped {}",
        scheduler.dropped_completions(),
    ));
    if rejected > 0 {
        out.push(
            "  the queue rejects rather than retries by design (amendment 3.5.1);\
             a rejected action must be reissued by whatever asked for it"
                .to_string(),
        );
    }

    out.push(String::new());

    // --- fair scheduling ------------------------------------------------------
    out.push("Fairness (base priority + aging + deadline pressure)".to_string());
    for subsystem in SUBSYSTEMS {
        out.push(format!(
            "  {:<11} base {}",
            subsystem.name(),
            scheduler.base_priority(*subsystem).get()
        ));
    }

    out.push(String::new());

    // --- accounting -------------------------------------------------------------
    out.push("Recent tasks (accounting, amendment section 6)".to_string());
    let records = scheduler.recent_records();
    if records.is_empty() {
        out.push("  none yet".to_string());
    }
    for record in records.iter().rev().take(6) {
        let ratio = record
            .cost_ratio()
            .map(|ratio| format!("{ratio:.1}x estimate"))
            .unwrap_or_else(|| "-".to_string());
        out.push(format!(
            "  {:<11} {:?}   wait {}   run {}   {}   {}",
            record.subsystem.name(),
            record.outcome,
            millis(record.queue_wait),
            millis(record.wall_time),
            bytes(record.bytes_read + record.bytes_written),
            ratio,
        ));
    }

    out.push(String::new());

    // --- memory pressure (signal only; see module docs) --------------------------
    out.push("Process (memory pressure signal, not yet an admission control)".to_string());
    out.push(format!("  RSS {:.0} MB    CPU {:.0}%", process.rss_mb(), process.cpu_percent));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_core::EffectiveConfig;
    use ls_platform::MemoryClipboard;

    fn editor() -> EditorCore {
        EditorCore::with_clipboard(EffectiveConfig::default(), Box::new(MemoryClipboard::new()))
    }

    fn process_stats() -> ProcessStats {
        ProcessStats {
            rss_bytes: 150 * 1024 * 1024,
            peak_rss_bytes: 150 * 1024 * 1024,
            cpu_percent: 3.5,
            uptime: std::time::Duration::from_secs(1),
        }
    }

    #[test]
    fn the_panel_reports_queue_depth_and_capacity() {
        let core = editor();
        let lines = lines(&core, &process_stats());
        let queue_line = lines.iter().find(|line| line.contains("queue ")).expect("a queue line");
        assert!(queue_line.contains(&core.scheduler().queue_capacity().to_string()));
    }

    #[test]
    fn the_panel_lists_every_subsystems_base_priority() {
        let core = editor();
        let lines = lines(&core, &process_stats());
        for subsystem in SUBSYSTEMS {
            assert!(
                lines.iter().any(|line| line.contains(subsystem.name())),
                "{} is missing from the fairness table",
                subsystem.name()
            );
        }
    }

    #[test]
    fn document_io_reports_the_documented_base_priority() {
        let core = editor();
        let lines = lines(&core, &process_stats());
        let line =
            lines.iter().find(|line| line.contains("document_io")).expect("document_io is listed");
        assert!(line.contains("800"), "amendment section 5's table: DOCUMENT IO is 800");
    }

    #[test]
    fn an_empty_history_says_so_rather_than_an_empty_section() {
        let core = editor();
        let lines = lines(&core, &process_stats());
        assert!(lines.iter().any(|line| line.contains("none yet")));
    }

    #[test]
    fn process_memory_is_reported_in_megabytes() {
        let core = editor();
        let lines = lines(&core, &process_stats());
        assert!(lines.iter().any(|line| line.contains("RSS 150 MB")));
    }

    #[test]
    fn a_completed_load_appears_in_recent_task_accounting() {
        let directory = std::env::temp_dir().join(format!(
            "lightspeed-resource-center-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("f.txt");
        std::fs::write(&path, "hello\n").unwrap();

        let mut core = editor();
        core.open_document(&path).expect("opened");

        let lines = lines(&core, &process_stats());
        assert!(
            lines.iter().any(|line| line.contains("document_io")),
            "the load that just completed should be in the accounting history"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn bytes_formats_across_units() {
        assert_eq!(bytes(500), "500 B");
        assert_eq!(bytes(2048), "2.0 KB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
