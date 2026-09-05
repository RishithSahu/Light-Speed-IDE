//! LightSpeed structured logging subsystem (specification section 11).
//!
//! Logging is a subsystem, not a pile of `println!` calls. Every record carries
//! a timestamp, level, subsystem, event id, message and optional typed fields:
//!
//! ```text
//! 2026-08-24T09:14:22.481Z INFO  core/document_opened  opened document  path="src/main.rs" bytes=1841
//! ```
//!
//! # Security (specification section 11.3)
//!
//! Logs must never automatically capture secrets (tokens, passwords, keys,
//! cookies, environment secrets) and must never log document contents by
//! default. Two mechanisms back that rule up:
//!
//! * [`Field::redacted`] renders as `***`, so a call site can name a value
//!   without disclosing it.
//! * string fields are truncated to [`MAX_FIELD_CHARS`], so an accidental large
//!   value cannot dump a file into the log.
//!
//! Logging document text is a code-review error; the architecture test suite
//! additionally rejects stray `println!`/`dbg!` calls in the library crates.

pub mod diag;

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum number of characters kept from a string field.
pub const MAX_FIELD_CHARS: usize = 256;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// `0` disables all logging; otherwise the numeric value of a [`Level`].
static MAX_LEVEL: AtomicU8 = AtomicU8::new(0);

/// Value of a structured log field.
#[derive(Debug, Clone, Copy)]
pub enum FieldValue<'a> {
    Str(&'a str),
    Int(i64),
    Uint(u64),
    Float(f64),
    Bool(bool),
    /// A value that exists but must never be written to the log.
    Redacted,
}

/// A single structured key/value pair attached to a [`LogRecord`].
#[derive(Debug, Clone, Copy)]
pub struct Field<'a> {
    pub key: &'static str,
    pub value: FieldValue<'a>,
}

impl<'a> Field<'a> {
    pub fn str(key: &'static str, value: &'a str) -> Self {
        Field { key, value: FieldValue::Str(value) }
    }
    pub fn int(key: &'static str, value: i64) -> Self {
        Field { key, value: FieldValue::Int(value) }
    }
    pub fn uint(key: &'static str, value: u64) -> Self {
        Field { key, value: FieldValue::Uint(value) }
    }
    pub fn float(key: &'static str, value: f64) -> Self {
        Field { key, value: FieldValue::Float(value) }
    }
    pub fn bool(key: &'static str, value: bool) -> Self {
        Field { key, value: FieldValue::Bool(value) }
    }
    /// Names a value without disclosing it (specification section 11.3).
    pub fn redacted(key: &'static str) -> Self {
        Field { key, value: FieldValue::Redacted }
    }
}

impl fmt::Display for FieldValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldValue::Str(s) => {
                let truncated: String = s.chars().take(MAX_FIELD_CHARS).collect();
                if truncated.len() < s.len() {
                    write!(f, "{:?}~", truncated)
                } else {
                    write!(f, "{:?}", truncated)
                }
            }
            FieldValue::Int(v) => write!(f, "{v}"),
            FieldValue::Uint(v) => write!(f, "{v}"),
            FieldValue::Float(v) => write!(f, "{v:.3}"),
            FieldValue::Bool(v) => write!(f, "{v}"),
            FieldValue::Redacted => f.write_str("***"),
        }
    }
}

/// One immutable log record (specification section 11.2).
pub struct LogRecord<'a> {
    pub level: Level,
    pub subsystem: &'static str,
    pub event: &'static str,
    pub message: fmt::Arguments<'a>,
    pub fields: &'a [Field<'a>],
}

enum Sink {
    Stderr,
    File(Mutex<BufWriter<File>>),
    Capture(Arc<Mutex<Vec<String>>>),
    Null,
}

struct Logger {
    sinks: Vec<Sink>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Where log output is written.
pub enum LogTarget {
    Stderr,
    File(std::path::PathBuf),
    StderrAndFile(std::path::PathBuf),
    /// In-memory sink used by tests.
    Capture(Arc<Mutex<Vec<String>>>),
    Null,
}

pub struct LogConfig {
    pub level: Level,
    pub target: LogTarget,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig { level: Level::Info, target: LogTarget::Stderr }
    }
}

/// Installs the logging subsystem. Later calls are ignored: the logger is
/// process-global and installed exactly once.
pub fn init(config: LogConfig) {
    let sinks = match config.target {
        LogTarget::Stderr => vec![Sink::Stderr],
        LogTarget::File(path) => vec![open_file_sink(&path)],
        LogTarget::StderrAndFile(path) => vec![Sink::Stderr, open_file_sink(&path)],
        LogTarget::Capture(buf) => vec![Sink::Capture(buf)],
        LogTarget::Null => vec![Sink::Null],
    };
    if LOGGER.set(Logger { sinks }).is_ok() {
        set_level(config.level);
    }
}

/// Initializes from `LIGHTSPEED_LOG` (level) and `LIGHTSPEED_LOG_FILE` (path).
/// Without `LIGHTSPEED_LOG`, `default_level` is used.
pub fn init_from_env(default_level: Level) {
    let level = std::env::var("LIGHTSPEED_LOG")
        .ok()
        .and_then(|v| Level::parse(&v))
        .unwrap_or(default_level);
    let target = match std::env::var("LIGHTSPEED_LOG_FILE") {
        Ok(path) if !path.is_empty() => LogTarget::StderrAndFile(path.into()),
        _ => LogTarget::Stderr,
    };
    init(LogConfig { level, target });
}

fn open_file_sink(path: &Path) -> Sink {
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(file) => Sink::File(Mutex::new(BufWriter::new(file))),
        Err(_) => Sink::Stderr,
    }
}

pub fn set_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn disable() {
    MAX_LEVEL.store(0, Ordering::Relaxed);
}

pub fn level() -> Option<Level> {
    match MAX_LEVEL.load(Ordering::Relaxed) {
        1 => Some(Level::Error),
        2 => Some(Level::Warn),
        3 => Some(Level::Info),
        4 => Some(Level::Debug),
        5 => Some(Level::Trace),
        _ => None,
    }
}

/// Fast-path guard used by the logging macros.
#[inline]
pub fn enabled(level: Level) -> bool {
    (level as u8) <= MAX_LEVEL.load(Ordering::Relaxed)
}

/// The current time as `YYYY-MM-DDThh:mm:ss.mmmZ`, the exact format every
/// log line already uses. Exposed so other permanent, human-readable
/// records (the terminal panel's transcript, for one) can stamp themselves
/// the same way instead of each growing its own date arithmetic.
pub fn timestamp_now() -> String {
    let mut out = String::with_capacity(24);
    format_timestamp(SystemTime::now(), &mut out);
    out
}

/// Formats and writes a record. Called by the macros; prefer those.
pub fn emit(record: &LogRecord<'_>) {
    let Some(logger) = LOGGER.get() else { return };

    let mut line = String::with_capacity(160);
    format_timestamp(SystemTime::now(), &mut line);
    line.push(' ');
    line.push_str(record.level.as_str());
    line.push(' ');
    line.push_str(record.subsystem);
    line.push('/');
    line.push_str(record.event);
    line.push_str("  ");
    let _ = fmt::Write::write_fmt(&mut line, record.message);
    for field in record.fields {
        line.push(' ');
        line.push_str(field.key);
        line.push('=');
        let _ = fmt::Write::write_fmt(&mut line, format_args!("{}", field.value));
    }
    line.push('\n');

    for sink in &logger.sinks {
        match sink {
            Sink::Stderr => {
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            Sink::File(file) => {
                if let Ok(mut file) = file.lock() {
                    let _ = file.write_all(line.as_bytes());
                    let _ = file.flush();
                }
            }
            Sink::Capture(buf) => {
                if let Ok(mut buf) = buf.lock() {
                    buf.push(line.trim_end().to_string());
                }
            }
            Sink::Null => {}
        }
    }
}

/// Writes `YYYY-MM-DDThh:mm:ss.mmmZ` without pulling in a date library.
fn format_timestamp(time: SystemTime, out: &mut String) {
    let dur = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    let _ = fmt::Write::write_fmt(
        out,
        format_args!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z"),
    );
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[doc(hidden)]
#[macro_export]
macro_rules! __record {
    ($lvl:expr, $sub:expr, $ev:expr, fields: [$($field:expr),* $(,)?], $($arg:tt)+) => {{
        if $crate::enabled($lvl) {
            $crate::emit(&$crate::LogRecord {
                level: $lvl,
                subsystem: $sub,
                event: $ev,
                message: ::core::format_args!($($arg)+),
                fields: &[$($field),*],
            });
        }
    }};
    ($lvl:expr, $sub:expr, $ev:expr, $($arg:tt)+) => {{
        if $crate::enabled($lvl) {
            $crate::emit(&$crate::LogRecord {
                level: $lvl,
                subsystem: $sub,
                event: $ev,
                message: ::core::format_args!($($arg)+),
                fields: &[],
            });
        }
    }};
}

/// `error!(subsystem, event, [fields: [..],] "message {}", args)`
#[macro_export]
macro_rules! error {
    ($sub:expr, $ev:expr, $($arg:tt)+) => { $crate::__record!($crate::Level::Error, $sub, $ev, $($arg)+) };
}

/// `warn!(subsystem, event, [fields: [..],] "message {}", args)`
#[macro_export]
macro_rules! warn {
    ($sub:expr, $ev:expr, $($arg:tt)+) => { $crate::__record!($crate::Level::Warn, $sub, $ev, $($arg)+) };
}

/// `info!(subsystem, event, [fields: [..],] "message {}", args)`
#[macro_export]
macro_rules! info {
    ($sub:expr, $ev:expr, $($arg:tt)+) => { $crate::__record!($crate::Level::Info, $sub, $ev, $($arg)+) };
}

/// `debug!(subsystem, event, [fields: [..],] "message {}", args)`
#[macro_export]
macro_rules! debug {
    ($sub:expr, $ev:expr, $($arg:tt)+) => { $crate::__record!($crate::Level::Debug, $sub, $ev, $($arg)+) };
}

/// `trace!(subsystem, event, [fields: [..],] "message {}", args)`
#[macro_export]
macro_rules! trace {
    ($sub:expr, $ev:expr, $($arg:tt)+) => { $crate::__record!($crate::Level::Trace, $sub, $ev, $($arg)+) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(20_689), (2026, 8, 24));
    }

    #[test]
    fn timestamp_is_iso8601() {
        let mut out = String::new();
        format_timestamp(
            UNIX_EPOCH + std::time::Duration::from_millis(1_600_000_000_123),
            &mut out,
        );
        assert_eq!(out, "2020-09-13T12:26:40.123Z");
    }

    #[test]
    fn secret_fields_are_never_rendered() {
        assert_eq!(format!("{}", Field::redacted("token").value), "***");
    }

    #[test]
    fn long_string_fields_are_truncated() {
        let big = "x".repeat(4096);
        let rendered = format!("{}", FieldValue::Str(&big));
        assert!(rendered.chars().count() <= MAX_FIELD_CHARS + 4, "{}", rendered.len());
    }

    #[test]
    fn level_filtering_is_ordered() {
        set_level(Level::Warn);
        assert!(enabled(Level::Error));
        assert!(enabled(Level::Warn));
        assert!(!enabled(Level::Info));
        assert!(!enabled(Level::Trace));
        disable();
        assert!(!enabled(Level::Error));
    }
}
