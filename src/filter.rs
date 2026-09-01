//! Rules that decide whether an entry is counted.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use jiff::civil::Date;
use jiff::{Timestamp, Zoned};
use regex::Regex;

/// Which file extensions are counted.
#[derive(Debug)]
pub enum TypeFilter {
    /// Count only these extensions.
    Only(Vec<String>),
    /// Count everything except these extensions.
    Except(Vec<String>),
}

impl TypeFilter {
    fn accepts(&self, name: &str) -> bool {
        let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
        match self {
            Self::Only(types) => types.iter().any(|t| t.eq_ignore_ascii_case(extension)),
            Self::Except(types) => !types.iter().any(|t| t.eq_ignore_ascii_case(extension)),
        }
    }
}

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// An inclusive mtime window, in Unix nanoseconds.
#[derive(Debug, Default, Clone, Copy)]
pub struct TimeWindow {
    pub from: Option<i128>,
    pub to: Option<i128>,
}

impl TimeWindow {
    fn accepts_nanosecond(&self, mtime: i128) -> bool {
        self.from.is_none_or(|from| mtime >= from) && self.to.is_none_or(|to| mtime <= to)
    }

    /// Builds a window from the absolute and relative time flags.
    pub fn parse(
        since: Option<&str>,
        until: Option<&str>,
        max_age: Option<&str>,
        min_age: Option<&str>,
        now: &Zoned,
    ) -> Result<Self, String> {
        let mut window = Self::default();

        if let Some(since) = since {
            window.from = Some(parse_instant(since, now, DayEdge::Start)?);
        }
        if let Some(until) = until {
            window.to = Some(parse_instant(until, now, DayEdge::End)?);
        }
        if let Some(max_age) = max_age {
            let bound = now.timestamp().as_nanosecond()
                - i128::from(parse_duration(max_age)?) * NANOS_PER_SECOND;
            window.from = Some(window.from.map_or(bound, |from| from.max(bound)));
        }
        if let Some(min_age) = min_age {
            let bound = now.timestamp().as_nanosecond()
                - i128::from(parse_duration(min_age)?) * NANOS_PER_SECOND;
            window.to = Some(window.to.map_or(bound, |to| to.min(bound)));
        }
        Ok(window)
    }
}

enum DayEdge {
    Start,
    End,
}

/// Parses an RFC 3339 timestamp or a local calendar date.
fn parse_instant(value: &str, now: &Zoned, edge: DayEdge) -> Result<i128, String> {
    if let Ok(timestamp) = value.parse::<Timestamp>() {
        return Ok(timestamp.as_nanosecond());
    }

    let date: Date = value
        .parse()
        .map_err(|_| format!("`{value}` is neither an RFC 3339 timestamp nor a YYYY-MM-DD date"))?;
    let civil = match edge {
        DayEdge::Start => date.at(0, 0, 0, 0),
        DayEdge::End => date.at(23, 59, 59, 999_999_999),
    };
    civil
        .to_zoned(now.time_zone().clone())
        .map(|zoned| zoned.timestamp().as_nanosecond())
        .map_err(|err| format!("`{value}` has no valid local time: {err}"))
}

/// Parses compact durations such as `7d`, `2h30m`, and `1y2mo`.
pub fn parse_duration(input: &str) -> Result<i64, String> {
    const YEAR: i64 = 365 * 24 * 3600;
    const MONTH: i64 = 30 * 24 * 3600;

    let mut total = 0i64;
    let mut digits = String::new();
    let mut unit = String::new();
    let mut saw_unit = false;

    let mut flush = |digits: &mut String, unit: &mut String| -> Result<(), String> {
        if digits.is_empty() {
            return Err(format!("`{input}` is missing a number before `{unit}`"));
        }
        let count: i64 = digits
            .parse()
            .map_err(|_| format!("`{digits}` in `{input}` is not a whole number"))?;
        let seconds = match unit.as_str() {
            "y" => YEAR,
            "mo" => MONTH,
            "w" => 7 * 24 * 3600,
            "d" => 24 * 3600,
            "h" => 3600,
            "m" => 60,
            "s" => 1,
            "" => return Err(format!("`{input}` is missing a unit after `{digits}`")),
            other => return Err(format!("`{other}` in `{input}` is not a known unit")),
        };
        let amount = count
            .checked_mul(seconds)
            .ok_or_else(|| format!("`{input}` is too large"))?;
        total = total
            .checked_add(amount)
            .ok_or_else(|| format!("`{input}` is too large"))?;
        digits.clear();
        unit.clear();
        Ok(())
    };

    for ch in input.trim().chars() {
        if ch.is_ascii_digit() {
            if !unit.is_empty() {
                flush(&mut digits, &mut unit)?;
            }
            digits.push(ch);
        } else {
            saw_unit = true;
            unit.push(ch);
        }
    }
    if !saw_unit {
        return Err(format!("`{input}` is missing a unit"));
    }
    flush(&mut digits, &mut unit)?;

    Ok(total)
}

/// The complete set of inclusion rules for one scan.
#[derive(Debug, Default)]
pub struct Filters {
    ignored_paths: HashSet<PathBuf>,
    ignored_pattern: Option<Regex>,
    ignore_hidden: bool,
    types: Option<TypeFilter>,
    time: TimeWindow,
}

impl Filters {
    /// Registers directories to skip.
    pub fn ignore_paths<I, S>(&mut self, root: &Path, paths: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<Path>,
    {
        for path in paths {
            let path = path.as_ref();
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            if let Ok(absolute) = std::path::absolute(resolved) {
                self.ignored_paths.insert(normalize_path(&absolute));
            }
        }
        self
    }

    /// Skips directories whose full path matches any of these patterns.
    pub fn ignore_patterns<I, S>(&mut self, patterns: I) -> Result<&mut Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let alternatives: Vec<String> = patterns
            .into_iter()
            .map(|p| format!("({})", p.as_ref()))
            .collect();
        if alternatives.is_empty() {
            return Ok(self);
        }

        let combined = format!("^(?:{})$", alternatives.join("|"));
        self.ignored_pattern =
            Some(Regex::new(&combined).map_err(|err| format!("invalid ignore pattern: {err}"))?);
        Ok(self)
    }

    pub fn ignore_hidden(&mut self, value: bool) -> &mut Self {
        self.ignore_hidden = value;
        self
    }

    pub fn types(&mut self, filter: Option<TypeFilter>) -> &mut Self {
        self.types = filter;
        self
    }

    pub fn time(&mut self, window: TimeWindow) -> &mut Self {
        self.time = window;
        self
    }

    pub(crate) fn needs_path(&self) -> bool {
        !self.ignored_paths.is_empty() || self.ignored_pattern.is_some()
    }

    pub(crate) fn has_file_filter(&self) -> bool {
        self.types.is_some()
    }

    pub(crate) fn has_dir_filter(&self) -> bool {
        self.ignore_hidden || self.needs_path()
    }

    pub fn skip_dir(&self, name: &OsStr, path: &Path) -> bool {
        if self.ignore_hidden && is_hidden(name) {
            return true;
        }
        if !self.ignored_paths.is_empty() && self.ignored_paths.contains(&normalize_path(path)) {
            return true;
        }
        self.ignored_pattern
            .as_ref()
            .is_some_and(|re| re.is_match(&path.to_string_lossy()))
    }

    /// Rejects a file by name before `stat`.
    pub fn skip_file(&self, name: &str) -> bool {
        self.types.as_ref().is_some_and(|t| !t.accepts(name))
    }

    pub fn accepts_mtime(&self, seconds: i64, nanoseconds: i64) -> bool {
        if self.time.from.is_none() && self.time.to.is_none() {
            return true;
        }
        self.time
            .accepts_nanosecond(i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let rooted = path.is_absolute();
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !rooted {
                    normalized.push("..");
                }
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    normalized
}

fn is_hidden(name: &OsStr) -> bool {
    name.as_bytes().first() == Some(&b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_compound_and_reject_nonsense() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("2h30m").unwrap(), 2 * 3600 + 1800);
        assert_eq!(parse_duration("7d").unwrap(), 7 * 86400);
        assert_eq!(parse_duration("1w").unwrap(), 7 * 86400);
        assert_eq!(parse_duration("1mo").unwrap(), 30 * 86400);
        assert_eq!(
            parse_duration("1y2mo").unwrap(),
            365 * 86400 + 2 * 30 * 86400
        );

        assert!(parse_duration("7").is_err(), "a bare number has no unit");
        assert!(parse_duration("d").is_err(), "a bare unit has no number");
        assert!(parse_duration("7x").is_err(), "x is not a unit");
        assert!(parse_duration("").is_err());
        assert!(
            parse_duration("9223372036854775807m").is_err(),
            "duration components must not overflow"
        );
    }

    #[test]
    fn time_window_bounds_combine_absolute_and_relative_flags() {
        let now: Zoned = "2026-08-21T12:00:00+00:00[UTC]".parse().unwrap();
        let epoch_now = now.timestamp().as_nanosecond();
        let day = 86_400i128 * NANOS_PER_SECOND;

        let window = TimeWindow::parse(None, None, Some("7d"), None, &now).unwrap();
        assert_eq!(window.from, Some(epoch_now - 7 * day));
        assert!(
            window.accepts_nanosecond(epoch_now - day),
            "a day old is within 7d"
        );
        assert!(!window.accepts_nanosecond(epoch_now - 8 * day));

        let window = TimeWindow::parse(None, None, None, Some("30d"), &now).unwrap();
        assert_eq!(window.to, Some(epoch_now - 30 * day));
        assert!(
            window.accepts_nanosecond(epoch_now - 31 * day),
            "31 days is over 30"
        );
        assert!(!window.accepts_nanosecond(epoch_now));

        let window = TimeWindow::parse(None, Some("2026-08-21"), None, None, &now).unwrap();
        assert!(window.accepts_nanosecond(epoch_now + 11 * 3_600i128 * NANOS_PER_SECOND));
        assert!(!window.accepts_nanosecond(epoch_now + 13 * 3_600i128 * NANOS_PER_SECOND));

        assert!(TimeWindow::parse(Some("nope"), None, None, None, &now).is_err());
    }

    #[test]
    fn absolute_time_filters_keep_subsecond_precision() {
        let now: Zoned = "2026-08-21T12:00:00+00:00[UTC]".parse().unwrap();
        let window = TimeWindow::parse(
            Some("2026-08-21T12:00:00.750000000Z"),
            None,
            None,
            None,
            &now,
        )
        .unwrap();
        let second = now.timestamp().as_nanosecond();

        assert!(!window.accepts_nanosecond(second + 500_000_000));
        assert!(window.accepts_nanosecond(second + 750_000_000));
    }

    #[test]
    fn type_filters_match_on_extension_case_insensitively() {
        let only = TypeFilter::Only(vec!["yaml".into(), "json".into()]);
        assert!(only.accepts("config.yaml"));
        assert!(only.accepts("CONFIG.JSON"));
        assert!(!only.accepts("notes.txt"));
        assert!(!only.accepts("README"));

        let except = TypeFilter::Except(vec!["log".into()]);
        assert!(except.accepts("notes.txt"));
        assert!(!except.accepts("server.log"));
    }

    #[test]
    fn directories_are_skipped_by_name_path_and_pattern() {
        let mut filters = Filters::default();
        filters
            .ignore_hidden(true)
            .ignore_paths(Path::new("/home/me"), ["/proc", "/sys"])
            .ignore_patterns([r".*/node_modules"])
            .unwrap();

        assert!(filters.skip_dir(OsStr::new(".git"), Path::new("/home/me/.git")));
        assert!(filters.skip_dir(OsStr::new("proc"), Path::new("/proc")));
        assert!(filters.skip_dir(
            OsStr::new("node_modules"),
            Path::new("/srv/app/node_modules")
        ));
        assert!(!filters.skip_dir(OsStr::new("src"), Path::new("/srv/app/src")));
    }

    #[test]
    fn relative_ignored_paths_are_resolved_from_the_scan_root() {
        let mut filters = Filters::default();
        filters.ignore_paths(Path::new("/srv/app"), ["./tmp/../tmp"]);

        assert!(filters.skip_dir(OsStr::new("tmp"), Path::new("/srv/app/tmp")));
        assert!(filters.skip_dir(OsStr::new("tmp"), Path::new("/srv/app/../app/tmp")));
        assert!(!filters.skip_dir(OsStr::new("tmp"), Path::new("/other/tmp")));
    }
}
