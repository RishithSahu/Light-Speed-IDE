//! Adversarial workload A2: interaction during an asynchronous save.
//!
//! The open benchmark answers "does reading a large file freeze the editor".
//! This one answers the harder question, because a save is not a read:
//!
//! ```text
//! request a save          the snapshot is taken here, inline
//!       +
//! keep editing            the buffer moves on; the snapshot does not
//!       ->
//! measure every keystroke and where the memory went
//! ```
//!
//! A save also has a cost an open does not: it holds a second reference to the
//! document's rope for as long as the write lasts. That is cheap while the user
//! is idle and unbounded-looking while the user is typing, because every edit
//! that touches a shared node copies it. So this file samples RSS at four
//! points -- before, after the snapshot, mid-write, after completion -- rather
//! than reporting one number and calling it memory usage.

use crate::harness::{format_bytes, format_duration, time, Measurement, Samples};
use ls_core::{EditorCore, EffectiveConfig, Movement, Viewport};
use ls_perf::Budget;
use ls_platform::{MemoryClipboard, ProcessSampler};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Interactive contract for input reaching editor state (amendment section 48).
fn input_budget() -> Budget {
    Budget::from_millis(2, 5)
}

fn editor() -> EditorCore {
    EditorCore::with_clipboard(EffectiveConfig::default(), Box::new(MemoryClipboard::new()))
}

/// Opens the workload file and returns an editor with it active.
fn opened(path: &Path) -> (EditorCore, ls_core::DocumentId) {
    let mut editor = editor();
    let id = editor.open_document(path).expect("open the workload file");
    editor.set_page_lines(40);
    (editor, id)
}

/// Pumps until the document has no save in flight and none queued behind it.
fn drain_saves(editor: &mut EditorCore, id: ls_core::DocumentId) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while editor.is_saving(id) || editor.has_queued_save(id) {
        editor.pump_completions();
        if Instant::now() > deadline {
            panic!("a save did not complete within five minutes");
        }
    }
}

/// The last save the editor recorded, for reporting what actually happened.
fn last_save(editor: &EditorCore) -> Option<ls_core::SaveRecord> {
    editor.save_activity().recent().next().cloned()
}

/// What a save request costs the interactive thread.
///
/// This is the only part of a save that is not on a worker: capture the
/// snapshot (an `Arc` clone of the rope, plus the path, encoding and the two
/// version stamps), then submit. If this grows with document size, the
/// snapshot is not O(1) and the design is wrong.
pub fn request_cost(path: &Path, workload: &str, sampler: &mut ProcessSampler) -> Measurement {
    let (mut editor, id) = opened(path);
    let target = save_target(path);

    let mut samples = Samples::new();
    for index in 0..32 {
        // Dirty the document so each request is a real save rather than a
        // no-op, and so the content revision differs every time.
        editor.type_text("x");
        let (outcome, elapsed) = time(|| editor.request_save_as(id, target.clone()));
        outcome.expect("admitted");
        if index >= 4 {
            samples.push(elapsed);
        }
        drain_saves(&mut editor, id);
    }

    let record = last_save(&editor);
    Measurement {
        scenario: "document.save_request".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: Some(input_budget()),
        note: Some(format!(
            "interactive part of a save: snapshot the rope, capture both versions, submit ({} written)",
            format_bytes(record.map(|record| record.bytes_written).unwrap_or(0))
        )),
    }
}

/// End-to-end save latency: request to applied completion.
///
/// Reported without a threshold. A save's duration is dominated by the disk and
/// by `fsync`, neither of which the editor controls; what the editor is
/// accountable for is that this time is not spent on the interactive thread,
/// which the A2 measurements below are what actually prove.
pub fn save_duration(path: &Path, workload: &str, sampler: &mut ProcessSampler) -> Measurement {
    let (mut editor, id) = opened(path);
    let target = save_target(path);
    let iterations = if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) >= 10 * 1024 * 1024 {
        3
    } else {
        16
    };

    let mut samples = Samples::new();
    let mut queue_wait = Duration::ZERO;
    let mut bytes = 0u64;
    for _ in 0..iterations {
        editor.type_text("x");
        let started = Instant::now();
        editor.request_save_as(id, target.clone()).expect("admitted");
        drain_saves(&mut editor, id);
        samples.push(started.elapsed());
        if let Some(record) = last_save(&editor) {
            queue_wait = record.queue_wait.unwrap_or_default();
            bytes = record.bytes_written;
        }
    }

    Measurement {
        scenario: "document.save_end_to_end".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: None,
        note: Some(format!(
            "request to applied completion; {} written, last queue wait {}",
            format_bytes(bytes),
            format_duration(queue_wait)
        )),
    }
}

/// A2: typing while a save is in flight, with memory sampled at four points.
pub fn typing_during_save(
    path: &Path,
    workload: &str,
    sampler: &mut ProcessSampler,
) -> Vec<Measurement> {
    let (mut editor, id) = opened(path);
    let target = save_target(path);
    editor.type_text("edited ");

    let before_rss = sampler.sample().rss_bytes;
    let started = Instant::now();
    editor.request_save_as(id, target.clone()).expect("admitted");
    let after_snapshot_rss = sampler.sample().rss_bytes;

    let mut keystrokes = Samples::new();
    let mut pumps = Samples::new();
    let mut during_rss = after_snapshot_rss;
    let mut count = 0u64;

    while editor.is_saving(id) {
        let (_, typed) = time(|| editor.type_text("x"));
        keystrokes.push(typed);
        let (_, pumped) = time(|| editor.pump_completions());
        pumps.push(pumped);
        count += 1;
        if count % 256 == 0 {
            during_rss = during_rss.max(sampler.sample().rss_bytes);
        }
        if started.elapsed() > Duration::from_secs(300) {
            break;
        }
    }

    let save_time = started.elapsed();
    drain_saves(&mut editor, id);
    let after_rss = sampler.sample().rss_bytes;
    let record = last_save(&editor);
    let written = record.as_ref().map(|record| record.bytes_written).unwrap_or(0);
    let stale = record.as_ref().map(|record| record.stale).unwrap_or(false);

    // The document must still be dirty: every keystroke above happened after
    // the snapshot was taken, so the save wrote an older version. A save that
    // declared this document clean would have silently lost that typing.
    let dirty = editor.document(id).map(|document| document.is_dirty()).unwrap_or(false);
    assert!(
        count == 0 || dirty,
        "editing during a save must leave the document dirty; the completion was stale={stale}"
    );

    let memory_note = format!(
        "RSS before {} -> after snapshot {} -> during write {} -> after completion {}",
        format_bytes(before_rss),
        format_bytes(after_snapshot_rss),
        format_bytes(during_rss),
        format_bytes(after_rss)
    );

    vec![
        Measurement {
            scenario: "A2.keystroke_during_save".to_string(),
            workload: workload.to_string(),
            stats: keystrokes.stats(),
            rss_bytes: during_rss,
            budget: Some(input_budget()),
            note: Some(format!(
                "{count} keystrokes while {} was written in {}; {memory_note}",
                format_bytes(written),
                format_duration(save_time)
            )),
        },
        Measurement {
            scenario: "A2.pump_during_save".to_string(),
            workload: workload.to_string(),
            stats: pumps.stats(),
            rss_bytes: during_rss,
            budget: Some(input_budget()),
            note: Some("cost of checking for the save completion each frame".to_string()),
        },
    ]
}

/// A2: moving the cursor while a save is in flight.
///
/// Separate from typing because it is the cheaper case and therefore the more
/// revealing one: if cursor movement is slow here, the cost is contention, not
/// copy-on-write.
pub fn cursor_during_save(
    path: &Path,
    workload: &str,
    sampler: &mut ProcessSampler,
) -> Measurement {
    let (mut editor, id) = opened(path);
    let target = save_target(path);
    editor.type_text("edited ");

    let started = Instant::now();
    editor.request_save_as(id, target).expect("admitted");

    let mut samples = Samples::new();
    let mut count = 0u64;
    let movements = [
        Movement::LineDown,
        Movement::CharRight,
        Movement::WordRight,
        Movement::LineUp,
        Movement::LineEnd,
    ];
    while editor.is_saving(id) {
        let movement = movements[(count % movements.len() as u64) as usize];
        let (result, elapsed) = time(|| editor.move_cursor(movement, false));
        result.expect("cursor movement");
        samples.push(elapsed);
        editor.pump_completions();
        count += 1;
        if started.elapsed() > Duration::from_secs(300) {
            break;
        }
    }
    drain_saves(&mut editor, id);

    Measurement {
        scenario: "A2.cursor_during_save".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: Some(input_budget()),
        note: Some(format!("{count} cursor movements while the save was in flight")),
    }
}

/// A2: publishing render snapshots while a save is in flight.
///
/// The renderer and the save worker both hold references into the same rope.
/// This measures whether building a frame is affected by that.
pub fn rendering_during_save(
    path: &Path,
    workload: &str,
    sampler: &mut ProcessSampler,
) -> Measurement {
    let (mut editor, id) = opened(path);
    let target = save_target(path);
    editor.type_text("edited ");

    let viewport = Viewport::default();
    let started = Instant::now();
    editor.request_save_as(id, target).expect("admitted");

    let mut samples = Samples::new();
    let mut count = 0u64;
    while editor.is_saving(id) {
        let (snapshot, elapsed) = time(|| editor.render_snapshot(id, viewport));
        std::hint::black_box(snapshot);
        samples.push(elapsed);
        editor.pump_completions();
        count += 1;
        if started.elapsed() > Duration::from_secs(300) {
            break;
        }
    }
    drain_saves(&mut editor, id);

    Measurement {
        scenario: "A2.render_snapshot_during_save".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        // The frame contract, not the input contract: this is per-frame work.
        budget: Some(Budget::from_millis(8, 16)),
        note: Some(format!("{count} snapshots published while the save was in flight")),
    }
}

/// A2: a save requested while another document's save is already queued.
///
/// This is the fairness case. Two documents compete for the same
/// `DOCUMENT IO` admission, and the second request must not block the
/// interactive thread waiting for the first.
pub fn save_behind_a_queued_task(
    path: &Path,
    workload: &str,
    sampler: &mut ProcessSampler,
) -> Measurement {
    let (mut editor, first) = opened(path);
    let second = editor.open_document(path).ok();
    let second = match second {
        Some(id) if id != first => id,
        // The path identity rules mean a second open of the same path joins the
        // first, so make a genuinely separate document instead.
        _ => {
            let id = editor.new_document();
            editor.set_active(id).expect("selectable");
            editor.type_text("second document\n");
            id
        }
    };

    let first_target = save_target(path);
    let second_target = save_target(path).with_extension("second.txt");

    let mut samples = Samples::new();
    let mut queued = 0u32;
    for _ in 0..24 {
        editor.set_active(first).expect("selectable");
        editor.type_text("x");
        editor.request_save_as(first, first_target.clone()).expect("admitted");

        editor.set_active(second).expect("selectable");
        editor.type_text("y");
        let (outcome, elapsed) = time(|| editor.request_save_as(second, second_target.clone()));
        let outcome = outcome.expect("admitted");
        if !matches!(outcome.disposition, ls_core::SaveDisposition::Started) {
            queued += 1;
        }
        samples.push(elapsed);

        drain_saves(&mut editor, first);
        drain_saves(&mut editor, second);
    }

    Measurement {
        scenario: "A2.save_request_behind_queued_work".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: Some(input_budget()),
        note: Some(format!(
            "24 second-document save requests with another save already admitted, {queued} not started immediately"
        )),
    }
}

/// A2: per-document serialization, measured rather than asserted.
///
/// The contract is one in-flight save and at most one queued behind it, with a
/// newer request superseding the queued one. Requesting far faster than the
/// disk can keep up is the only way to see that happen.
pub fn supersession(path: &Path, workload: &str, sampler: &mut ProcessSampler) -> Measurement {
    let (mut editor, id) = opened(path);
    let target = save_target(path);

    let mut samples = Samples::new();
    let mut started = 0u32;
    let mut queued = 0u32;
    let mut superseded = 0u32;
    for _ in 0..64 {
        editor.type_text("x");
        let (outcome, elapsed) = time(|| editor.request_save_as(id, target.clone()));
        match outcome.expect("admitted").disposition {
            ls_core::SaveDisposition::Started => started += 1,
            ls_core::SaveDisposition::Queued => queued += 1,
            ls_core::SaveDisposition::SupersededQueued => superseded += 1,
        }
        samples.push(elapsed);
        editor.pump_completions();
    }
    drain_saves(&mut editor, id);

    Measurement {
        scenario: "A2.save_supersession".to_string(),
        workload: workload.to_string(),
        stats: samples.stats(),
        rss_bytes: sampler.sample().rss_bytes,
        budget: Some(input_budget()),
        note: Some(format!(
            "64 rapid requests: {started} started, {queued} queued, {superseded} replaced a queued one"
        )),
    }
}

/// Where a benchmark save writes, next to the workload file.
fn save_target(path: &Path) -> PathBuf {
    let mut target = path.to_path_buf();
    let stem = target.file_stem().map(|stem| stem.to_string_lossy().to_string());
    target.set_file_name(format!("{}-a2-save.txt", stem.unwrap_or_else(|| "workload".into())));
    target
}

/// Everything above, for one workload.
pub fn run(
    path: &Path,
    workload: &str,
    sampler: &mut ProcessSampler,
    large: bool,
) -> Vec<Measurement> {
    let mut measurements = vec![
        request_cost(path, workload, sampler),
        save_duration(path, workload, sampler),
        supersession(path, workload, sampler),
        save_behind_a_queued_task(path, workload, sampler),
    ];
    // The interaction-during-save cases need a write long enough to interact
    // with. On a 1 KB file the save is over before the first keystroke.
    if large {
        measurements.extend(typing_during_save(path, workload, sampler));
        measurements.push(cursor_during_save(path, workload, sampler));
        measurements.push(rendering_during_save(path, workload, sampler));
    }
    measurements
}
