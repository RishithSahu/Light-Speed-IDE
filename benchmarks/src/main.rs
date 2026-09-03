//! LightSpeed benchmark harness (specification sections 52, 56, 70).
//!
//! Runs the Stage 1 measurement set - startup, typing, large-file editing,
//! cursor and selection movement, undo/redo, scrolling, tab switching, open,
//! save and Unicode-heavy editing - and reports P50/P95/P99/max with RSS
//! against the declared performance contracts.
//!
//! ```text
//! cargo run --release -p ls-bench -- --quick
//! cargo run --release -p ls-bench -- --json benchmarks/results/latest.json
//! ```

mod async_open_bench;
mod async_save_bench;
mod harness;
mod scheduler_bench;
mod workload;

use harness::{format_bytes, format_duration, measure, time, Environment, Measurement, Samples};
use ls_buffer::{CharOffset, LineIndex, TextBuffer};
use ls_core::{EditorCore, EffectiveConfig, Movement, Position, Selection, Viewport};
use ls_perf::Budget;
use ls_platform::ProcessSampler;
use std::path::PathBuf;
use workload::Workload;

struct Options {
    quick: bool,
    json: Option<PathBuf>,
    filter: Option<String>,
}

fn parse_options() -> Options {
    let mut options = Options { quick: false, json: None, filter: None };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--quick" => options.quick = true,
            "--json" => options.json = args.next().map(PathBuf::from),
            "--filter" => options.filter = args.next(),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument {other}");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    options
}

fn print_usage() {
    println!(
        "usage: ls-bench [--quick] [--json <path>] [--filter <substring>]\n\
         \n\
         --quick   skip the 100 MB workloads\n\
         --json    also write machine-readable results\n\
         --filter  only run scenarios whose name contains this substring"
    );
}

fn main() {
    let options = parse_options();
    let environment = Environment::capture();
    let mut sampler = ProcessSampler::new();

    print_header(&environment);

    let mut measurements = Vec::new();
    measurements.extend(startup_and_empty_memory(&mut sampler));

    // The scheduler is measured before the document workloads so its numbers
    // are taken on an otherwise-idle process.
    if let Err(error) = scheduler_bench::verify_accounting() {
        eprintln!("scheduler accounting check failed: {error}");
        std::process::exit(1);
    }
    measurements.extend(scheduler_bench::run(&mut sampler));

    let mut workloads: Vec<Workload> = workload::DOCUMENT_WORKLOADS.to_vec();
    if options.quick {
        workloads.retain(|w| w.target_bytes <= 10 * 1024 * 1024);
    }

    for workload in workloads {
        measurements.extend(run_document_workload(workload, &mut sampler, &options));
    }
    measurements.extend(run_document_workload(workload::UNICODE_WORKLOAD, &mut sampler, &options));
    if !options.quick {
        measurements.extend(run_document_workload(
            workload::LONG_LINE_WORKLOAD,
            &mut sampler,
            &options,
        ));
    }

    print_table(&measurements);
    print_contract_summary(&measurements);

    if let Some(path) = options.json {
        match write_json(&path, &environment, &measurements) {
            Ok(()) => println!("\nwrote {}", path.display()),
            Err(error) => eprintln!("\ncould not write {}: {error}", path.display()),
        }
    }
}

fn print_header(environment: &Environment) {
    println!("LightSpeed benchmark - editor core v{}", environment.version);
    println!("workload definitions v{}", workload::WORKLOAD_VERSION);
    println!(
        "{} / {} / {} cores / {} RAM / {} build",
        environment.os_version,
        environment.cpu,
        environment.cores,
        format_bytes(environment.total_ram_bytes),
        environment.build_profile
    );
    println!("gpu: {}", environment.gpu);
    if cfg!(debug_assertions) {
        println!("WARNING: debug build - these numbers are not the product's numbers");
    }
    println!();
}

/// W1: an editor with no workspace, no files and no background tasks
/// (specification section 50).
fn startup_and_empty_memory(sampler: &mut ProcessSampler) -> Vec<Measurement> {
    let baseline = sampler.sample().rss_bytes;

    let mut samples = Samples::new();
    for _ in 0..20 {
        let (_editor, elapsed) = time(|| EditorCore::new(EffectiveConfig::default()));
        samples.push(elapsed);
    }

    let editor = EditorCore::new(EffectiveConfig::default());
    let after = sampler.sample().rss_bytes;
    drop(editor);

    vec![Measurement {
        scenario: "startup.core_construct".to_string(),
        workload: "W1_empty".to_string(),
        stats: samples.stats(),
        rss_bytes: after,
        budget: None,
        note: Some(format!(
            "headless core only; process RSS {} (baseline {})",
            format_bytes(after),
            format_bytes(baseline)
        )),
    }]
}

fn iterations_for(bytes: usize, base: usize) -> usize {
    match bytes {
        b if b >= 100 * 1024 * 1024 => (base / 8).max(20),
        b if b >= 10 * 1024 * 1024 => (base / 4).max(50),
        b if b >= 1024 * 1024 => base / 2,
        _ => base,
    }
}

fn run_document_workload(
    workload: Workload,
    sampler: &mut ProcessSampler,
    options: &Options,
) -> Vec<Measurement> {
    let text = workload::generate(workload);
    println!("{}", workload::describe(workload, &text));

    let mut measurements = Vec::new();
    let mut add = |scenario: &str, samples: Samples, rss: u64, budget: Option<Budget>| {
        if options.filter.as_ref().is_some_and(|f| !scenario.contains(f.as_str())) {
            return;
        }
        measurements.push(Measurement {
            scenario: scenario.to_string(),
            workload: workload.name.to_string(),
            stats: samples.stats(),
            rss_bytes: rss,
            budget,
            note: None,
        });
    };

    // --- building the buffer (the in-memory half of opening a file) ----------
    let before_rss = sampler.sample().rss_bytes;
    let build_iterations = if workload.target_bytes >= 10 * 1024 * 1024 { 5 } else { 30 };
    let build = measure(1, build_iterations, |_| {
        let (buffer, elapsed) = time(|| TextBuffer::from_str(&text));
        std::hint::black_box(buffer.len_chars());
        elapsed
    });

    let buffer = TextBuffer::from_str(&text);
    let after_rss = sampler.sample().rss_bytes;
    let overhead = after_rss.saturating_sub(before_rss);
    println!(
        "  rope: depth {}, {} resident for {} of text ({:.2}x)",
        buffer.depth(),
        format_bytes(overhead),
        format_bytes(text.len() as u64),
        overhead as f64 / text.len().max(1) as f64
    );
    add("buffer.build", build, after_rss, None);
    drop(buffer);

    // --- opening and saving a real file --------------------------------------
    let directory = std::env::temp_dir().join("lightspeed-bench");
    std::fs::create_dir_all(&directory).expect("temp directory");
    let path = directory.join(format!("{}.txt", workload.name));
    std::fs::write(&path, text.as_bytes()).expect("write workload file");

    let io_iterations = if workload.target_bytes >= 10 * 1024 * 1024 { 3 } else { 20 };
    let open = measure(1, io_iterations, |_| {
        let mut editor = EditorCore::with_clipboard(
            EffectiveConfig::default(),
            Box::new(ls_platform::MemoryClipboard::new()),
        );
        let (id, elapsed) = time(|| editor.open_document(&path).expect("open"));
        std::hint::black_box(id);
        elapsed
    });
    add(
        "document.open",
        open,
        sampler.sample().rss_bytes,
        // The contract is stated for small files; larger ones are reported
        // without a threshold until the data justifies one.
        (workload.target_bytes <= 64 * 1024).then(|| Budget::from_millis(20, 50)),
    );

    // The asynchronous path: what the request costs interactively, whether
    // duplicates join, and - the point of Stage 1.1 - whether typing survives
    // a large load. Collected separately because `add` borrows `measurements`
    // for the rest of this function.
    let mut async_measurements = vec![
        async_open_bench::request_cost(&path, workload.name, sampler),
        async_open_bench::duplicate_join(&path, workload.name, sampler),
    ];
    if workload.target_bytes >= 10 * 1024 * 1024 {
        async_measurements.extend(async_open_bench::typing_during_load(
            &path,
            workload.name,
            sampler,
        ));
    }

    // The save side of the same question (amendment sections 9 and 10).
    async_measurements.extend(async_save_bench::run(
        &path,
        workload.name,
        sampler,
        workload.target_bytes >= 1024 * 1024,
    ));

    let mut editor = EditorCore::with_clipboard(
        EffectiveConfig::default(),
        Box::new(ls_platform::MemoryClipboard::new()),
    );
    let id = editor.open_document(&path).expect("open");
    editor.set_page_lines(40);

    let save_path = directory.join(format!("{}-save.txt", workload.name));
    let save = measure(1, io_iterations, |_| {
        let (result, elapsed) = time(|| editor.save_as(id, save_path.clone()));
        result.expect("save");
        elapsed
    });
    add("document.save", save, sampler.sample().rss_bytes, None);

    // --- typing ---------------------------------------------------------------
    let total_chars = editor.document(id).expect("document").text().len_chars();
    let type_iterations = iterations_for(workload.target_bytes, 2000);

    for (label, fraction) in [("start", 0.0), ("middle", 0.5), ("end", 1.0)] {
        let anchor = (total_chars as f64 * fraction) as usize;
        editor.set_selection(Selection::caret(CharOffset::new(anchor.min(total_chars))));
        let samples = measure(20, type_iterations, |_| {
            let (_, elapsed) = time(|| editor.type_text("x"));
            elapsed
        });
        add(
            &format!("edit.type_char_{label}"),
            samples,
            sampler.sample().rss_bytes,
            Some(Budget::from_millis(2, 5)),
        );
    }

    let samples = measure(20, type_iterations, |_| {
        let (_, elapsed) = time(|| editor.delete_backward());
        elapsed
    });
    add("edit.backspace", samples, sampler.sample().rss_bytes, Some(Budget::from_millis(2, 5)));

    let block = "pasted block of text\n".repeat(50); // ~1 KB
    let paste_iterations = iterations_for(workload.target_bytes, 200);
    let samples = measure(5, paste_iterations, |_| {
        let (_, elapsed) = time(|| editor.paste_text(&block));
        elapsed
    });
    add("edit.paste_1kb", samples, sampler.sample().rss_bytes, Some(Budget::from_millis(2, 5)));

    // --- cursor and selection --------------------------------------------------
    let move_iterations = iterations_for(workload.target_bytes, 2000);
    let middle = CharOffset::new(total_chars / 2);
    editor.set_selection(Selection::caret(middle));

    for (label, movement) in [
        ("char", Movement::CharRight),
        ("word", Movement::WordRight),
        ("line", Movement::LineDown),
        ("page", Movement::PageDown),
    ] {
        editor.set_selection(Selection::caret(middle));
        let samples = measure(20, move_iterations, |_| {
            let (_, elapsed) = time(|| editor.move_cursor(movement, false));
            elapsed
        });
        add(
            &format!("cursor.{label}"),
            samples,
            sampler.sample().rss_bytes,
            Some(Budget::from_millis(4, 10)),
        );
    }

    editor.set_selection(Selection::caret(middle));
    let samples = measure(20, move_iterations, |_| {
        let (_, elapsed) = time(|| editor.move_cursor(Movement::LineDown, true));
        elapsed
    });
    add(
        "selection.extend_line",
        samples,
        sampler.sample().rss_bytes,
        Some(Budget::from_millis(4, 10)),
    );

    // Jumping to the end of a huge document is the pathological cursor case.
    let samples = measure(5, 50, |index| {
        let movement = if index % 2 == 0 { Movement::DocumentEnd } else { Movement::DocumentStart };
        let (_, elapsed) = time(|| editor.move_cursor(movement, false));
        elapsed
    });
    add(
        "cursor.document_end",
        samples,
        sampler.sample().rss_bytes,
        Some(Budget::from_millis(4, 10)),
    );

    // --- undo and redo ----------------------------------------------------------
    let undo_iterations = iterations_for(workload.target_bytes, 500).min(400);
    for index in 0..undo_iterations {
        editor.insert(id, Position::new(index % 100, 0), "u").expect("insert for undo history");
    }
    let samples = measure(0, undo_iterations, |_| {
        let (_, elapsed) = time(|| editor.undo(id).expect("undo"));
        elapsed
    });
    add("edit.undo", samples, sampler.sample().rss_bytes, Some(Budget::from_millis(2, 5)));

    let samples = measure(0, undo_iterations, |_| {
        let (_, elapsed) = time(|| editor.redo(id).expect("redo"));
        elapsed
    });
    add("edit.redo", samples, sampler.sample().rss_bytes, Some(Budget::from_millis(2, 5)));

    // --- rendering ---------------------------------------------------------------
    let total_lines = editor.document(id).expect("document").text().len_lines();
    let scroll_iterations = iterations_for(workload.target_bytes, 1000);
    let samples = measure(20, scroll_iterations, |index| {
        let first = (index * 37) % total_lines.max(1);
        let viewport =
            Viewport { first_line: LineIndex::new(first), visible_lines: 50, ..Default::default() };
        let (snapshot, elapsed) = time(|| editor.render_snapshot(id, viewport));
        std::hint::black_box(snapshot.map(|s| s.lines.len()));
        elapsed
    });
    add(
        "render.snapshot_50_lines",
        samples,
        sampler.sample().rss_bytes,
        Some(Budget::from_millis(2, 5)),
    );

    // --- tab switching -------------------------------------------------------------
    let second = editor.new_document();
    let samples = measure(20, 500, |index| {
        let target = if index % 2 == 0 { id } else { second };
        let (_, elapsed) = time(|| editor.set_active(target));
        elapsed
    });
    add("tab.switch", samples, sampler.sample().rss_bytes, Some(Budget::from_millis(2, 5)));

    // --- offset conversion -----------------------------------------------------------
    let samples = measure(20, move_iterations, |index| {
        let document = editor.document(id).expect("document");
        let line = LineIndex::new((index * 61) % total_lines.max(1));
        let (offset, elapsed) = time(|| document.text().line_to_char(line));
        std::hint::black_box(offset);
        elapsed
    });
    add("query.line_to_char", samples, sampler.sample().rss_bytes, None);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&save_path);
    println!();
    measurements.extend(async_measurements);
    measurements
}

fn print_table(measurements: &[Measurement]) {
    println!(
        "{:<26} {:<16} {:>7} {:>10} {:>10} {:>10} {:>10} {:>9} {:>6}",
        "scenario", "workload", "n", "p50", "p95", "p99", "max", "rss", "budget"
    );
    println!("{}", "-".repeat(112));
    for measurement in measurements {
        println!(
            "{:<26} {:<16} {:>7} {:>10} {:>10} {:>10} {:>10} {:>9} {:>6}",
            measurement.scenario,
            measurement.workload,
            measurement.stats.count,
            format_duration(measurement.stats.p50),
            format_duration(measurement.stats.p95),
            format_duration(measurement.stats.p99),
            format_duration(measurement.stats.max),
            format_bytes(measurement.rss_bytes),
            measurement.status(),
        );
        if let Some(note) = &measurement.note {
            println!("    {note}");
        }
    }
}

fn print_contract_summary(measurements: &[Measurement]) {
    let failing: Vec<&Measurement> = measurements.iter().filter(|m| m.status() == "FAIL").collect();
    let over: Vec<&Measurement> = measurements.iter().filter(|m| m.status() == "over").collect();

    println!();
    if failing.is_empty() && over.is_empty() {
        println!("all measured scenarios meet their targets");
        return;
    }
    for measurement in over {
        println!(
            "over target: {} on {} - p95 {} (target {})",
            measurement.scenario,
            measurement.workload,
            format_duration(measurement.stats.p95),
            format_duration(measurement.budget.expect("status implies a budget").target_p95),
        );
    }
    for measurement in failing {
        println!(
            "CONTRACT FAILURE: {} on {} - p95 {} (threshold {})",
            measurement.scenario,
            measurement.workload,
            format_duration(measurement.stats.p95),
            format_duration(measurement.budget.expect("status implies a budget").failure_p95),
        );
    }
}

fn write_json(
    path: &PathBuf,
    environment: &Environment,
    measurements: &[Measurement],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!("  \"version\": {},\n", quote(&environment.version)));
    json.push_str(&format!("  \"workload_version\": {},\n", workload::WORKLOAD_VERSION));
    json.push_str(&format!("  \"platform\": {},\n", quote(&environment.platform)));
    json.push_str(&format!("  \"os\": {},\n", quote(&environment.os_version)));
    json.push_str(&format!("  \"cpu\": {},\n", quote(&environment.cpu)));
    json.push_str(&format!("  \"cores\": {},\n", environment.cores));
    json.push_str(&format!("  \"ram_bytes\": {},\n", environment.total_ram_bytes));
    json.push_str(&format!("  \"build\": {},\n", quote(environment.build_profile)));
    json.push_str(&format!("  \"gpu\": {},\n", quote(&environment.gpu)));
    json.push_str(&format!(
        "  \"timestamp_unix\": {},\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));
    json.push_str("  \"measurements\": [\n");
    for (index, measurement) in measurements.iter().enumerate() {
        json.push_str("    {");
        json.push_str(&format!("\"scenario\": {}, ", quote(&measurement.scenario)));
        json.push_str(&format!("\"workload\": {}, ", quote(&measurement.workload)));
        json.push_str(&format!("\"count\": {}, ", measurement.stats.count));
        json.push_str(&format!("\"mean_us\": {}, ", measurement.stats.mean.as_micros()));
        json.push_str(&format!("\"p50_us\": {}, ", measurement.stats.p50.as_micros()));
        json.push_str(&format!("\"p95_us\": {}, ", measurement.stats.p95.as_micros()));
        json.push_str(&format!("\"p99_us\": {}, ", measurement.stats.p99.as_micros()));
        json.push_str(&format!("\"max_us\": {}, ", measurement.stats.max.as_micros()));
        json.push_str(&format!("\"rss_bytes\": {}, ", measurement.rss_bytes));
        json.push_str(&format!("\"status\": {}", quote(measurement.status())));
        json.push('}');
        if index + 1 < measurements.len() {
            json.push(',');
        }
        json.push('\n');
    }
    json.push_str("  ]\n}\n");
    std::fs::write(path, json)
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
