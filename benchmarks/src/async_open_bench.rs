//! Adversarial workload A1: interaction during a large asynchronous load.
//!
//! Stage 1 could measure how long a 100 MB open took. It could not measure the
//! thing that actually matters, because the answer was "the editor is frozen
//! for that whole time". Stage 1.1 can:
//!
//! ```text
//! request a 100 MB open
//!       +
//! keep typing
//!       ->
//! measure every keystroke
//! ```
//!
//! The claim is not "the load is fast". The claim is that the load does not
//! touch interaction, and this is the measurement that proves it.

use crate::harness::{format_bytes, format_duration, time, Measurement, Samples};
use ls_core::{EditorCore, EffectiveConfig};
use ls_perf::Budget;
use ls_platform::{MemoryClipboard, ProcessSampler};
use std::path::Path;
use std::time::{Duration, Instant};

/// Interactive contract for input reaching editor state (amendment section 48).
fn input_budget() -> Budget {
    Budget::from_millis(2, 5)
}

fn editor() -> EditorCore {
    EditorCore::with_clipboard(EffectiveConfig::default(), Box::new(MemoryClipboard::new()))
}

/// Measures what a request costs the interactive thread.
///
/// This is the part of an open that still happens inline: canonicalize, look up
/// identity, allocate, submit. It is the only part that can affect latency.
pub fn request_cost(path: &Path, workload: &str, sampler: &mut ProcessSampler) -> Measurement {
    let mut samples = Samples::new();
    for index in 0..64 {
        let mut editor = editor();
        let (request, elapsed) = time(|| editor.request_open_document(path));
        let request = request.expect("admitted");
        if index >= 4 {
            samples.push(elapsed);
        }
        // Cancel rather than wait: this measures the request, not the load.
        editor.cancel_open(request.document);
        let deadline = Instant::now() + Duration::from_secs(10);
        while editor.loading_count() > 0 && Instant::now() < deadline {
            editor.pump_completions();
        }
    }

    Measurement {
        scenario: "document.open_request".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: Some(input_budget()),
        note: Some(
            "interactive part of an open: canonicalize, identity, submit - the read is elsewhere"
                .to_string(),
        ),
    }
}

/// A1: typing while a large file loads.
pub fn typing_during_load(
    path: &Path,
    workload: &str,
    sampler: &mut ProcessSampler,
) -> Vec<Measurement> {
    let mut editor = editor();
    let scratch = editor.new_document();
    let request = editor.request_open_document(path).expect("admitted");
    editor.set_active(scratch).expect("scratch document is selectable");

    let mut keystrokes = Samples::new();
    let mut pumps = Samples::new();
    let started = Instant::now();
    let mut count = 0u64;

    while editor.is_loading(request.document) {
        let (_, typed) = time(|| editor.type_text("x"));
        keystrokes.push(typed);
        let (_, pumped) = time(|| editor.pump_completions());
        pumps.push(pumped);
        count += 1;
        if started.elapsed() > Duration::from_secs(120) {
            break;
        }
    }

    let load_time = started.elapsed();
    let loaded = editor.document(request.document);
    let lines = loaded.map(|document| document.text().len_lines()).unwrap_or(0);
    let bytes = editor.load_activity().recent().next().map(|record| record.bytes).unwrap_or(0);
    let rss = sampler.sample().rss_bytes;

    // Every keystroke must have landed in the scratch document: responsiveness
    // that drops input is not responsiveness.
    let typed_chars = editor.document(scratch).map(|d| d.text().len_chars()).unwrap_or(0);
    assert_eq!(typed_chars as u64, count, "a keystroke was lost during the load");

    vec![
        Measurement {
            scenario: "A1.keystroke_during_open".to_string(),
            workload: workload.to_string(),
            stats: keystrokes.stats(),
            rss_bytes: rss,
            budget: Some(input_budget()),
            note: Some(format!(
                "{count} keystrokes while {} loaded in {} ({lines} lines)",
                format_bytes(bytes),
                format_duration(load_time)
            )),
        },
        Measurement {
            scenario: "A1.pump_during_open".to_string(),
            workload: workload.to_string(),
            stats: pumps.stats(),
            rss_bytes: rss,
            budget: Some(input_budget()),
            note: Some("cost of checking for finished background work each frame".to_string()),
        },
    ]
}

/// Duplicate requests for one path: the join path, measured.
pub fn duplicate_join(path: &Path, workload: &str, sampler: &mut ProcessSampler) -> Measurement {
    let mut editor = editor();
    let first = editor.request_open_document(path).expect("admitted");

    let mut samples = Samples::new();
    let mut joined = 0u32;
    for _ in 0..64 {
        let (request, elapsed) = time(|| editor.request_open_document(path));
        let request = request.expect("joins or reports already open");
        if request.joined {
            joined += 1;
        }
        samples.push(elapsed);
    }

    let deadline = Instant::now() + Duration::from_secs(120);
    while editor.loading_count() > 0 && Instant::now() < deadline {
        editor.pump_completions();
    }
    let loads = editor.load_activity().len();

    Measurement {
        scenario: "document.open_join".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: Some(input_budget()),
        note: Some(format!(
            "64 duplicate requests, {joined} joined, {loads} underlying load(s); task {}",
            first.task.map(|task| task.get()).unwrap_or(0)
        )),
    }
}
