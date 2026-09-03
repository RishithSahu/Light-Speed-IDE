//! Process resource sampling (specification sections 50, 51, 56).
//!
//! Memory contracts are stated in RSS, so RSS is measured, not estimated. CPU
//! is sampled as a delta between two observations of process CPU time, which is
//! why callers hold a [`ProcessSampler`] rather than calling a free function.

use std::time::{Duration, Instant};

/// One observation of this process's resource usage.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ProcessStats {
    /// Resident set size (Windows: working set) in bytes.
    pub rss_bytes: u64,
    /// Largest resident set size seen by the OS for this process.
    pub peak_rss_bytes: u64,
    /// Share of one machine's total CPU capacity used since the previous
    /// sample, in percent (100 = every core busy).
    pub cpu_percent: f64,
    /// Time since the sampler was created.
    pub uptime: Duration,
}

impl ProcessStats {
    pub fn rss_mb(&self) -> f64 {
        self.rss_bytes as f64 / (1024.0 * 1024.0)
    }
    pub fn peak_rss_mb(&self) -> f64 {
        self.peak_rss_bytes as f64 / (1024.0 * 1024.0)
    }
}

/// Samples process CPU and memory usage over time.
pub struct ProcessSampler {
    start: Instant,
    last_wall: Instant,
    last_cpu: Duration,
    cores: f64,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler {
    pub fn new() -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
        ProcessSampler {
            start: Instant::now(),
            last_wall: Instant::now(),
            last_cpu: process_cpu_time().unwrap_or_default(),
            cores,
        }
    }

    /// Takes a new observation. CPU percentage covers the interval since the
    /// previous call.
    pub fn sample(&mut self) -> ProcessStats {
        let now = Instant::now();
        let cpu = process_cpu_time().unwrap_or_default();
        let wall_delta = now.duration_since(self.last_wall);
        let cpu_delta = cpu.saturating_sub(self.last_cpu);
        let cpu_percent = if wall_delta.as_secs_f64() > 0.0 {
            (cpu_delta.as_secs_f64() / (wall_delta.as_secs_f64() * self.cores)) * 100.0
        } else {
            0.0
        };
        self.last_wall = now;
        self.last_cpu = cpu;

        let (rss_bytes, peak_rss_bytes) = memory_usage().unwrap_or((0, 0));
        ProcessStats {
            rss_bytes,
            peak_rss_bytes,
            cpu_percent: cpu_percent.clamp(0.0, 100.0 * self.cores),
            uptime: now.duration_since(self.start),
        }
    }
}

/// Current and peak resident bytes, or `None` where unsupported.
pub fn memory_usage() -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        // SAFETY: `counters` is a correctly sized, writable PROCESS_MEMORY_COUNTERS.
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        Some((counters.WorkingSetSize as u64, counters.PeakWorkingSetSize as u64))
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page_size = 4096;
        let rss = resident_pages * page_size;
        let peak = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|l| l.starts_with("VmHWM:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
            .unwrap_or(rss);
        Some((rss, peak))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// Total CPU time (kernel + user) consumed by this process.
pub fn process_cpu_time() -> Option<Duration> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

        let mut creation = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        // SAFETY: all four out-parameters are valid, writable FILETIMEs.
        let ok = unsafe {
            GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user)
        };
        if ok == 0 {
            return None;
        }
        let to_nanos = |ft: FILETIME| -> u64 {
            // FILETIME counts 100-nanosecond intervals.
            (((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64) * 100
        };
        Some(Duration::from_nanos(to_nanos(kernel) + to_nanos(user)))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let fields: Vec<&str> = stat.rsplit(')').next()?.split_whitespace().collect();
        // utime and stime are fields 14 and 15 (1-based) of /proc/pid/stat,
        // which is index 11 and 12 after the comm field.
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        let ticks_per_second = 100u64;
        Some(Duration::from_secs_f64((utime + stime) as f64 / ticks_per_second as f64))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// CPU time (kernel + user) consumed by the **calling thread**.
///
/// This is what the scheduler's per-task accounting samples: a worker runs one
/// task at a time, so the difference across a task's execution is that task's
/// CPU cost (amendment section 6).
///
/// Returns `None` where the platform cannot report it. Linux would need
/// `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, which is not reachable from `std`
/// without a libc dependency, so only Windows reports a value today.
pub fn thread_cpu_time() -> Option<Duration> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

        let mut creation = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        // SAFETY: all four out-parameters are valid, writable FILETIMEs, and
        // the pseudo-handle from GetCurrentThread needs no cleanup.
        let ok = unsafe {
            GetThreadTimes(GetCurrentThread(), &mut creation, &mut exit, &mut kernel, &mut user)
        };
        if ok == 0 {
            return None;
        }
        let to_nanos = |ft: FILETIME| -> u64 {
            (((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64) * 100
        };
        Some(Duration::from_nanos(to_nanos(kernel) + to_nanos(user)))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_usage_is_reported_on_supported_platforms() {
        if cfg!(any(windows, target_os = "linux")) {
            let (rss, peak) = memory_usage().expect("supported platform reports memory");
            assert!(rss > 0, "rss should be positive");
            assert!(peak >= rss, "peak {peak} should be at least current {rss}");
        }
    }

    #[test]
    fn cpu_time_is_monotonic() {
        if cfg!(any(windows, target_os = "linux")) {
            let first = process_cpu_time().expect("supported platform reports cpu time");
            let mut sink = 0u64;
            for i in 0..2_000_000u64 {
                sink = sink.wrapping_add(i);
            }
            assert!(sink > 0);
            let second = process_cpu_time().unwrap();
            assert!(second >= first, "cpu time went backwards: {second:?} < {first:?}");
        }
    }

    #[test]
    fn thread_cpu_time_is_monotonic_on_supported_platforms() {
        if cfg!(windows) {
            let first = thread_cpu_time().expect("windows reports thread cpu time");
            let mut sink = 0u64;
            for index in 0..3_000_000u64 {
                sink = sink.wrapping_add(index);
            }
            assert!(sink > 0);
            let second = thread_cpu_time().unwrap();
            assert!(second >= first, "thread cpu time went backwards: {second:?} < {first:?}");
        } else {
            assert_eq!(thread_cpu_time(), None, "unsupported platforms report nothing");
        }
    }

    #[test]
    fn thread_cpu_time_is_per_thread_not_per_process() {
        if !cfg!(windows) {
            return;
        }
        // Burn CPU on a second thread; this thread's own counter must not move
        // anywhere near as much as the process counter does.
        let before_thread = thread_cpu_time().unwrap();
        let worker = std::thread::spawn(|| {
            let mut sink = 0u64;
            for index in 0..40_000_000u64 {
                sink = sink.wrapping_add(index);
            }
            sink
        });
        assert!(worker.join().unwrap() > 0);
        let after_thread = thread_cpu_time().unwrap();
        assert!(
            after_thread.saturating_sub(before_thread) < Duration::from_millis(50),
            "another thread's work leaked into this thread's accounting"
        );
    }

    #[test]
    fn sampler_reports_plausible_values() {
        let mut sampler = ProcessSampler::new();
        let stats = sampler.sample();
        if cfg!(any(windows, target_os = "linux")) {
            assert!(stats.rss_mb() > 0.0);
        }
        assert!(stats.cpu_percent >= 0.0);
        assert!(stats.uptime < Duration::from_secs(60));
    }
}
