//! Measurement plumbing: sample collection, percentiles and environment capture
//! (specification section 56).

use std::time::{Duration, Instant};

/// A collected set of latency samples for one measured operation.
#[derive(Default)]
pub struct Samples {
    values: Vec<Duration>,
}

impl Samples {
    pub fn new() -> Self {
        Samples::default()
    }

    pub fn push(&mut self, value: Duration) {
        self.values.push(value);
    }

    pub fn stats(&self) -> Stats {
        if self.values.is_empty() {
            return Stats::default();
        }
        let mut sorted = self.values.clone();
        sorted.sort_unstable();
        let pick = |q: f64| {
            let rank = (q * sorted.len() as f64).ceil() as usize;
            sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
        };
        let total: Duration = sorted.iter().sum();
        Stats {
            count: sorted.len(),
            p50: pick(0.50),
            p95: pick(0.95),
            p99: pick(0.99),
            max: *sorted.last().expect("non-empty"),
            mean: total / sorted.len() as u32,
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Stats {
    pub count: usize,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub mean: Duration,
}

/// One measured scenario, ready to report.
pub struct Measurement {
    pub scenario: String,
    pub workload: String,
    pub stats: Stats,
    /// Resident set size after the scenario ran.
    pub rss_bytes: u64,
    /// Performance contract this scenario is measured against, if any.
    pub budget: Option<ls_perf::Budget>,
    pub note: Option<String>,
}

impl Measurement {
    pub fn status(&self) -> &'static str {
        match self.budget {
            None => "-",
            Some(budget) => {
                if self.stats.p95 > budget.failure_p95 {
                    "FAIL"
                } else if self.stats.p95 > budget.target_p95 {
                    "over"
                } else {
                    "ok"
                }
            }
        }
    }
}

/// Runs `operation` `iterations` times after `warmup` untimed runs.
pub fn measure<F>(warmup: usize, iterations: usize, mut operation: F) -> Samples
where
    F: FnMut(usize) -> Duration,
{
    for index in 0..warmup {
        let _ = operation(index);
    }
    let mut samples = Samples::new();
    for index in 0..iterations {
        samples.push(operation(index));
    }
    samples
}

/// Times one closure.
#[inline]
pub fn time<T>(operation: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let value = operation();
    let elapsed = start.elapsed();
    (value, elapsed)
}

/// Machine and build description that every benchmark report carries
/// (specification section 56).
pub struct Environment {
    pub version: String,
    pub platform: String,
    pub os_version: String,
    pub cpu: String,
    pub cores: usize,
    pub total_ram_bytes: u64,
    pub build_profile: &'static str,
    pub gpu: String,
}

impl Environment {
    pub fn capture() -> Self {
        Environment {
            version: ls_core::VERSION.to_string(),
            platform: ls_platform::platform_name().to_string(),
            os_version: os_version(),
            cpu: cpu_brand(),
            cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            total_ram_bytes: total_ram(),
            build_profile: if cfg!(debug_assertions) { "debug" } else { "release" },
            // The GPU is only known once the renderer initializes; the
            // application logs its adapter at startup.
            gpu: "not used by the headless benchmark".to_string(),
        }
    }
}

fn os_version() -> String {
    if cfg!(windows) {
        std::env::var("OS").unwrap_or_else(|_| "Windows".to_string())
    } else {
        std::env::consts::OS.to_string()
    }
}

fn cpu_brand() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        // Leaf 0x80000000 reports the highest extended leaf; 0x80000002..4 hold
        // the brand string. Every x86_64 CPU implements them.
        let highest = __cpuid(0x8000_0000).eax;
        if highest < 0x8000_0004 {
            return "unknown x86_64".to_string();
        }
        let mut bytes = Vec::with_capacity(48);
        for leaf in 0x8000_0002u32..=0x8000_0004 {
            let result = __cpuid(leaf);
            for register in [result.eax, result.ebx, result.ecx, result.edx] {
                bytes.extend_from_slice(&register.to_le_bytes());
            }
        }
        String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_string()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::env::consts::ARCH.to_string()
    }
}

fn total_ram() -> u64 {
    #[cfg(windows)]
    {
        // Reported by the platform crate's process sampler is per-process; total
        // physical memory comes from the OS directly.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetPhysicallyInstalledSystemMemory(total_kilobytes: *mut u64) -> i32;
        }
        let mut kilobytes: u64 = 0;
        // SAFETY: the out-parameter is a valid, writable u64.
        let ok = unsafe { GetPhysicallyInstalledSystemMemory(&mut kilobytes) };
        if ok != 0 {
            return kilobytes * 1024;
        }
        0
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("MemTotal:"))
                    .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
            .unwrap_or(0)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        0
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_duration(duration: Duration) -> String {
    ls_perf::format_duration(duration)
}
