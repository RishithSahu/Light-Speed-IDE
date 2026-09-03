//! Version-controlled synthetic workloads (specification section 52).
//!
//! Stage 1 measures single-document editing, so the workloads are document
//! sizes rather than repository shapes. Each is generated deterministically
//! from a fixed seed, so a result from one machine is comparable with a result
//! from another and with the same machine next week.

/// Workload definition version. Bump when generation changes, so old results
/// are not silently compared against new ones.
pub const WORKLOAD_VERSION: u32 = 1;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    pub name: &'static str,
    pub target_bytes: usize,
    /// Text with a realistic mix of code-like lines.
    pub flavor: Flavor,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// ASCII source-like text, ~40 characters per line.
    Source,
    /// Heavy multi-byte content: CJK, accents, emoji, combining marks.
    Unicode,
    /// One extremely long line, the pathological layout case.
    SingleLine,
}

pub const DOCUMENT_WORKLOADS: &[Workload] = &[
    Workload { name: "D1_1KB", target_bytes: 1024, flavor: Flavor::Source },
    Workload { name: "D2_64KB", target_bytes: 64 * 1024, flavor: Flavor::Source },
    Workload { name: "D3_1MB", target_bytes: 1024 * 1024, flavor: Flavor::Source },
    Workload { name: "D4_10MB", target_bytes: 10 * 1024 * 1024, flavor: Flavor::Source },
    Workload { name: "D5_100MB", target_bytes: 100 * 1024 * 1024, flavor: Flavor::Source },
];

pub const UNICODE_WORKLOAD: Workload =
    Workload { name: "U1_1MB_unicode", target_bytes: 1024 * 1024, flavor: Flavor::Unicode };

pub const LONG_LINE_WORKLOAD: Workload = Workload {
    name: "L1_10MB_one_line",
    target_bytes: 10 * 1024 * 1024,
    flavor: Flavor::SingleLine,
};

/// Deterministic pseudo-random generator, so workloads are reproducible without
/// pulling in a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn pick<'a, T>(&mut self, values: &'a [T]) -> &'a T {
        &values[(self.next() % values.len() as u64) as usize]
    }
}

const SOURCE_LINES: &[&str] = &[
    "fn compute_offset(index: usize, width: usize) -> usize {",
    "    let mut total = 0;",
    "    for (position, value) in values.iter().enumerate() {",
    "        total += value * position;",
    "    }",
    "    total",
    "}",
    "",
    "// Adjust the running total before the next pass.",
    "pub struct Configuration {",
    "    pub name: String,",
    "    pub retries: u32,",
    "}",
    "let result = registry.lookup(name).unwrap_or_default();",
    "assert_eq!(buffer.len_chars(), expected_length);",
];

const UNICODE_LINES: &[&str] = &[
    "\u{4F60}\u{597D}\u{4E16}\u{754C} - wide characters take two columns",
    "caf\u{e9} na\u{ef}ve r\u{e9}sum\u{e9} \u{fc}ber",
    "emoji: \u{1F600}\u{1F680}\u{1F469}\u{200D}\u{1F467} family and rocket",
    "combining: e\u{0301}a\u{0300}o\u{0308}u\u{030A}",
    "\u{440}\u{443}\u{441}\u{441}\u{43A}\u{438}\u{439} \u{442}\u{435}\u{43A}\u{441}\u{442}",
    "\u{1F1EF}\u{1F1F5} \u{1F1FA}\u{1F1F8} flags are two scalars each",
    "mixed ascii and \u{6F22}\u{5B57} in one line",
];

/// Generates a workload's text.
pub fn generate(workload: Workload) -> String {
    let mut rng = Rng::new(0x5EED_1234_5678_9ABC);
    let mut text = String::with_capacity(workload.target_bytes + 128);
    match workload.flavor {
        Flavor::Source => {
            while text.len() < workload.target_bytes {
                text.push_str(rng.pick(SOURCE_LINES));
                text.push('\n');
            }
        }
        Flavor::Unicode => {
            while text.len() < workload.target_bytes {
                text.push_str(rng.pick(UNICODE_LINES));
                text.push('\n');
            }
        }
        Flavor::SingleLine => {
            while text.len() < workload.target_bytes {
                text.push_str(rng.pick(SOURCE_LINES));
                text.push(' ');
            }
        }
    }
    text
}

/// Describes a workload for the report header.
pub fn describe(workload: Workload, text: &str) -> String {
    let lines = text.lines().count().max(1);
    format!(
        "{} - {} bytes, {} lines, {:.0} bytes/line, flavor {:?}",
        workload.name,
        text.len(),
        lines,
        text.len() as f64 / lines as f64,
        workload.flavor
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic() {
        let workload = DOCUMENT_WORKLOADS[0];
        assert_eq!(generate(workload), generate(workload));
    }

    #[test]
    fn workloads_reach_their_target_size() {
        for workload in &DOCUMENT_WORKLOADS[..3] {
            let text = generate(*workload);
            assert!(text.len() >= workload.target_bytes);
            assert!(text.len() < workload.target_bytes + 200, "overshoot should be bounded");
        }
    }

    #[test]
    fn the_unicode_workload_is_multi_byte() {
        let text = generate(UNICODE_WORKLOAD);
        assert!(text.chars().count() < text.len(), "should contain multi-byte characters");
    }

    #[test]
    fn the_long_line_workload_has_one_line() {
        let workload = Workload { target_bytes: 4096, ..LONG_LINE_WORKLOAD };
        let text = generate(workload);
        assert_eq!(text.lines().count(), 1);
    }
}
