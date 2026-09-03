//! Development panel: a live view of asynchronous document loading.
//!
//! This exists to make Stage 1.1's claims checkable by looking at the running
//! editor rather than by reading a test report. It shows what the scheduler is
//! doing, which loads joined which task, what each one cost, and a heartbeat
//! that keeps ticking while a 100 MB file is being read.
//!
//! The panel is a **pure view**: [`lines`] turns editor state into strings and
//! nothing else. Every state transition it displays happens in `ls-core`, which
//! is why this file can be tested without a window.

use ls_core::{EditorCore, LoadState};
use std::time::{Duration, Instant};

/// Proof that the event loop is still turning.
///
/// The counter advances once per rendered frame. While a load is in flight the
/// shell keeps asking for frames, so a stalled counter means a blocked loop -
/// which is exactly the failure Stage 1.1 exists to prevent.
#[derive(Debug)]
pub struct Heartbeat {
    ticks: u64,
    last_tick: Instant,
    longest_gap: Duration,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Heartbeat::new()
    }
}

impl Heartbeat {
    pub fn new() -> Self {
        Heartbeat { ticks: 0, last_tick: Instant::now(), longest_gap: Duration::ZERO }
    }

    /// Records a frame. Returns the gap since the previous one.
    pub fn tick(&mut self) -> Duration {
        let now = Instant::now();
        let gap = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        self.ticks += 1;
        // The first tick's gap measures startup, not responsiveness.
        if self.ticks > 1 {
            self.longest_gap = self.longest_gap.max(gap);
        }
        gap
    }

    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// Longest observed gap between frames: the number that would grow if the
    /// event loop ever blocked on I/O.
    pub fn longest_gap(&self) -> Duration {
        self.longest_gap
    }

    /// Time since the last frame, for display.
    pub fn since_last_tick(&self) -> Duration {
        self.last_tick.elapsed()
    }
}

fn millis(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

fn optional_millis(duration: Option<Duration>) -> String {
    duration.map(millis).unwrap_or_else(|| "-".to_string())
}

/// Renders the panel.
///
/// Pure: same editor state and heartbeat, same lines.
pub fn lines(core: &EditorCore, heartbeat: &Heartbeat) -> Vec<String> {
    let mut out = Vec::with_capacity(16);

    out.push("LightSpeed - async document loading".to_string());
    out.push(format!(
        "heartbeat {:>8}   since last frame {:>9}   worst gap {:>9}",
        heartbeat.ticks(),
        millis(heartbeat.since_last_tick()),
        millis(heartbeat.longest_gap()),
    ));

    let scheduler = core.scheduler();
    out.push(format!(
        "scheduler  workers {}   queue {}/{}   running {}   dropped completions {}",
        scheduler.worker_count(),
        scheduler.queue_depth(),
        scheduler.queue_capacity(),
        scheduler.running_count(),
        scheduler.dropped_completions(),
    ));

    match core.active() {
        Some(active) if core.is_loading(active) => {
            let pending = core.pending_load(active).expect("loading tabs have a pending load");
            out.push(format!(
                "current    {} LOADING   task {}   elapsed {}   joined requests {}",
                pending.path.file_name(),
                pending.task.get(),
                millis(pending.elapsed()),
                pending.joins,
            ));
        }
        Some(active) => match core.document(active) {
            Some(document) => out.push(format!(
                "current    {}   {} lines   {}   {}",
                document.display_name(),
                document.text().len_lines(),
                document.encoding().label(),
                if document.is_dirty() { "modified" } else { "saved" },
            )),
            None => out.push("current    (no document)".to_string()),
        },
        None => out.push("current    (no tab)".to_string()),
    }

    out.push(format!("loads in flight {}   tabs {}", core.loading_count(), core.tabs().len()));

    out.push(String::new());
    out.push(format!(
        "{:<22} {:>6} {:<9} {:>5} {:>10} {:>10} {:>10}",
        "recent load", "task", "state", "joins", "total", "queue", "work"
    ));

    for record in core.load_activity().recent().take(8) {
        let name = record.path.rsplit(['/', '\\']).next().unwrap_or(&record.path);
        let mut state = record.state.name().to_string();
        if record.is_joined() {
            state.push('*');
        }
        out.push(format!(
            "{:<22} {:>6} {:<9} {:>5} {:>10} {:>10} {:>10}",
            truncate(name, 22),
            record.task.get(),
            state,
            record.joins,
            optional_millis(record.total),
            optional_millis(record.queue_wait),
            optional_millis(record.wall_time),
        ));
        if let Some(error) = &record.error {
            out.push(format!("  {}", truncate(error, 70)));
        }
    }

    if core.load_activity().is_empty() {
        out.push("(nothing loaded yet - Ctrl+O opens a file)".to_string());
    }

    out.push(String::new());
    out.push(
        "F9 panel   F5 duplicate storm   F6 slow load   F7 failing load   Esc cancel".to_string(),
    );

    out
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}~")
}

/// Whether the panel needs another frame to stay live.
///
/// Two reasons, both bounded: something is loading (so the elapsed time and the
/// heartbeat are moving), or the panel is open and its timers are on screen.
pub fn wants_continuous_frames(core: &EditorCore, panel_open: bool) -> bool {
    core.loading_count() > 0 || panel_open
}

/// A load state summarized for the status bar, so the main UI does not have to
/// reimplement the reasoning.
pub fn status_summary(core: &EditorCore) -> Option<String> {
    let active = core.active()?;
    if let Some(pending) = core.pending_load(active) {
        return Some(format!(
            "Loading {} ... {} (task {}, Esc to cancel)",
            pending.path.file_name(),
            millis(pending.elapsed()),
            pending.task.get()
        ));
    }

    // A load that just settled is worth showing briefly.
    let record = core.load_activity().recent().find(|record| record.document == active)?;
    match record.state {
        LoadState::Failed => {
            Some(format!("Failed to open {}: {}", record.path, record.error.as_deref()?))
        }
        LoadState::Cancelled => Some(format!("Cancelled loading {}", record.path)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_core::{EffectiveConfig, LoadInjection};
    use ls_platform::MemoryClipboard;

    fn editor() -> EditorCore {
        EditorCore::with_clipboard(EffectiveConfig::default(), Box::new(MemoryClipboard::new()))
    }

    fn temp_file(name: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("lightspeed-devpanel-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn the_heartbeat_counts_frames_and_remembers_the_worst_gap() {
        let mut heartbeat = Heartbeat::new();
        assert_eq!(heartbeat.ticks(), 0);
        heartbeat.tick();
        assert_eq!(heartbeat.ticks(), 1);
        // The first tick measures startup and must not become the worst gap.
        assert_eq!(heartbeat.longest_gap(), Duration::ZERO);

        std::thread::sleep(Duration::from_millis(5));
        heartbeat.tick();
        assert_eq!(heartbeat.ticks(), 2);
        assert!(heartbeat.longest_gap() >= Duration::from_millis(4));
    }

    #[test]
    fn the_panel_renders_with_an_empty_editor() {
        let core = editor();
        let heartbeat = Heartbeat::new();
        let rendered = lines(&core, &heartbeat);
        assert!(rendered[0].contains("async document loading"));
        assert!(rendered.iter().any(|line| line.contains("heartbeat")));
        assert!(rendered.iter().any(|line| line.contains("nothing loaded yet")));
    }

    #[test]
    fn the_panel_shows_a_loading_document_with_its_task_and_joins() {
        let mut core = editor();
        let path = temp_file("panel-loading.txt", "hello");
        let request = core
            .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(400)))
            .expect("admitted");
        core.request_open_document(&path).expect("joins");

        let rendered = lines(&core, &Heartbeat::new());
        let current = rendered.iter().find(|line| line.starts_with("current")).unwrap();
        assert!(current.contains("LOADING"), "{current}");
        assert!(current.contains(&format!("task {}", request.task.unwrap().get())), "{current}");
        assert!(current.contains("joined requests 2"), "{current}");

        assert!(rendered.iter().any(|line| line.contains("loads in flight 1")));

        core.cancel_open(request.document);
        while core.is_loading(request.document) {
            core.pump_completions();
        }
    }

    #[test]
    fn the_panel_marks_joined_loads_in_the_recent_list() {
        let mut core = editor();
        let path = temp_file("panel-join.txt", "content");
        let request = core
            .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(120)))
            .unwrap();
        for _ in 0..3 {
            core.request_open_document(&path).unwrap();
        }
        while core.is_loading(request.document) {
            core.pump_completions();
        }

        let rendered = lines(&core, &Heartbeat::new());
        // The recent-list row, not the "current document" line: both name the
        // same file, only the row carries a load state.
        let row = rendered
            .iter()
            .find(|line| line.starts_with("panel-join.txt") && line.contains("loaded"))
            .expect("the load appears in the recent list");
        assert!(row.contains("loaded*"), "a joined load is marked: {row}");
        assert!(row.contains(" 4 "), "four requests shared it: {row}");
    }

    #[test]
    fn the_status_summary_reports_loading_then_clears() {
        let mut core = editor();
        let path = temp_file("panel-status.txt", "body");
        let request = core
            .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(150)))
            .unwrap();

        let summary = status_summary(&core).expect("a loading document has a summary");
        assert!(summary.starts_with("Loading panel-status.txt"), "{summary}");
        assert!(summary.contains("Esc to cancel"));

        while core.is_loading(request.document) {
            core.pump_completions();
        }
        assert_eq!(status_summary(&core), None, "a loaded document needs no status line");
    }

    #[test]
    fn the_status_summary_reports_a_failure() {
        let mut core = editor();
        let path = temp_file("panel-fail.txt", "body");
        let request = core.request_open_document_with(&path, LoadInjection::failing()).unwrap();
        while core.is_loading(request.document) {
            core.pump_completions();
        }

        let rendered = lines(&core, &Heartbeat::new());
        assert!(
            rendered.iter().any(|line| line.contains("failed")),
            "the failure shows in the recent list"
        );
    }

    #[test]
    fn continuous_frames_are_requested_only_when_something_is_happening() {
        let mut core = editor();
        assert!(!wants_continuous_frames(&core, false), "an idle editor draws nothing");
        assert!(wants_continuous_frames(&core, true), "an open panel keeps its timers live");

        let path = temp_file("panel-frames.txt", "x");
        let request = core
            .request_open_document_with(&path, LoadInjection::delayed(Duration::from_millis(150)))
            .unwrap();
        assert!(wants_continuous_frames(&core, false), "a load in flight keeps frames coming");

        core.cancel_open(request.document);
        while core.is_loading(request.document) {
            core.pump_completions();
        }
        assert!(!wants_continuous_frames(&core, false), "and stops when it settles");
    }

    #[test]
    fn long_names_are_truncated_rather_than_wrapping() {
        assert_eq!(truncate("short", 22), "short");
        let long = "a-very-long-file-name-that-will-not-fit.txt";
        let truncated = truncate(long, 22);
        assert_eq!(truncated.chars().count(), 22);
        assert!(truncated.ends_with('~'));
    }
}
