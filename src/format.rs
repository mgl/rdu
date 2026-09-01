//! Human-readable size and count formatting.

/// How to render a byte count.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum SizeFormat {
    /// Binary prefixes (KiB, MiB, ...).
    #[default]
    Binary,
    /// SI prefixes (kB, MB, ...).
    Si,
    /// Raw bytes without a unit.
    Raw,
    /// A fixed unit, such as KiB.
    Fixed { divisor: f64, suffix: &'static str },
}

impl SizeFormat {
    /// Resolves the size-format flags.
    pub fn new(si: bool, no_prefix: bool, fixed: bool) -> Self {
        match (no_prefix, fixed, si) {
            (true, _, _) => Self::Raw,
            (_, true, true) => Self::Fixed {
                divisor: 1e3,
                suffix: " kB",
            },
            (_, true, false) => Self::Fixed {
                divisor: 1024.0,
                suffix: " KiB",
            },
            (_, false, true) => Self::Si,
            (_, false, false) => Self::Binary,
        }
    }
}

const BINARY_UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
const SI_UNITS: [&str; 6] = ["kB", "MB", "GB", "TB", "PB", "EB"];

/// Formats a byte count, preserving negative values.
pub fn format_size(size: i64, format: SizeFormat) -> String {
    match format {
        SizeFormat::Raw => size.to_string(),
        SizeFormat::Fixed { divisor, suffix } => {
            format!("{:.1}{suffix}", size as f64 / divisor)
        }
        SizeFormat::Binary => scale(size, 1024.0, &BINARY_UNITS),
        SizeFormat::Si => scale(size, 1000.0, &SI_UNITS),
    }
}

fn scale(size: i64, base: f64, units: &[&str; 6]) -> String {
    let value = size as f64;
    let magnitude = value.abs();
    if magnitude < base {
        return format!("{size} B");
    }

    let mut divisor = base;
    for (i, unit) in units.iter().enumerate() {
        let next = divisor * base;
        if magnitude < next || i + 1 == units.len() {
            return format!("{:.1} {unit}", value / divisor);
        }
        divisor = next;
    }
    unreachable!("the loop always returns on its last iteration")
}

/// Formats an item count compactly: `1.5k`, `2.3M`, `4.1G`.
pub fn format_count(count: i64) -> String {
    match count {
        c if c >= 1_000_000_000 => format!("{:.1}G", c as f64 / 1e9),
        c if c >= 1_000_000 => format!("{:.1}M", c as f64 / 1e6),
        c if c >= 1_000 => format!("{:.1}k", c as f64 / 1e3),
        c => c.to_string(),
    }
}

/// Formats a number with thousands separators, for progress lines.
pub fn format_number(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() * 4 / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Escapes control and bidi characters for terminal output.
pub fn escape_name(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\\' => out.push_str("\\\\"),
            '\u{1b}' => out.push_str("\\x1b"),
            '\u{7f}' => out.push_str("\\x7f"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{{{:04x}}}", ch as u32);
            }
            ch if is_bidi_control(ch) => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{{{:04x}}}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{206f}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_cover_every_prefix_and_boundary() {
        let cases = [
            (0, SizeFormat::Binary, "0 B"),
            (1023, SizeFormat::Binary, "1023 B"),
            (1024, SizeFormat::Binary, "1.0 KiB"),
            (1536, SizeFormat::Binary, "1.5 KiB"),
            (1024 * 1024, SizeFormat::Binary, "1.0 MiB"),
            (1024i64.pow(3), SizeFormat::Binary, "1.0 GiB"),
            (1024i64.pow(4), SizeFormat::Binary, "1.0 TiB"),
            (1024i64.pow(5), SizeFormat::Binary, "1.0 PiB"),
            (1024i64.pow(6), SizeFormat::Binary, "1.0 EiB"),
            (-2048, SizeFormat::Binary, "-2.0 KiB"),
            (999, SizeFormat::Si, "999 B"),
            (1000, SizeFormat::Si, "1.0 kB"),
            (1_500_000, SizeFormat::Si, "1.5 MB"),
            (1_000_000_000, SizeFormat::Si, "1.0 GB"),
            (4096, SizeFormat::Raw, "4096"),
        ];
        for (size, format, want) in cases {
            assert_eq!(format_size(size, format), want, "size {size} as {format:?}");
        }

        assert_eq!(
            format_size(2048, SizeFormat::new(false, false, true)),
            "2.0 KiB"
        );
        assert_eq!(
            format_size(2000, SizeFormat::new(true, false, true)),
            "2.0 kB"
        );
        assert_eq!(
            format_size(2000, SizeFormat::new(false, true, false)),
            "2000"
        );
    }

    #[test]
    fn counts_and_numbers() {
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1500), "1.5k");
        assert_eq!(format_count(2_500_000), "2.5M");
        assert_eq!(format_count(3_100_000_000), "3.1G");

        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
        assert_eq!(format_number(-1_234), "-1,234");
    }

    #[test]
    fn names_escape_terminal_controls_and_bidi_controls() {
        assert_eq!(
            escape_name("a\n\r\t\u{1b}\u{7f}\u{0001}\u{0085}\u{202e}\\z"),
            "a\\n\\r\\t\\x1b\\x7f\\u{0001}\\u{0085}\\u{202e}\\\\z"
        );
    }
}
