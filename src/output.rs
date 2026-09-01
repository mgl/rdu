//! Non-interactive text output.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::format::{SizeFormat, escape_name, format_count, format_size};
use crate::tree::{Dir, Entry, Order, SortBy};

const RESET: &str = "\u{1b}[0m";
const BLUE: &str = "\u{1b}[34;1m";
const ORANGE: &str = "\u{1b}[33;1m";
const RED: &str = "\u{1b}[31;1m";

/// Width of the size column before color codes.
const SIZE_WIDTH: usize = 9;
const COUNT_WIDTH: usize = 11;

pub struct Printer<W> {
    out: W,
    pub size_format: SizeFormat,
    pub color: bool,
    pub show_item_count: bool,
    /// Report apparent size instead of disk usage.
    pub apparent: bool,
    pub reverse: bool,
}

impl<W: Write> Printer<W> {
    pub fn new(out: W, size_format: SizeFormat, color: bool) -> Self {
        Self {
            out,
            size_format,
            color,
            show_item_count: false,
            apparent: false,
            reverse: false,
        }
    }

    fn order(&self) -> Order {
        if self.reverse {
            Order::Ascending
        } else {
            Order::Descending
        }
    }

    fn sort_by(&self) -> SortBy {
        if self.apparent {
            SortBy::ApparentSize
        } else {
            SortBy::Usage
        }
    }

    fn size_of(&self, entry: &Entry<'_>) -> i64 {
        if self.apparent {
            entry.size
        } else {
            entry.usage
        }
    }

    fn paint<'a>(&self, text: &'a str, color: &str) -> Cow<'a, str> {
        if self.color {
            Cow::Owned(format!("{color}{text}{RESET}"))
        } else {
            Cow::Borrowed(text)
        }
    }

    /// Lists `dir`'s direct children.
    pub fn dir(&mut self, dir: &Dir) -> io::Result<()> {
        for entry in dir.sorted(self.sort_by(), self.order()) {
            self.item(&entry)?;
        }
        Ok(())
    }

    fn item(&mut self, entry: &Entry<'_>) -> io::Result<()> {
        let size = format!(
            "{:>SIZE_WIDTH$}",
            format_size(self.size_of(entry), self.size_format)
        );
        let size = self.paint(&size, ORANGE);

        let escaped = escape_name(entry.name);
        let name = if entry.is_dir() {
            let directory_name = format!("/{escaped}");
            self.paint(&directory_name, BLUE).into_owned()
        } else {
            escaped
        };

        if self.show_item_count {
            let count = format!("{:>COUNT_WIDTH$}", format_count(entry.items));
            let count = self.paint(&count, RED);
            writeln!(self.out, "{} {size} {count} {name}", entry.flag.as_char())
        } else {
            writeln!(self.out, "{} {size} {name}", entry.flag.as_char())
        }
    }

    /// Prints the scanned path's total.
    pub fn summary(&mut self, dir: &Dir) -> io::Result<()> {
        let total = if self.apparent { dir.size } else { dir.usage };
        let size = format!("{:>SIZE_WIDTH$}", format_size(total, self.size_format));
        writeln!(
            self.out,
            "{} {}",
            self.paint(&size, ORANGE),
            escape_name(&dir.name)
        )
    }

    /// Lists the tree to `depth`.
    pub fn depth(&mut self, dir: &Dir, depth: u32) -> io::Result<()> {
        let root = PathBuf::from(dir.name.as_ref());
        let total = if self.apparent { dir.size } else { dir.usage };
        self.path_line(total, &root, true)?;
        self.depth_inner(dir, &root, 0, depth)
    }

    fn depth_inner(&mut self, dir: &Dir, path: &Path, current: u32, max: u32) -> io::Result<()> {
        if current >= max {
            return Ok(());
        }
        for entry in dir.sorted(self.sort_by(), self.order()) {
            let child = path.join(entry.name);
            self.path_line(self.size_of(&entry), &child, entry.is_dir())?;
            if let Some(subdir) = entry.dir {
                self.depth_inner(subdir, &child, current + 1, max)?;
            }
        }
        Ok(())
    }

    /// Lists the `n` largest files.
    pub fn top(&mut self, dir: &Dir, n: usize) -> io::Result<()> {
        let mut files = largest_files(dir, n, self.apparent);
        if self.reverse {
            files.reverse();
        }
        for (size, path) in files {
            self.path_line(size, &path, false)?;
        }
        Ok(())
    }

    fn path_line(&mut self, size: i64, path: &Path, is_dir: bool) -> io::Result<()> {
        let formatted = format!("{:>SIZE_WIDTH$}", format_size(size, self.size_format));
        let shown = escape_name(&path.to_string_lossy());
        let shown = if is_dir {
            self.paint(&shown, BLUE)
        } else {
            Cow::Owned(shown)
        };
        writeln!(self.out, "{} {shown}", self.paint(&formatted, ORANGE))
    }
}

/// Collects the `n` largest files with a bounded heap.
fn largest_files(root: &Dir, n: usize, apparent: bool) -> Vec<(i64, PathBuf)> {
    if n == 0 {
        return Vec::new();
    }

    // Keep the smallest candidate at the top for eviction.
    let mut heap: BinaryHeap<(Reverse<i64>, PathBuf)> = BinaryHeap::new();
    visit(root, Path::new(root.name.as_ref()), n, apparent, &mut heap);

    let mut collected: Vec<_> = heap
        .into_iter()
        .map(|(Reverse(size), path)| (size, path))
        .collect();
    collected.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    collected
}

fn visit(
    dir: &Dir,
    path: &Path,
    n: usize,
    apparent: bool,
    heap: &mut BinaryHeap<(Reverse<i64>, PathBuf)>,
) {
    for entry in dir.entries() {
        let child = path.join(entry.name);
        match entry.dir {
            Some(subdir) => visit(subdir, &child, n, apparent, heap),
            None => {
                let size = if apparent { entry.size } else { entry.usage };
                if heap.len() < n {
                    heap.push((Reverse(size), child));
                } else if let Some((Reverse(smallest), _)) = heap.peek()
                    && size > *smallest
                {
                    heap.pop();
                    heap.push((Reverse(size), child));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::FileStats;

    fn sample() -> Dir {
        let mut root = Dir::new("/tmp/sample");
        root.push_file("small.txt", FileStats::plain(1024, 1024, 100));

        let mut sub = Dir::new("sub");
        sub.push_file("huge.bin", FileStats::plain(1024 * 1024, 1024 * 1024, 200));
        sub.push_file("medium.bin", FileStats::plain(4096, 4096, 150));
        sub.finish(2);

        root.push_dir(sub);
        root.finish(2);
        root
    }

    fn render(f: impl FnOnce(&mut Printer<Vec<u8>>) -> io::Result<()>) -> String {
        let mut printer = Printer::new(Vec::new(), SizeFormat::Binary, false);
        f(&mut printer).unwrap();
        String::from_utf8(printer.out).unwrap()
    }

    #[test]
    fn children_list_largest_first_with_directories_marked() {
        let dir = sample();
        let out = render(|p| p.dir(&dir));
        let lines: Vec<_> = out.lines().collect();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("/sub"), "the big directory leads: {out}");
        assert!(lines[1].ends_with("small.txt"));
        assert!(lines[0].contains("1.0 MiB"));
    }

    #[test]
    fn reverse_and_item_count_change_the_rendering() {
        let dir = sample();
        let out = render(|p| {
            p.reverse = true;
            p.show_item_count = true;
            p.dir(&dir)
        });
        let lines: Vec<_> = out.lines().collect();

        assert!(lines[0].ends_with("small.txt"), "smallest first: {out}");
        assert!(lines[1].contains(" 3 "), "sub holds 3 items: {out}");
    }

    #[test]
    fn top_files_are_global_and_sorted_by_size() {
        let dir = sample();
        let out = render(|p| p.top(&dir, 2));
        let lines: Vec<_> = out.lines().collect();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("/tmp/sample/sub/huge.bin"), "{out}");
        assert!(lines[1].ends_with("/tmp/sample/sub/medium.bin"), "{out}");
    }

    #[test]
    fn top_uses_the_selected_size_and_honours_reverse_order() {
        let mut dir = Dir::new("/tmp/sample");
        dir.push_file("sparse", FileStats::plain(1024 * 1024, 0, 0));
        dir.push_file("allocated", FileStats::plain(4096, 8192, 0));

        let usage = render(|p| p.top(&dir, 2));
        let usage_lines: Vec<_> = usage.lines().collect();
        assert!(usage_lines[0].ends_with("/tmp/sample/allocated"), "{usage}");
        assert!(usage_lines[0].contains("8.0 KiB"), "{usage}");

        let apparent = render(|p| {
            p.apparent = true;
            p.reverse = true;
            p.top(&dir, 2)
        });
        let apparent_lines: Vec<_> = apparent.lines().collect();
        assert!(
            apparent_lines[0].ends_with("/tmp/sample/allocated"),
            "{apparent}"
        );
        assert!(
            apparent_lines[1].ends_with("/tmp/sample/sparse"),
            "{apparent}"
        );
        assert!(apparent_lines[1].contains("1.0 MiB"), "{apparent}");
    }

    #[test]
    fn top_zero_prints_nothing_and_top_beyond_the_tree_is_clamped() {
        let dir = sample();
        assert_eq!(render(|p| p.top(&dir, 0)), "");
        assert_eq!(render(|p| p.top(&dir, 99)).lines().count(), 3);

        let empty = Dir::new("/tmp/empty");
        assert_eq!(render(|p| p.top(&empty, usize::MAX)), "");
    }

    #[test]
    fn depth_limits_recursion_but_always_shows_the_root() {
        let dir = sample();
        let shallow = render(|p| p.depth(&dir, 1));
        assert_eq!(
            shallow.lines().count(),
            3,
            "root, sub, small.txt: {shallow}"
        );
        assert!(!shallow.contains("huge.bin"));

        let deep = render(|p| p.depth(&dir, 2));
        assert!(deep.contains("/tmp/sample/sub/huge.bin"), "{deep}");
    }

    #[test]
    fn summary_prints_one_total_line() {
        let dir = sample();
        let out = render(|p| p.summary(&dir));
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("/tmp/sample"));
    }

    #[test]
    fn colour_wraps_the_padded_column_so_alignment_is_unaffected() {
        let dir = sample();
        let mut printer = Printer::new(Vec::new(), SizeFormat::Binary, true);
        printer.dir(&dir).unwrap();
        let out = String::from_utf8(printer.out).unwrap();

        assert!(out.contains(BLUE), "directories are coloured");
        assert!(out.contains(ORANGE), "sizes are coloured");
        assert!(
            out.contains(&format!("{ORANGE}  1.0 MiB{RESET}")),
            "{out:?}"
        );
    }

    #[test]
    fn names_are_visibly_escaped_in_all_text_presentations() {
        let mut dir = Dir::new("/tmp/root\n\u{202e}");
        dir.push_file("bad\u{1b}\r\u{7f}.txt", FileStats::plain(1, 1, 0));

        let listing = render(|p| p.dir(&dir));
        assert!(listing.contains(r"bad\x1b\r\x7f.txt"), "{listing:?}");
        assert!(!listing.contains('\u{1b}'));

        let summary = render(|p| p.summary(&dir));
        assert!(summary.contains(r"/tmp/root\n\u{202e}"), "{summary:?}");

        let depth = render(|p| p.depth(&dir, 1));
        assert!(depth.contains(r"bad\x1b\r\x7f.txt"), "{depth:?}");
    }
}
