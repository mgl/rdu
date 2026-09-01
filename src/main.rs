//! rdu — a very fast disk usage analyzer.

mod filter;
mod format;
mod output;
mod report;
mod scan;
mod tree;
mod tui;

use std::error::Error;
use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Parser;

use filter::{Filters, TimeWindow, TypeFilter};
use format::{SizeFormat, escape_name, format_number, format_size};
use output::Printer;
use scan::{Options, Progress};

type Fallible = Result<(), Box<dyn Error>>;

#[derive(Parser, Debug)]
#[command(
    name = "rdu",
    version,
    about = "Very fast disk usage analyzer",
    max_term_width = 100
)]
struct Cli {
    /// Directory to analyze.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Show apparent size instead of disk usage.
    #[arg(short = 'a', long)]
    show_apparent_size: bool,

    /// Show the number of items in each directory.
    #[arg(short = 'C', long)]
    show_item_count: bool,

    /// Print only the total for the given path.
    #[arg(short, long, conflicts_with_all = ["top", "depth"])]
    summarize: bool,

    /// Show only the largest N files anywhere in the tree.
    #[arg(short, long, value_name = "N", conflicts_with = "depth")]
    top: Option<usize>,

    /// Show the directory structure down to this depth.
    #[arg(long, value_name = "N")]
    depth: Option<u32>,

    /// List smallest first.
    #[arg(long)]
    reverse_sort: bool,

    /// Do not colorize the output.
    #[arg(short = 'c', long)]
    no_color: bool,

    /// Use decimal SI prefixes (kB, MB) instead of binary ones (KiB, MiB).
    #[arg(long)]
    si: bool,

    /// Print raw byte counts with no unit.
    #[arg(long, conflicts_with_all = ["si", "show_in_kib"])]
    no_prefix: bool,

    /// Report every size in KiB (or kB with --si).
    #[arg(short = 'k', long)]
    show_in_kib: bool,

    /// Do not show scan progress.
    #[arg(short = 'p', long)]
    no_progress: bool,

    /// Ignore hidden directories (those beginning with a dot).
    #[arg(short = 'H', long)]
    no_hidden: bool,

    /// Paths to ignore.
    #[arg(short, long, value_name = "PATHS", value_delimiter = ',')]
    ignore_dirs: Vec<PathBuf>,

    /// Regular expressions matching paths to ignore.
    #[arg(short = 'I', long, value_name = "PATTERNS", value_delimiter = ',')]
    ignore_dirs_pattern: Vec<String>,

    /// Read ignore patterns from a file, one per line.
    #[arg(short = 'X', long, value_name = "FILE")]
    ignore_from: Option<PathBuf>,

    /// Count only these file types, e.g. --type yaml,json.
    #[arg(
        short = 'T',
        long = "type",
        value_name = "TYPES",
        value_delimiter = ',',
        conflicts_with = "exclude_type"
    )]
    include_types: Vec<String>,

    /// Skip these file types.
    #[arg(short = 'E', long, value_name = "TYPES", value_delimiter = ',')]
    exclude_type: Vec<String>,

    /// Report the size of a file symlink's target; directory symlinks are not followed.
    #[arg(short = 'L', long)]
    follow_symlinks: bool,

    /// Do not cross filesystem boundaries.
    #[arg(short = 'x', long)]
    no_cross: bool,

    /// Count only files modified at or after this RFC 3339 timestamp or date.
    #[arg(long, value_name = "WHEN")]
    since: Option<String>,

    /// Count only files modified at or before this RFC 3339 timestamp or date.
    #[arg(long, value_name = "WHEN")]
    until: Option<String>,

    /// Count only files no older than this, e.g. 7d, 2h30m.
    #[arg(long, value_name = "DURATION")]
    max_age: Option<String>,

    /// Count only files at least this old, e.g. 30d, 1w.
    #[arg(long, value_name = "DURATION")]
    min_age: Option<String>,

    /// Write the analysis to a JSON file, or to stdout with `-`.
    #[arg(short, long, value_name = "FILE")]
    output_file: Option<String>,

    /// Export only these attributes: name, asize, dsize, items, mtime, notreg.
    #[arg(long, value_name = "ATTRS", requires = "output_file")]
    output_attrs: Option<String>,

    /// Read a previous analysis from a JSON file, or from stdin with `-`.
    #[arg(short = 'f', long, value_name = "FILE")]
    input_file: Option<String>,

    /// Use at most this many cores.
    #[arg(short = 'm', long, value_name = "N")]
    max_cores: Option<usize>,

    /// Scan with a single thread, for rotating disks.
    #[arg(long, conflicts_with = "max_cores")]
    sequential: bool,

    /// Print to stdout instead of opening the browser.
    #[arg(short = 'n', long)]
    non_interactive: bool,
}

impl Cli {
    /// Returns whether interactive output should be used.
    fn interactive(&self) -> bool {
        !self.non_interactive
            && self.output_file.is_none()
            && !self.summarize
            && self.top.is_none()
            && self.depth.is_none()
            && io::stdout().is_terminal()
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if is_broken_pipe(err.as_ref()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rdu: {}", escape_name(&err.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Fallible {
    let tree = match &cli.input_file {
        Some(path) if path == "-" => report::import(io::stdin().lock())?,
        Some(path) => report::import(File::open(path)?)?,
        None => analyze(cli)?,
    };

    let size_format = SizeFormat::new(cli.si, cli.no_prefix, cli.show_in_kib);

    if cli.interactive() {
        let flag = tree.flag;
        tui::run(tree, cli.show_apparent_size, size_format)?;
        return ensure_complete(flag);
    }

    let stdout = io::stdout();
    let color = !cli.no_color && stdout.is_terminal();
    let mut out = BufWriter::new(stdout.lock());

    if let Some(destination) = &cli.output_file {
        let attributes = match &cli.output_attrs {
            Some(spec) => report::Attributes::parse(spec)?,
            None => report::Attributes::default(),
        };
        let timestamp = jiff::Timestamp::now().as_second();
        let version = env!("CARGO_PKG_VERSION");

        if destination == "-" {
            report::export(&tree, &mut out, attributes, version, timestamp)?;
            out.flush()?;
        } else {
            let mut file = BufWriter::new(File::create(destination)?);
            report::export(&tree, &mut file, attributes, version, timestamp)?;
            file.flush()?;
        }
        return ensure_complete(tree.flag);
    }

    let mut printer = Printer::new(&mut out, size_format, color);
    printer.apparent = cli.show_apparent_size;
    printer.show_item_count = cli.show_item_count;
    printer.reverse = cli.reverse_sort;

    match (cli.top, cli.depth, cli.summarize) {
        (Some(n), _, _) => printer.top(&tree, n)?,
        (_, Some(depth), _) => printer.depth(&tree, depth)?,
        (_, _, true) => printer.summary(&tree)?,
        _ => printer.dir(&tree)?,
    }

    out.flush()?;
    ensure_complete(tree.flag)
}

fn ensure_complete(flag: tree::Flag) -> Fallible {
    if matches!(flag, tree::Flag::Unreadable | tree::Flag::ChildUnreadable) {
        Err(io::Error::other("scan incomplete: unreadable paths were skipped").into())
    } else {
        Ok(())
    }
}

/// Scans the filesystem, showing progress on stderr unless it is suppressed.
fn analyze(cli: &Cli) -> Result<tree::Dir, Box<dyn Error>> {
    let root = std::path::absolute(&cli.path)?;
    let filters = build_filters(cli, &root)?;
    let options = Options {
        depth_cap: depth_cap(cli),
        follow_symlinks: cli.follow_symlinks,
        one_filesystem: cli.no_cross,
    };

    let threads = match (cli.sequential, cli.max_cores) {
        (true, _) => 1,
        (_, Some(cores)) => cores.max(1),
        // Metadata walks benefit from several lookups in flight per core.
        _ => std::thread::available_parallelism()
            .map_or(1, |cores| cores.get().saturating_mul(4).min(64)),
    };

    let show_progress = !cli.no_progress && io::stderr().is_terminal();
    let progress = Progress::new(show_progress);

    let done = AtomicBool::new(false);
    let tree = std::thread::scope(|scope| {
        if show_progress {
            scope.spawn(|| report_progress(&progress, &done));
        }

        let result = scan::run(&root, &filters, options, &progress, threads);
        done.store(true, Ordering::Relaxed);
        result
    })?;

    Ok(tree)
}

/// Returns the storage depth needed by the selected output.
fn depth_cap(cli: &Cli) -> Option<u32> {
    if cli.interactive() {
        return None;
    }
    match (&cli.output_file, cli.top, cli.depth, cli.summarize) {
        (Some(_), _, _, _) | (_, Some(_), _, _) => None,
        (_, _, Some(depth), _) => Some(depth),
        (_, _, _, true) => Some(0),
        _ => Some(1),
    }
}

fn build_filters(cli: &Cli, root: &Path) -> Result<Filters, Box<dyn Error>> {
    let mut filters = Filters::default();
    filters
        .ignore_hidden(cli.no_hidden)
        .ignore_paths(root, &cli.ignore_dirs);

    let mut patterns = cli.ignore_dirs_pattern.clone();
    if let Some(path) = &cli.ignore_from {
        patterns.extend(
            std::fs::read_to_string(path)?
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
        );
    }
    filters.ignore_patterns(&patterns)?;

    let types = match (cli.include_types.is_empty(), cli.exclude_type.is_empty()) {
        (false, _) => Some(TypeFilter::Only(lowercased(&cli.include_types))),
        (_, false) => Some(TypeFilter::Except(lowercased(&cli.exclude_type))),
        _ => None,
    };
    filters.types(types);

    if cli.since.is_some() || cli.until.is_some() || cli.max_age.is_some() || cli.min_age.is_some()
    {
        let now = jiff::Zoned::now();
        filters.time(TimeWindow::parse(
            cli.since.as_deref(),
            cli.until.as_deref(),
            cli.max_age.as_deref(),
            cli.min_age.as_deref(),
            &now,
        )?);
    }

    Ok(filters)
}

fn lowercased(types: &[String]) -> Vec<String> {
    types.iter().map(|t| t.to_ascii_lowercase()).collect()
}

/// Draws progress until the scan finishes.
fn report_progress(progress: &Progress, done: &AtomicBool) {
    const SPINNER: [char; 4] = ['|', '/', '-', '\\'];
    let mut frame = 0usize;
    let mut stderr = io::stderr().lock();

    while !done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        if done.load(Ordering::Relaxed) {
            break;
        }

        let current = progress.current_dir();
        let _ = write!(
            stderr,
            "\r\u{1b}[K {} {} items, {} — {}",
            SPINNER[frame % SPINNER.len()],
            format_number(progress.items()),
            format_size(progress.usage(), SizeFormat::Binary),
            elide(&escape_name(&current.to_string_lossy()), 60),
        );
        let _ = stderr.flush();
        frame += 1;
    }

    let _ = write!(stderr, "\r\u{1b}[K");
    let _ = stderr.flush();
}

/// Trims a path from the left to fit the progress line.
fn elide(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    format!(
        "...{}",
        chars[chars.len() - width + 3..].iter().collect::<String>()
    )
}

fn is_broken_pipe(err: &(dyn Error + 'static)) -> bool {
    err.downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_paths_are_escaped_before_elision() {
        let shown = elide(&escape_name("/tmp/a\n\u{1b}\u{202e}"), 60);
        assert_eq!(shown, r"/tmp/a\n\x1b\u{202e}");
    }

    #[test]
    fn type_filters_cannot_be_combined() {
        assert!(Cli::try_parse_from(["rdu", "--type", "txt", "--exclude-type", "log"]).is_err());
    }

    #[test]
    fn output_and_size_modes_reject_conflicting_flags() {
        assert!(Cli::try_parse_from(["rdu", "--top", "1", "--depth", "1"]).is_err());
        assert!(Cli::try_parse_from(["rdu", "--summarize", "--depth", "1"]).is_err());
        assert!(Cli::try_parse_from(["rdu", "--summarize", "--top", "1"]).is_err());
        assert!(Cli::try_parse_from(["rdu", "--no-prefix", "--show-in-kib"]).is_err());
    }

    #[test]
    fn incomplete_trees_return_an_error() {
        let mut tree = tree::Dir::new("/tmp/root");
        tree.flag = tree::Flag::ChildUnreadable;
        assert!(ensure_complete(tree.flag).is_err());
    }
}
