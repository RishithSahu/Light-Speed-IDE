//! LightSpeed performance instrumentation (specification sections 48-51, 56).
//!
//! Performance is a product requirement, so measurement is a first-class
//! subsystem rather than a debugging afterthought. Every interactive operation
//! records a latency sample; every sample lands in a bounded ring buffer from
//! which P50/P95/P99/max are computed on demand.
//!
//! Contracts are expressed as a [`Budget`] pair - a *target* and a *failure
//! threshold* (specification section 48) - never as a single "maximum
//! acceptable" number:
//!
//! ```
//! use std::time::Duration;
//! ls_perf::set_budget(
//!     ls_perf::names::INPUT_TO_STATE,
//!     Budget::new(Duration::from_millis(2), Duration::from_millis(5)),
//! );
//! # use ls_perf::Budget;
//! ```
//!
//! Recording is deliberately cheap: a handle lookup is amortized away by
//! [`metric`], and a sample is one mutex acquisition plus a slot write. The
//! whole subsystem can be switched off with [`set_enabled`] so an instrumented
//! build can be measured against an uninstrumented one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// Number of latency samples retained per metric.
pub const RING_CAPACITY: usize = 4096;

/// Canonical metric names, so producers and the overlay cannot drift apart.
pub mod names {
    /// Input event to committed editor-state change (spec: target P95 2 ms).
    pub const INPUT_TO_STATE: &str = "input.to_state";
    /// Input event to presented frame (spec: target P95 8 ms).
    pub const INPUT_TO_FRAME: &str = "input.to_frame";
    /// Whole-frame CPU time.
    pub const FRAME: &str = "render.frame";
    /// Building an immutable RenderSnapshot.
    pub const SNAPSHOT_BUILD: &str = "render.snapshot_build";
    /// Text shaping for newly visible or invalidated lines.
    pub const TEXT_SHAPE: &str = "render.text_shape";
    /// Cursor movement command.
    pub const CURSOR_MOVE: &str = "editor.cursor_move";
    /// Selection movement command.
    pub const SELECTION_MOVE: &str = "editor.selection_move";
    /// Single text edit applied to a document.
    pub const EDIT_APPLY: &str = "editor.edit_apply";
    /// Undo or redo of one transaction.
    pub const UNDO_REDO: &str = "editor.undo_redo";
    /// Switching to an already-loaded tab (spec: target P95 2 ms).
    pub const TAB_SWITCH: &str = "editor.tab_switch";
    /// `open_document()` end to end (spec: small file target P95 20 ms).
    pub const DOCUMENT_OPEN: &str = "document.open";
    /// Atomic save end to end.
    pub const DOCUMENT_SAVE: &str = "document.save";
    /// Process start to first usable editor frame (spec: target P95 500 ms).
    pub const STARTUP_USABLE: &str = "startup.usable";
}

/// A performance contract: a target and the threshold that counts as failure.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    pub target_p95: Duration,
    pub failure_p95: Duration,
}

impl Budget {
    pub const fn new(target_p95: Duration, failure_p95: Duration) -> Self {
        Budget { target_p95, failure_p95 }
    }

    pub const fn from_millis(target: u64, failure: u64) -> Self {
        Budget::new(Duration::from_millis(target), Duration::from_millis(failure))
    }
}

/// Status of a metric relative to its [`Budget`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BudgetStatus {
    /// No budget declared for this metric.
    Unmeasured,
    /// P95 is within target.
    MeetsTarget,
    /// P95 is past target but below the failure threshold.
    OverTarget,
    /// P95 is past the failure threshold: a contract violation.
    Failing,
}

impl BudgetStatus {
    pub const fn label(self) -> &'static str {
        match self {
            BudgetStatus::Unmeasured => "-",
            BudgetStatus::MeetsTarget => "ok",
            BudgetStatus::OverTarget => "over",
            BudgetStatus::Failing => "FAIL",
        }
    }
}

/// Latency distribution of one metric.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LatencyStats {
    /// Total samples ever recorded (not just those retained in the ring).
    pub count: u64,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub mean: Duration,
    /// Samples that exceeded the metric's failure threshold.
    pub violations: u64,
}

#[derive(Clone, Debug)]
pub struct MetricSnapshot {
    pub name: &'static str,
    pub stats: LatencyStats,
    pub budget: Option<Budget>,
    pub status: BudgetStatus,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub metrics: Vec<MetricSnapshot>,
    pub counters: Vec<(&'static str, i64)>,
    pub gauges: Vec<(&'static str, f64)>,
}

impl Snapshot {
    pub fn metric(&self, name: &str) -> Option<&MetricSnapshot> {
        self.metrics.iter().find(|m| m.name == name)
    }

    pub fn gauge(&self, name: &str) -> Option<f64> {
        self.gauges.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    pub fn counter(&self, name: &str) -> Option<i64> {
        self.counters.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    /// Metrics whose P95 is past the failure threshold.
    pub fn failing(&self) -> impl Iterator<Item = &MetricSnapshot> {
        self.metrics.iter().filter(|m| m.status == BudgetStatus::Failing)
    }
}

struct MetricInner {
    ring: Box<[u64; RING_CAPACITY]>,
    next: usize,
    retained: usize,
    count: u64,
    sum_nanos: u128,
    max_nanos: u64,
    violations: u64,
    budget: Option<Budget>,
}

impl MetricInner {
    fn new() -> Self {
        MetricInner {
            ring: Box::new([0; RING_CAPACITY]),
            next: 0,
            retained: 0,
            count: 0,
            sum_nanos: 0,
            max_nanos: 0,
            violations: 0,
            budget: None,
        }
    }

    fn record(&mut self, nanos: u64) {
        self.ring[self.next] = nanos;
        self.next = (self.next + 1) % RING_CAPACITY;
        if self.retained < RING_CAPACITY {
            self.retained += 1;
        }
        self.count += 1;
        self.sum_nanos += nanos as u128;
        self.max_nanos = self.max_nanos.max(nanos);
        if let Some(budget) = self.budget {
            if nanos > budget.failure_p95.as_nanos() as u64 {
                self.violations += 1;
            }
        }
    }

    fn stats(&self) -> LatencyStats {
        if self.retained == 0 {
            return LatencyStats::default();
        }
        let mut samples: Vec<u64> = self.ring[..self.retained].to_vec();
        samples.sort_unstable();
        let pick = |q: f64| -> Duration {
            // Nearest-rank percentile over retained samples.
            let rank = (q * samples.len() as f64).ceil() as usize;
            let idx = rank.saturating_sub(1).min(samples.len() - 1);
            Duration::from_nanos(samples[idx])
        };
        LatencyStats {
            count: self.count,
            p50: pick(0.50),
            p95: pick(0.95),
            p99: pick(0.99),
            max: Duration::from_nanos(self.max_nanos),
            mean: Duration::from_nanos((self.sum_nanos / self.count as u128) as u64),
            violations: self.violations,
        }
    }
}

struct MetricState {
    inner: Mutex<MetricInner>,
}

/// Cheap reusable reference to one metric. Hold it across a hot path instead of
/// looking the metric up by name on every sample.
#[derive(Clone)]
pub struct MetricHandle {
    name: &'static str,
    state: Arc<MetricState>,
}

impl MetricHandle {
    pub fn name(&self) -> &'static str {
        self.name
    }

    #[inline]
    pub fn record(&self, elapsed: Duration) {
        if !enabled() {
            return;
        }
        if let Ok(mut inner) = self.state.inner.lock() {
            inner.record(elapsed.as_nanos().min(u64::MAX as u128) as u64);
        }
    }

    /// Starts a scoped timer that records into this metric when dropped.
    #[inline]
    pub fn timer(&self) -> Timer {
        Timer { handle: Some(self.clone()), start: Instant::now() }
    }

    pub fn stats(&self) -> LatencyStats {
        self.state.inner.lock().map(|inner| inner.stats()).unwrap_or_default()
    }
}

/// RAII latency timer; records elapsed time into its metric on drop.
pub struct Timer {
    handle: Option<MetricHandle>,
    start: Instant,
}

impl Timer {
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Records now and returns the elapsed time, cancelling the drop record.
    pub fn stop(mut self) -> Duration {
        let elapsed = self.start.elapsed();
        if let Some(handle) = self.handle.take() {
            handle.record(elapsed);
        }
        elapsed
    }

    /// Abandons the measurement (for paths that turned out not to be measurable).
    pub fn cancel(mut self) {
        self.handle = None;
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.record(self.start.elapsed());
        }
    }
}

#[derive(Clone)]
pub struct CounterHandle {
    value: Arc<AtomicI64>,
}

impl CounterHandle {
    #[inline]
    pub fn add(&self, delta: i64) {
        self.value.fetch_add(delta, Ordering::Relaxed);
    }
    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }
    #[inline]
    pub fn set(&self, value: i64) {
        self.value.store(value, Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct Registry {
    metrics: RwLock<HashMap<&'static str, Arc<MetricState>>>,
    counters: RwLock<HashMap<&'static str, Arc<AtomicI64>>>,
    gauges: RwLock<HashMap<&'static str, u64>>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(true);

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::default)
}

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turns instrumentation on or off process-wide.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Returns a reusable handle for `name`, creating the metric if needed.
pub fn metric(name: &'static str) -> MetricHandle {
    let registry = registry();
    if let Some(state) = registry.metrics.read().ok().and_then(|m| m.get(name).cloned()) {
        return MetricHandle { name, state };
    }
    let mut metrics = registry.metrics.write().expect("perf registry poisoned");
    let state = metrics
        .entry(name)
        .or_insert_with(|| Arc::new(MetricState { inner: Mutex::new(MetricInner::new()) }))
        .clone();
    MetricHandle { name, state }
}

/// Records one latency sample by name. Prefer [`metric`] on hot paths.
pub fn record(name: &'static str, elapsed: Duration) {
    metric(name).record(elapsed);
}

/// Starts a scoped timer by name. Prefer [`MetricHandle::timer`] on hot paths.
pub fn start(name: &'static str) -> Timer {
    metric(name).timer()
}

/// Declares the performance contract for a metric.
pub fn set_budget(name: &'static str, budget: Budget) {
    let handle = metric(name);
    let Ok(mut inner) = handle.state.inner.lock() else { return };
    inner.budget = Some(budget);
}

pub fn counter(name: &'static str) -> CounterHandle {
    let registry = registry();
    if let Some(value) = registry.counters.read().ok().and_then(|c| c.get(name).cloned()) {
        return CounterHandle { value };
    }
    let mut counters = registry.counters.write().expect("perf registry poisoned");
    let value = counters.entry(name).or_insert_with(|| Arc::new(AtomicI64::new(0))).clone();
    CounterHandle { value }
}

/// Sets a sampled value such as RSS or CPU percentage.
pub fn gauge(name: &'static str, value: f64) {
    if let Ok(mut gauges) = registry().gauges.write() {
        gauges.insert(name, value.to_bits());
    }
}

fn status_for(stats: &LatencyStats, budget: Option<Budget>) -> BudgetStatus {
    match budget {
        None => BudgetStatus::Unmeasured,
        Some(_) if stats.count == 0 => BudgetStatus::Unmeasured,
        Some(budget) => {
            if stats.p95 > budget.failure_p95 {
                BudgetStatus::Failing
            } else if stats.p95 > budget.target_p95 {
                BudgetStatus::OverTarget
            } else {
                BudgetStatus::MeetsTarget
            }
        }
    }
}

/// Takes a consistent-enough view of all instrumentation for reporting.
pub fn snapshot() -> Snapshot {
    let registry = registry();
    let mut metrics = Vec::new();
    if let Ok(map) = registry.metrics.read() {
        for (name, state) in map.iter() {
            let (stats, budget) = match state.inner.lock() {
                Ok(inner) => (inner.stats(), inner.budget),
                Err(_) => (LatencyStats::default(), None),
            };
            metrics.push(MetricSnapshot {
                name,
                stats,
                budget,
                status: status_for(&stats, budget),
            });
        }
    }
    metrics.sort_by_key(|m| m.name);

    let mut counters: Vec<(&'static str, i64)> = registry
        .counters
        .read()
        .map(|map| map.iter().map(|(n, v)| (*n, v.load(Ordering::Relaxed))).collect())
        .unwrap_or_default();
    counters.sort_by_key(|(n, _)| *n);

    let mut gauges: Vec<(&'static str, f64)> = registry
        .gauges
        .read()
        .map(|map| map.iter().map(|(n, v)| (*n, f64::from_bits(*v))).collect())
        .unwrap_or_default();
    gauges.sort_by_key(|(n, _)| *n);

    Snapshot { metrics, counters, gauges }
}

/// Clears all samples, counters and gauges. Budgets are retained.
pub fn reset() {
    let registry = registry();
    if let Ok(map) = registry.metrics.read() {
        for state in map.values() {
            if let Ok(mut inner) = state.inner.lock() {
                let budget = inner.budget;
                *inner = MetricInner::new();
                inner.budget = budget;
            }
        }
    }
    if let Ok(map) = registry.counters.read() {
        for value in map.values() {
            value.store(0, Ordering::Relaxed);
        }
    }
    if let Ok(mut map) = registry.gauges.write() {
        map.clear();
    }
}

/// Formats a duration for overlays and reports.
pub fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos >= 1_000_000_000 {
        format!("{:.2} s", d.as_secs_f64())
    } else if nanos >= 1_000_000 {
        format!("{:.2} ms", nanos as f64 / 1_000_000.0)
    } else if nanos >= 1_000 {
        format!("{:.1} us", nanos as f64 / 1_000.0)
    } else {
        format!("{nanos} ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that touch the process-global enable flag or the shared registry
    /// must not run concurrently with each other.
    static GLOBAL_STATE: Mutex<()> = Mutex::new(());

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut inner = MetricInner::new();
        for i in 1..=100u64 {
            inner.record(i * 1_000_000);
        }
        let stats = inner.stats();
        assert_eq!(stats.count, 100);
        assert_eq!(stats.p50, Duration::from_millis(50));
        assert_eq!(stats.p95, Duration::from_millis(95));
        assert_eq!(stats.p99, Duration::from_millis(99));
        assert_eq!(stats.max, Duration::from_millis(100));
        assert_eq!(stats.mean, Duration::from_millis(50) + Duration::from_micros(500));
    }

    #[test]
    fn ring_retains_only_the_most_recent_samples_but_counts_all() {
        let mut inner = MetricInner::new();
        for i in 0..(RING_CAPACITY as u64 * 2) {
            inner.record(i);
        }
        let stats = inner.stats();
        assert_eq!(stats.count, RING_CAPACITY as u64 * 2);
        // Max is tracked outside the ring, so old peaks are never lost.
        assert_eq!(stats.max, Duration::from_nanos(RING_CAPACITY as u64 * 2 - 1));
        // The retained window is the newest half.
        assert!(stats.p50 >= Duration::from_nanos(RING_CAPACITY as u64));
    }

    #[test]
    fn empty_metric_has_zero_stats() {
        let stats = MetricInner::new().stats();
        assert_eq!(stats, LatencyStats::default());
    }

    #[test]
    fn budget_status_distinguishes_target_from_failure() {
        let mut inner = MetricInner::new();
        inner.budget = Some(Budget::from_millis(2, 5));
        for _ in 0..100 {
            inner.record(1_000_000);
        }
        let stats = inner.stats();
        assert_eq!(status_for(&stats, inner.budget), BudgetStatus::MeetsTarget);

        let mut inner = MetricInner::new();
        inner.budget = Some(Budget::from_millis(2, 5));
        for _ in 0..100 {
            inner.record(3_000_000);
        }
        assert_eq!(status_for(&inner.stats(), inner.budget), BudgetStatus::OverTarget);

        let mut inner = MetricInner::new();
        inner.budget = Some(Budget::from_millis(2, 5));
        for _ in 0..100 {
            inner.record(9_000_000);
        }
        let stats = inner.stats();
        assert_eq!(status_for(&stats, inner.budget), BudgetStatus::Failing);
        assert_eq!(stats.violations, 100);
    }

    #[test]
    fn no_budget_means_unmeasured() {
        let mut inner = MetricInner::new();
        inner.record(1);
        assert_eq!(status_for(&inner.stats(), None), BudgetStatus::Unmeasured);
    }

    #[test]
    fn handles_counters_and_gauges_round_trip() {
        let _guard = GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let handle = metric("test.roundtrip");
        handle.record(Duration::from_micros(500));
        assert_eq!(handle.stats().count, 1);

        counter("test.counter").add(3);
        counter("test.counter").inc();
        assert_eq!(counter("test.counter").get(), 4);

        gauge("test.gauge", 12.5);
        let snapshot = snapshot();
        assert_eq!(snapshot.gauge("test.gauge"), Some(12.5));
        assert_eq!(snapshot.counter("test.counter"), Some(4));
        assert!(snapshot.metric("test.roundtrip").is_some());
    }

    #[test]
    fn timer_records_on_drop_and_stop_is_idempotent() {
        let _guard = GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let handle = metric("test.timer");
        {
            let _timer = handle.timer();
        }
        assert_eq!(handle.stats().count, 1);
        // `stop` records once and returns what it recorded. The elapsed value
        // itself is not asserted: an empty scope can legitimately measure zero
        // on a clock with 100 ns resolution.
        let _elapsed = handle.timer().stop();
        assert_eq!(handle.stats().count, 2);
        handle.timer().cancel();
        assert_eq!(handle.stats().count, 2);
    }

    #[test]
    fn disabled_instrumentation_records_nothing() {
        let _guard = GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let handle = metric("test.disabled");
        set_enabled(false);
        handle.record(Duration::from_millis(1));
        set_enabled(true);
        assert_eq!(handle.stats().count, 0);
    }

    #[test]
    fn durations_format_by_magnitude() {
        assert_eq!(format_duration(Duration::from_nanos(950)), "950 ns");
        assert_eq!(format_duration(Duration::from_micros(1500)), "1.50 ms");
        assert_eq!(format_duration(Duration::from_millis(2500)), "2.50 s");
    }
}
