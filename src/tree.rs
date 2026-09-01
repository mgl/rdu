//! The scanned directory tree and its accumulated statistics.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

/// Apparent size charged to a directory with no counted children.
pub const EMPTY_DIR_SIZE: i64 = 512;

/// Per-item status shown in the leftmost output column.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Flag {
    /// No special status.
    #[default]
    Ok,
    /// Directory could not be read.
    Unreadable,
    /// A descendant could not be read.
    ChildUnreadable,
    /// An empty directory.
    Empty,
    /// Not a regular file.
    NotRegular,
    /// Inode was already counted elsewhere.
    HardLink,
}

impl Flag {
    /// Returns rdu's display character.
    pub fn as_char(self) -> char {
        match self {
            Self::Ok => ' ',
            Self::Unreadable => '!',
            Self::ChildUnreadable => '.',
            Self::Empty => 'e',
            Self::NotRegular => '@',
            Self::HardLink => 'H',
        }
    }
}

/// The measured facts about one file, as handed to its directory.
#[derive(Clone, Copy, Debug)]
pub struct FileStats {
    pub size: i64,
    pub usage: i64,
    pub mtime: i64,
    pub flag: Flag,
    /// Whether these bytes add to the parent's totals.
    pub counted: bool,
}

impl FileStats {
    /// An ordinary file whose bytes count in full.
    pub fn plain(size: i64, usage: i64, mtime: i64) -> Self {
        Self {
            size,
            usage,
            mtime,
            flag: Flag::Ok,
            counted: true,
        }
    }

    pub fn with_flag(self, flag: Flag) -> Self {
        Self { flag, ..self }
    }
}

/// Summary passed upward when a child is not stored.
#[derive(Clone, Copy, Debug, Default)]
pub struct Summary {
    pub size: i64,
    pub usage: i64,
    pub items: i64,
    pub mtime: i64,
    pub flag: Flag,
}

/// A non-directory entry. `name_*` index into the owning [`Dir`]'s name pool.
#[derive(Clone, Debug)]
struct FileRecord {
    size: i64,
    usage: i64,
    mtime: i64,
    name_start: u32,
    name_len: u32,
    flag: Flag,
    raw_start: u32,
    raw_len: u32,
}

/// A directory and everything counted beneath it.
#[derive(Clone, Debug, Default)]
pub struct Dir {
    /// Directory name, or the root path.
    pub name: Box<str>,
    /// Apparent size of the subtree.
    pub size: i64,
    /// Disk usage of the subtree.
    pub usage: i64,
    /// Latest subtree mtime in Unix seconds.
    pub mtime: i64,
    /// Number of items, including this directory.
    pub items: i64,
    pub flag: Flag,

    /// Exact filesystem name; `name` is for display.
    fs_name: Box<OsStr>,

    /// Display and raw file names in one allocation.
    names: Vec<u8>,
    files: Vec<FileRecord>,
    /// Child directories.
    pub dirs: Vec<Dir>,
}

/// Uniform view of a file or directory child.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub name: &'a str,
    /// Exact filesystem name.
    pub fs_name: &'a OsStr,
    pub size: i64,
    pub usage: i64,
    pub mtime: i64,
    pub items: i64,
    pub flag: Flag,
    /// Index in `Dir::dirs` when this is a directory.
    pub dir_index: Option<usize>,
    /// Child directory, when present.
    pub dir: Option<&'a Dir>,
}

impl Entry<'_> {
    pub fn is_dir(&self) -> bool {
        self.dir.is_some()
    }
}

/// Which field to order children by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortBy {
    #[default]
    Usage,
    ApparentSize,
    Name,
    ItemCount,
    Mtime,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    Ascending,
    #[default]
    Descending,
}

impl Dir {
    pub fn new(name: impl AsRef<OsStr>) -> Self {
        let fs_name: OsString = name.as_ref().to_owned();
        Self {
            name: fs_name.to_string_lossy().into_owned().into_boxed_str(),
            fs_name: fs_name.into_boxed_os_str(),
            items: 1,
            ..Default::default()
        }
    }

    /// Creates a directory that only accumulates statistics.
    pub fn aggregate() -> Self {
        Self {
            items: 1,
            ..Default::default()
        }
    }

    pub fn fs_name(&self) -> &OsStr {
        &self.fs_name
    }

    /// Adds file statistics without storing the file.
    pub fn account_file(&mut self, stats: FileStats) {
        self.items += 1;
        self.mtime = self.mtime.max(stats.mtime);
        if stats.counted {
            self.size += stats.size;
            self.usage += stats.usage;
        }
    }

    /// Folds a file in and keeps it for display.
    pub fn push_file(&mut self, name: impl AsRef<OsStr>, stats: FileStats) {
        self.account_file(stats);

        let fs_name = name.as_ref();
        let display_name = fs_name.to_string_lossy();
        let start = self.names.len() as u32;
        self.names.extend_from_slice(display_name.as_bytes());
        let raw_start = self.names.len() as u32;
        self.names.extend_from_slice(fs_name.as_bytes());
        self.files.push(FileRecord {
            size: stats.size,
            usage: stats.usage,
            mtime: stats.mtime,
            name_start: start,
            name_len: display_name.len() as u32,
            flag: stats.flag,
            raw_start,
            raw_len: fs_name.len() as u32,
        });
    }

    /// This directory reduced to what an ancestor needs from it.
    pub fn summary(&self) -> Summary {
        Summary {
            size: self.size,
            usage: self.usage,
            items: self.items,
            mtime: self.mtime,
            flag: self.flag,
        }
    }

    /// Folds a finished child's statistics in without storing it.
    pub fn account(&mut self, child: &Summary) {
        self.size += child.size;
        self.usage += child.usage;
        self.items += child.items;
        self.mtime = self.mtime.max(child.mtime);

        // Preserve an unreadable directory's stronger status.
        if matches!(child.flag, Flag::Unreadable | Flag::ChildUnreadable)
            && self.flag != Flag::Unreadable
        {
            self.flag = Flag::ChildUnreadable;
        }
    }

    /// Folds a finished child directory's statistics in without storing it.
    pub fn account_dir(&mut self, child: &Dir) {
        self.account(&child.summary());
    }

    /// Folds a finished child directory in and keeps it for display.
    pub fn push_dir(&mut self, child: Dir) {
        self.account_dir(&child);
        self.dirs.push(child);
    }

    /// Applies the empty-directory rule after adding all children.
    pub fn finish(&mut self, child_count: usize) {
        let counted_anything = self.items > 1;
        if child_count == 0 || !counted_anything {
            self.items = 1;
            self.size += EMPTY_DIR_SIZE;
            self.usage = 0;
            if child_count == 0 && self.flag == Flag::Ok {
                self.flag = Flag::Empty;
            }
        }
    }

    /// Iterates files followed by subdirectories.
    pub fn entries(&self) -> impl Iterator<Item = Entry<'_>> {
        let files = self.files.iter().map(|f| {
            let start = f.name_start as usize;
            Entry {
                name: std::str::from_utf8(&self.names[start..start + f.name_len as usize])
                    .expect("lossy display names are valid UTF-8"),
                fs_name: OsStr::from_bytes(
                    &self.names[f.raw_start as usize..f.raw_start as usize + f.raw_len as usize],
                ),
                size: f.size,
                usage: f.usage,
                mtime: f.mtime,
                items: 1,
                flag: f.flag,
                dir_index: None,
                dir: None,
            }
        });
        let dirs = self.dirs.iter().enumerate().map(|(dir_index, d)| Entry {
            name: &d.name,
            fs_name: d.fs_name(),
            size: d.size,
            usage: d.usage,
            mtime: d.mtime,
            items: d.items,
            flag: d.flag,
            dir_index: Some(dir_index),
            dir: Some(d),
        });
        files.chain(dirs)
    }

    /// Children in display order.
    pub fn sorted(&self, by: SortBy, order: Order) -> Vec<Entry<'_>> {
        let mut entries: Vec<_> = self.entries().collect();
        entries.sort_by(|a, b| {
            let ordering = compare(a, b, by);
            match order {
                Order::Ascending => ordering,
                Order::Descending => ordering.reverse(),
            }
        });
        entries
    }
}

/// Compares entries by field, then by natural name order.
fn compare(a: &Entry<'_>, b: &Entry<'_>, by: SortBy) -> Ordering {
    let primary = match by {
        SortBy::Usage => a.usage.cmp(&b.usage),
        SortBy::ApparentSize => a.size.cmp(&b.size),
        SortBy::ItemCount => a.items.cmp(&b.items),
        SortBy::Mtime => a.mtime.cmp(&b.mtime),
        SortBy::Name => Ordering::Equal,
    };
    primary.then_with(|| natord::compare(a.name, b.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with_two_files() -> Dir {
        let mut dir = Dir::new("root");
        dir.push_file("b.txt", FileStats::plain(100, 4096, 20));
        dir.push_file("a.txt", FileStats::plain(300, 8192, 10));
        dir
    }

    #[test]
    fn stats_accumulate_as_children_are_added() {
        let dir = dir_with_two_files();
        assert_eq!(dir.size, 400);
        assert_eq!(dir.usage, 12288);
        assert_eq!(dir.items, 3, "two files plus the directory itself");
        assert_eq!(dir.mtime, 20, "the newest child mtime wins");
    }

    #[test]
    fn names_survive_interning() {
        let dir = dir_with_two_files();
        let names: Vec<_> = dir.entries().map(|e| e.name).collect();
        assert_eq!(names, ["b.txt", "a.txt"]);
    }

    #[test]
    fn empty_directory_gets_the_sentinel_size() {
        let mut dir = Dir::new("empty");
        dir.finish(0);
        assert_eq!(dir.size, EMPTY_DIR_SIZE);
        assert_eq!(dir.usage, 0);
        assert_eq!(dir.items, 1);
        assert_eq!(dir.flag, Flag::Empty);
    }

    #[test]
    fn accepted_empty_child_does_not_empty_its_parent_while_filtering() {
        let mut dir = Dir::new("filtered");
        let mut child = Dir::new("sub");
        child.finish(0);
        dir.push_dir(child);

        dir.finish(1);
        assert_eq!(dir.items, 2);
        assert_eq!(dir.usage, 0);
        assert_eq!(dir.dirs.len(), 1);
    }

    #[test]
    fn raw_names_remain_distinct_when_their_display_names_collide() {
        use std::os::unix::ffi::OsStrExt;

        let mut dir = Dir::new("root");
        dir.push_file(OsStr::from_bytes(b"bad\xff"), FileStats::plain(1, 1, 0));
        dir.push_file(OsStr::from_bytes(b"bad\xfe"), FileStats::plain(1, 1, 0));

        let entries: Vec<_> = dir.entries().collect();
        assert_eq!(entries[0].name, entries[1].name);
        assert_ne!(entries[0].fs_name.as_bytes(), entries[1].fs_name.as_bytes());
    }

    #[test]
    fn unreadable_children_taint_ancestors_without_overriding_a_hard_error() {
        let mut parent = Dir::new("parent");
        let mut broken = Dir::new("broken");
        broken.flag = Flag::Unreadable;
        parent.push_dir(broken);
        assert_eq!(parent.flag, Flag::ChildUnreadable);

        let mut worse = Dir::new("worse");
        worse.flag = Flag::Unreadable;
        let mut child = Dir::new("child");
        child.flag = Flag::ChildUnreadable;
        worse.push_dir(child);
        assert_eq!(
            worse.flag,
            Flag::Unreadable,
            "a hard error is not downgraded"
        );
    }

    #[test]
    fn sorting_falls_back_to_natural_name_order() {
        let mut dir = Dir::new("root");
        dir.push_file("file10", FileStats::plain(100, 0, 0));
        dir.push_file("file9", FileStats::plain(100, 0, 0));
        dir.push_file("big", FileStats::plain(500, 0, 0));

        let by_size = dir.sorted(SortBy::ApparentSize, Order::Descending);
        assert_eq!(by_size[0].name, "big");
        assert_eq!(by_size[1].name, "file10");
        assert_eq!(by_size[2].name, "file9");

        let by_name = dir.sorted(SortBy::Name, Order::Ascending);
        assert_eq!(
            by_name.iter().map(|e| e.name).collect::<Vec<_>>(),
            ["big", "file9", "file10"]
        );
    }
}
