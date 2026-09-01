//! Parallel filesystem traversal using directory-relative `statx` calls,
//! work stealing, and continuation-based completion.

mod pool;

use std::cell::{RefCell, UnsafeCell};

use std::ffi::{CStr, OsStr};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashSet;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, Statx, StatxFlags};
use smallvec::SmallVec;

use crate::filter::Filters;
use crate::tree::{Dir, FileStats, Flag, Summary};

use pool::Spawner;

/// `stat(2)` reports disk usage in 512-byte units.
const BLOCK_SIZE: i64 = 512;

/// Fields requested from `statx`.
const STATX_MASK: StatxFlags = StatxFlags::SIZE
    .union(StatxFlags::BLOCKS)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::NLINK)
    .union(StatxFlags::INO)
    .union(StatxFlags::TYPE);

/// Fields needed for directory traversal.
const DIR_STATX_MASK: StatxFlags = StatxFlags::MTIME.union(StatxFlags::INO);

/// Entry count above which `statx` work is split.
const STAT_PARALLEL_THRESHOLD: usize = 128;

/// Entries per parallel `statx` task.
const STAT_CHUNK: usize = 512;

/// Buffer size for `getdents64`.
const DENTS_BUFFER: usize = 32 * 1024;

thread_local! {
    /// Per-worker `getdents64` buffer.
    static DENTS: RefCell<Box<[MaybeUninit<u8>]>> =
        RefCell::new(Box::new([MaybeUninit::uninit(); DENTS_BUFFER]));
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Maximum depth at which entries are stored.
    pub depth_cap: Option<u32>,
    /// Report the size of a symlink's target rather than of the link itself.
    pub follow_symlinks: bool,
    /// Stay on the filesystem the scan started on.
    pub one_filesystem: bool,
}

/// Live counters for the progress line, updated by every worker.
#[derive(Debug)]
pub struct Progress {
    pub items: AtomicI64,
    pub usage: AtomicI64,
    current: Mutex<PathBuf>,
    enabled: bool,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        Self {
            items: AtomicI64::default(),
            usage: AtomicI64::default(),
            current: Mutex::default(),
            enabled,
        }
    }

    pub fn items(&self) -> i64 {
        self.items.load(Ordering::Relaxed)
    }

    pub fn usage(&self) -> i64 {
        self.usage.load(Ordering::Relaxed)
    }

    pub fn current_dir(&self) -> PathBuf {
        self.current.lock().expect("progress mutex").clone()
    }

    fn enter(&self, path: &Path) {
        if !self.enabled {
            return;
        }
        *self.current.lock().expect("progress mutex") = path.to_path_buf();
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn reset(&self) {
        self.items.store(0, Ordering::Relaxed);
        self.usage.store(0, Ordering::Relaxed);
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new(false)
    }
}

/// Sharded set of device/inode pairs shared between workers.
#[derive(Debug)]
struct InodeSet {
    shards: Vec<Mutex<FxHashSet<(u64, u64)>>>,
}

impl InodeSet {
    const SHARDS: usize = 64;

    fn new() -> Self {
        Self {
            shards: (0..Self::SHARDS).map(|_| Mutex::default()).collect(),
        }
    }

    fn with(device: u64, inode: u64) -> Self {
        let set = Self::new();
        set.insert(device, inode);
        set
    }

    /// Records the inode and reports whether it was newly inserted.
    fn insert(&self, device: u64, inode: u64) -> bool {
        let shard = &self.shards[(device ^ inode) as usize % Self::SHARDS];
        shard.lock().expect("inode shard").insert((device, inode))
    }
}

/// Inline storage for listing indices.
type Entries = SmallVec<[u32; 16]>;

/// Entry returned by `getdents64`, before `stat`.
struct RawEntry {
    name_start: u32,
    name_len: u16,
    kind: FileType,
}

#[derive(Clone, Copy)]
struct FileMeta {
    stats: FileStats,
    device: u64,
    inode: u64,
    deduplicate: bool,
}

enum ResolvedEntry {
    File(FileMeta),
    Directory,
    ChildUnreadable,
    Skip,
}

/// A directory's entries, with every name in a single allocation.
#[derive(Default)]
struct Listing {
    names: SmallVec<[u8; 64]>,
    entries: SmallVec<[RawEntry; 8]>,
}

impl Listing {
    /// Stores a name as Unix bytes followed by NUL.
    fn push(&mut self, name: &[u8], kind: FileType) {
        let start = self.names.len() as u32;
        self.names.extend_from_slice(name);
        self.names.push(0);
        self.entries.push(RawEntry {
            name_start: start,
            name_len: u16::try_from(name.len()).expect("directory entry name fits in u16"),
            kind,
        });
    }

    fn name(&self, entry: &RawEntry) -> &OsStr {
        let start = entry.name_start as usize;
        OsStr::from_bytes(&self.names[start..start + usize::from(entry.name_len)])
    }

    fn c_name(&self, entry: &RawEntry) -> &CStr {
        let start = entry.name_start as usize;
        // SAFETY: `push` appends one NUL after the kernel-provided name.
        unsafe {
            CStr::from_bytes_with_nul_unchecked(
                &self.names[start..=start + usize::from(entry.name_len)],
            )
        }
    }

    fn at(&self, index: u32) -> &RawEntry {
        &self.entries[index as usize]
    }
}

/// Result returned by a completed task.
enum Outcome {
    /// Completed child directory.
    Subtree(Box<Dir>),
    /// Totals for an unstored child or chunk.
    Totals { totals: Summary, unreadable: bool },
    /// Files resolved by a chunk.
    Files {
        /// Listing indices and statistics.
        files: Vec<(u32, FileStats)>,
        unreadable: bool,
    },
}

impl Outcome {
    /// Empty result for a skipped entry.
    fn nothing() -> Self {
        Self::Totals {
            totals: Summary::default(),
            unreadable: false,
        }
    }
}

/// Directory state shared by its child tasks.
struct Node {
    parent: Option<Arc<Node>>,
    /// Index in the parent's result slots.
    slot: usize,
    depth: u32,
    /// Whether entries are stored for display.
    store: bool,
    /// Open descriptor used by child tasks.
    dirfd: OwnedFd,
    listing: Box<Listing>,
    /// Path used by progress or path filters.
    path: Option<PathBuf>,
    /// Entries requiring `statx`.
    candidates: Entries,
    /// Whether entries can be counted without an intermediate list.
    aggregate_only: bool,
    /// On-disk entry count for the empty-directory rule.
    child_count: usize,
    /// Directory metadata and inline files.
    seed: UnsafeCell<Dir>,
    pending: AtomicUsize,
    results: Box<[UnsafeCell<Option<Outcome>>]>,
}

// SAFETY: The creator writes `seed` before publishing child tasks. Each result
// slot is written once; `pending` reaching zero synchronizes those writes.
unsafe impl Sync for Node {}

/// Work item queued on the pool.
enum Task {
    /// Walk the scan root.
    Root(Box<RootStart>),
    /// Open and walk a known directory.
    Dir {
        parent: Arc<Node>,
        entry: u32,
        slot: u32,
    },
    /// Resolve an entry with unknown type.
    Untyped {
        parent: Arc<Node>,
        entry: u32,
        slot: u32,
    },
    /// `statx` a range of entries.
    Chunk {
        node: Arc<Node>,
        start: u32,
        end: u32,
        slot: u32,
    },
}

struct RootStart {
    dirfd: OwnedFd,
    seed: Dir,
    path: Option<PathBuf>,
}

/// Scan state shared by all workers.
struct Walk<'a> {
    filters: &'a Filters,
    options: Options,
    progress: &'a Progress,
    links: InodeSet,
    duplicate_links: AtomicBool,
    visited_dirs: InodeSet,
    root_device: u64,
    parallel: bool,
    root: Mutex<Option<Dir>>,
}

/// Walks `root` and returns its completed tree.
pub fn run(
    root: &Path,
    filters: &Filters,
    options: Options,
    progress: &Progress,
    workers: usize,
) -> io::Result<Dir> {
    let (dir, duplicate_links) = run_once(root, filters, options, progress, workers)?;
    if workers > 1 && duplicate_links {
        progress.reset();
        return run_once(root, filters, options, progress, 1).map(|(dir, _)| dir);
    }
    Ok(dir)
}

fn run_once(
    root: &Path,
    filters: &Filters,
    options: Options,
    progress: &Progress,
    workers: usize,
) -> io::Result<(Dir, bool)> {
    let dirfd = open_dir(rustix::fs::CWD, root, options.follow_symlinks)?;
    let stat = rustix::fs::statx(
        &dirfd,
        "",
        AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
        DIR_STATX_MASK,
    )?;

    let walk = Walk {
        filters,
        options,
        progress,
        links: InodeSet::new(),
        duplicate_links: AtomicBool::new(false),
        visited_dirs: InodeSet::with(device_of(&stat), stat.stx_ino),
        root_device: device_of(&stat),
        parallel: workers > 1,
        root: Mutex::new(None),
    };

    let mut seed = Dir::new(root.as_os_str());
    seed.mtime = stat.stx_mtime.tv_sec;

    let start = Task::Root(Box::new(RootStart {
        dirfd,
        seed,
        path: Some(root.to_path_buf()),
    }));
    pool::run(workers.max(1), start, |task, spawner| {
        walk.execute(task, spawner)
    });

    let dir = walk.root.lock().expect("root result").take().expect(
        "the walk always publishes a root: every task either delivers into a \
         parent slot or, for the root, into this cell",
    );
    Ok((dir, walk.duplicate_links.load(Ordering::Relaxed)))
}

impl Walk<'_> {
    fn execute(&self, task: Task, spawner: &Spawner<'_, Task>) {
        match task {
            Task::Root(start) => {
                self.open_node(start.dirfd, start.seed, start.path, 0, None, 0, spawner);
            }
            Task::Dir {
                parent,
                entry,
                slot,
            } => self.enter_dir(&parent, entry, slot as usize, spawner),
            Task::Untyped {
                parent,
                entry,
                slot,
            } => self.enter_untyped(&parent, entry, slot as usize, spawner),
            Task::Chunk {
                node,
                start,
                end,
                slot,
            } => {
                let outcome = self.stat_chunk(&node, start, end);
                self.deliver(node, slot as usize, outcome);
            }
        }
    }

    fn require_file_deterministic_retry(&self) {
        if self.options.depth_cap != Some(0) && !self.duplicate_links.load(Ordering::Relaxed) {
            self.duplicate_links.store(true, Ordering::Relaxed);
        }
    }

    /// Opens a known subdirectory and turns it into a node.
    fn enter_dir(&self, parent: &Arc<Node>, index: u32, slot: usize, spawner: &Spawner<'_, Task>) {
        let entry = parent.listing.at(index);
        let name = parent.listing.name(entry);
        let child_path = parent
            .path
            .as_deref()
            .filter(|_| self.progress.enabled() || self.filters.needs_path())
            .map(|path| path.join(name));

        if self.filters.has_dir_filter() {
            let filter_path = child_path.as_deref().or(parent.path.as_deref());
            if let Some(path) = filter_path
                && self.filters.skip_dir(name, path)
            {
                return self.absent(parent, slot);
            }
        }

        let named = parent.store;
        let child_fd = match open_dir(parent.dirfd.as_fd(), parent.listing.c_name(entry), false) {
            Ok(fd) => fd,
            Err(err) if err.raw_os_error() == Some(rustix::io::Errno::NOENT.raw_os_error()) => {
                return self.absent(parent, slot);
            }
            Err(_) => return self.unreadable(parent, slot, name, named),
        };

        let stat = match rustix::fs::statx(
            &child_fd,
            "",
            AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC,
            DIR_STATX_MASK,
        ) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return self.absent(parent, slot),
            Err(_) => return self.unreadable(parent, slot, name, named),
        };
        if self.options.one_filesystem && device_of(&stat) != self.root_device {
            return self.absent(parent, slot);
        }
        if !self.visited_dirs.insert(device_of(&stat), stat.stx_ino) {
            if !self.duplicate_links.load(Ordering::Relaxed) {
                self.duplicate_links.store(true, Ordering::Relaxed);
            }
            return self.absent(parent, slot);
        }

        let mut seed = if named {
            Dir::new(name)
        } else {
            Dir::aggregate()
        };
        seed.mtime = stat.stx_mtime.tv_sec;
        self.open_node(
            child_fd,
            seed,
            child_path,
            parent.depth + 1,
            Some(parent.clone()),
            slot,
            spawner,
        );
    }

    /// Resolves an entry whose type was unknown.
    fn enter_untyped(
        &self,
        parent: &Arc<Node>,
        index: u32,
        slot: usize,
        spawner: &Spawner<'_, Task>,
    ) {
        let entry = parent.listing.at(index);
        match self.resolve_candidate(&parent.dirfd, &parent.listing, entry) {
            ResolvedEntry::Directory => self.enter_dir(parent, index, slot, spawner),
            ResolvedEntry::File(meta) => {
                let stats = self.finish_file(meta);
                self.record_progress(stats);
                let outcome = if parent.store {
                    Outcome::Files {
                        files: vec![(index, stats)],
                        unreadable: false,
                    }
                } else {
                    let mut totals = Dir::default();
                    totals.account_file(stats);
                    Outcome::Totals {
                        totals: totals.summary(),
                        unreadable: false,
                    }
                };
                self.deliver(parent.clone(), slot, outcome);
            }
            ResolvedEntry::ChildUnreadable => self.deliver(
                parent.clone(),
                slot,
                Outcome::Totals {
                    totals: Summary::default(),
                    unreadable: true,
                },
            ),
            ResolvedEntry::Skip => self.absent(parent, slot),
        }
    }

    /// Reads a directory, publishes its work, and returns without waiting.
    #[allow(clippy::too_many_arguments)]
    fn open_node(
        &self,
        dirfd: OwnedFd,
        mut seed: Dir,
        path: Option<PathBuf>,
        depth: u32,
        parent: Option<Arc<Node>>,
        slot: usize,
        spawner: &Spawner<'_, Task>,
    ) {
        if let Some(path) = path.as_deref() {
            self.progress.enter(path);
        }

        let Ok(listing) = read_listing(&dirfd) else {
            seed.flag = Flag::Unreadable;
            seed.finish(0);
            return self.publish(parent, slot, seed);
        };

        let store = self.stores(depth);
        let has_file_filter = self.filters.has_file_filter();
        let aggregate_only = !store && !has_file_filter && self.options.depth_cap == Some(0);
        let child_count = listing.entries.len();

        let mut subdirs: Entries = SmallVec::new();
        let mut untyped: Entries = SmallVec::new();
        let mut candidates: Entries = SmallVec::new();
        for (index, entry) in listing.entries.iter().enumerate() {
            let index = index as u32;
            match entry.kind {
                FileType::Directory => subdirs.push(index),
                FileType::Unknown => untyped.push(index),
                _ if aggregate_only => {}
                _ => {
                    if !has_file_filter
                        || !self
                            .filters
                            .skip_file(&listing.name(entry).to_string_lossy())
                    {
                        candidates.push(index);
                    }
                }
            }
        }
        if !self.parallel && self.options.depth_cap != Some(0) {
            let by_name = |a: &u32, b: &u32| {
                listing
                    .at(*a)
                    .name_bytes(&listing)
                    .cmp(listing.at(*b).name_bytes(&listing))
            };
            candidates.sort_unstable_by(by_name);
            subdirs.sort_unstable_by(by_name);
        }

        let inline_count = if aggregate_only {
            child_count - subdirs.len() - untyped.len()
        } else {
            candidates.len()
        };
        let chunked = self.parallel && inline_count >= STAT_PARALLEL_THRESHOLD;
        let chunk_span = if aggregate_only {
            child_count
        } else {
            candidates.len()
        };
        let chunks = if chunked {
            chunk_span.div_ceil(STAT_CHUNK)
        } else {
            0
        };

        let total = subdirs.len() + untyped.len() + chunks;
        if total == 0 {
            self.stat_inline(
                &dirfd,
                &listing,
                &candidates,
                aggregate_only,
                store,
                &mut seed,
            );
            if self.progress.enabled() {
                self.progress
                    .items
                    .fetch_add(child_count as i64, Ordering::Relaxed);
            }
            seed.finish(child_count);
            return self.publish(parent, slot, seed);
        }

        let node = Arc::new(Node {
            parent,
            slot,
            depth,
            store,
            dirfd,
            listing,
            path,
            candidates,
            aggregate_only,
            child_count,
            seed: UnsafeCell::new(seed),
            pending: AtomicUsize::new(total),
            results: (0..total).map(|_| UnsafeCell::new(None)).collect(),
        });

        if !chunked {
            // SAFETY: no task references this node yet.
            let dir = unsafe { &mut *node.seed.get() };
            self.stat_inline(
                &node.dirfd,
                &node.listing,
                &node.candidates,
                aggregate_only,
                store,
                dir,
            );
        }

        let chunk_base = subdirs.len() + untyped.len();
        for (slot, &entry) in subdirs.iter().enumerate().rev() {
            spawner.push(Task::Dir {
                parent: node.clone(),
                entry,
                slot: slot as u32,
            });
        }
        for (offset, &entry) in untyped.iter().enumerate().rev() {
            spawner.push(Task::Untyped {
                parent: node.clone(),
                entry,
                slot: (subdirs.len() + offset) as u32,
            });
        }
        for chunk in (0..chunks).rev() {
            let start = chunk * STAT_CHUNK;
            spawner.push(Task::Chunk {
                node: node.clone(),
                start: start as u32,
                end: (start + STAT_CHUNK).min(chunk_span) as u32,
                slot: (chunk_base + chunk) as u32,
            });
        }
    }

    /// Stats entries kept by this worker.
    fn stat_inline(
        &self,
        dirfd: &OwnedFd,
        listing: &Listing,
        candidates: &[u32],
        aggregate_only: bool,
        store: bool,
        dir: &mut Dir,
    ) {
        if aggregate_only {
            for entry in &listing.entries {
                if entry.kind != FileType::Directory && entry.kind != FileType::Unknown {
                    self.apply(dirfd, listing, store, dir, entry, None);
                }
            }
        } else {
            for &index in candidates {
                self.apply(dirfd, listing, store, dir, listing.at(index), Some(index));
            }
        }
    }

    /// Resolves one entry.
    fn apply(
        &self,
        dirfd: &OwnedFd,
        listing: &Listing,
        store: bool,
        dir: &mut Dir,
        entry: &RawEntry,
        index: Option<u32>,
    ) {
        match self.resolve_candidate(dirfd, listing, entry) {
            ResolvedEntry::File(meta) => {
                let stats = self.finish_file(meta);
                self.record_progress(stats);
                match index.filter(|_| store) {
                    Some(index) => dir.push_file(listing.name(listing.at(index)), stats),
                    None => dir.account_file(stats),
                }
            }
            ResolvedEntry::Directory | ResolvedEntry::Skip => {}
            ResolvedEntry::ChildUnreadable => {
                if dir.flag != Flag::Unreadable {
                    dir.flag = Flag::ChildUnreadable;
                }
            }
        }
    }

    /// Stats one entry chunk.
    fn stat_chunk(&self, node: &Node, start: u32, end: u32) -> Outcome {
        let mut files = Vec::new();
        let mut totals = Dir::default();
        let mut unreadable = false;
        if node.store {
            files.reserve((end - start) as usize);
        }

        let mut resolve = |index: u32, entry: &RawEntry| match self.resolve_candidate(
            &node.dirfd,
            &node.listing,
            entry,
        ) {
            ResolvedEntry::File(meta) => {
                let stats = self.finish_file(meta);
                self.record_progress(stats);
                if node.store {
                    files.push((index, stats));
                } else {
                    totals.account_file(stats);
                }
            }
            ResolvedEntry::Directory | ResolvedEntry::Skip => {}
            ResolvedEntry::ChildUnreadable => unreadable = true,
        };

        if node.aggregate_only {
            for entry in &node.listing.entries[start as usize..end as usize] {
                if entry.kind != FileType::Directory && entry.kind != FileType::Unknown {
                    resolve(0, entry);
                }
            }
        } else {
            for &index in &node.candidates[start as usize..end as usize] {
                resolve(index, node.listing.at(index));
            }
        }

        if node.store {
            Outcome::Files { files, unreadable }
        } else {
            Outcome::Totals {
                totals: totals.summary(),
                unreadable,
            }
        }
    }

    /// Reports an empty result slot.
    fn absent(&self, parent: &Arc<Node>, slot: usize) {
        self.deliver(parent.clone(), slot, Outcome::nothing());
    }

    /// Reports an unreadable subdirectory.
    fn unreadable(&self, parent: &Arc<Node>, slot: usize, name: &OsStr, named: bool) {
        let mut stub = if named {
            Dir::new(name)
        } else {
            Dir::aggregate()
        };
        stub.flag = Flag::Unreadable;
        stub.finish(0);
        let outcome = Self::hand_up(stub, parent.store);
        self.deliver(parent.clone(), slot, outcome);
    }

    /// Converts a finished directory to its parent result.
    fn hand_up(dir: Dir, stored: bool) -> Outcome {
        if stored {
            Outcome::Subtree(Box::new(dir))
        } else {
            Outcome::Totals {
                totals: dir.summary(),
                unreadable: false,
            }
        }
    }

    /// Publishes a finished directory.
    fn publish(&self, parent: Option<Arc<Node>>, slot: usize, dir: Dir) {
        match parent {
            Some(parent) => {
                let outcome = Self::hand_up(dir, parent.store);
                self.deliver(parent, slot, outcome);
            }
            None => *self.root.lock().expect("root result") = Some(dir),
        }
    }

    /// Fills a result slot and completes ancestors when possible.
    fn deliver(&self, node: Arc<Node>, slot: usize, outcome: Outcome) {
        // SAFETY: this task owns `slot`; readers wait for `pending` to reach 0.
        unsafe { *node.results[slot].get() = Some(outcome) };
        if node.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finish(node);
        }
    }

    /// Folds a completed directory up the tree.
    fn finish(&self, node: Arc<Node>) {
        let mut node = node;
        loop {
            // SAFETY: `pending` is zero, so child writes are complete.
            let mut dir = unsafe { std::mem::take(&mut *node.seed.get()) };
            let mut unreadable = false;
            for cell in &node.results {
                match unsafe { (*cell.get()).take() } {
                    None => {}
                    Some(Outcome::Subtree(child)) => dir.push_dir(*child),
                    Some(Outcome::Totals {
                        totals,
                        unreadable: bad,
                    }) => {
                        unreadable |= bad;
                        dir.account(&totals);
                    }
                    Some(Outcome::Files {
                        files,
                        unreadable: bad,
                    }) => {
                        unreadable |= bad;
                        for (index, stats) in files {
                            dir.push_file(node.listing.name(node.listing.at(index)), stats);
                        }
                    }
                }
            }
            if unreadable && dir.flag != Flag::Unreadable {
                dir.flag = Flag::ChildUnreadable;
            }
            if self.progress.enabled() {
                self.progress
                    .items
                    .fetch_add(node.child_count as i64, Ordering::Relaxed);
            }
            dir.finish(node.child_count);

            let Some(parent) = node.parent.clone() else {
                *self.root.lock().expect("root result") = Some(dir);
                return;
            };
            let slot = node.slot;
            drop(node);

            let outcome = Self::hand_up(dir, parent.store);
            // SAFETY: this task owns the parent's slot.
            unsafe { *parent.results[slot].get() = Some(outcome) };
            if parent.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            node = parent;
        }
    }

    fn stores(&self, depth: u32) -> bool {
        self.options.depth_cap.is_none_or(|cap| depth < cap)
    }

    fn record_progress(&self, stats: FileStats) {
        if stats.counted && self.progress.enabled() {
            self.progress
                .usage
                .fetch_add(stats.usage, Ordering::Relaxed);
        }
    }

    fn resolve_candidate(
        &self,
        dirfd: &OwnedFd,
        listing: &Listing,
        entry: &RawEntry,
    ) -> ResolvedEntry {
        let lstat_flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC;
        let lstat = match rustix::fs::statx(dirfd, listing.c_name(entry), lstat_flags, STATX_MASK) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return ResolvedEntry::Skip,
            Err(_) => return ResolvedEntry::ChildUnreadable,
        };
        let is_symlink = FileType::from_raw_mode(lstat.stx_mode.into()) == FileType::Symlink;
        let stat = if self.options.follow_symlinks && is_symlink {
            let target = match rustix::fs::statx(
                dirfd,
                listing.c_name(entry),
                AtFlags::STATX_DONT_SYNC,
                STATX_MASK,
            ) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => return ResolvedEntry::Skip,
                Err(_) => return ResolvedEntry::ChildUnreadable,
            };
            if FileType::from_raw_mode(target.stx_mode.into()) == FileType::Directory {
                lstat
            } else {
                target
            }
        } else {
            lstat
        };
        if FileType::from_raw_mode(stat.stx_mode.into()) == FileType::Directory {
            return ResolvedEntry::Directory;
        }
        if entry.kind == FileType::Unknown
            && self.filters.has_file_filter()
            && self
                .filters
                .skip_file(&listing.name(entry).to_string_lossy())
        {
            return ResolvedEntry::Skip;
        }
        if self.options.one_filesystem && device_of(&stat) != self.root_device {
            return ResolvedEntry::Skip;
        }
        self.resolve_meta(&stat)
            .map_or(ResolvedEntry::Skip, ResolvedEntry::File)
    }

    /// Resolves metadata shared by all stat paths.
    fn resolve_meta(&self, stat: &Statx) -> Option<FileMeta> {
        if !self
            .filters
            .accepts_mtime(stat.stx_mtime.tv_sec, i64::from(stat.stx_mtime.tv_nsec))
        {
            return None;
        }

        let mut stats = FileStats::plain(
            stat.stx_size as i64,
            stat.stx_blocks as i64 * BLOCK_SIZE,
            stat.stx_mtime.tv_sec,
        );
        if FileType::from_raw_mode(stat.stx_mode.into()) != FileType::RegularFile {
            stats = stats.with_flag(Flag::NotRegular);
        }

        let deduplicate = self.options.follow_symlinks || stat.stx_nlink > 1;
        Some(FileMeta {
            stats,
            device: if deduplicate { device_of(stat) } else { 0 },
            inode: if deduplicate { stat.stx_ino } else { 0 },
            deduplicate,
        })
    }

    fn finish_file(&self, mut meta: FileMeta) -> FileStats {
        // Count each hard-linked inode once.
        if meta.deduplicate && !self.links.insert(meta.device, meta.inode) {
            self.require_file_deterministic_retry();
            meta.stats = meta.stats.with_flag(Flag::HardLink);
            meta.stats.counted = false;
        }
        meta.stats
    }
}

impl RawEntry {
    fn name_bytes<'a>(&self, listing: &'a Listing) -> &'a [u8] {
        let start = self.name_start as usize;
        &listing.names[start..start + usize::from(self.name_len)]
    }
}

fn open_dir(
    at: BorrowedFd<'_>,
    path: impl rustix::path::Arg,
    follow_symlinks: bool,
) -> io::Result<OwnedFd> {
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !follow_symlinks {
        flags |= OFlags::NOFOLLOW;
    }
    rustix::fs::openat(at, path, flags, Mode::empty()).map_err(Into::into)
}

/// Reads a directory into one compact listing.
fn read_listing(dirfd: &OwnedFd) -> io::Result<Box<Listing>> {
    DENTS.with(|cell| {
        let mut buffer = cell.borrow_mut();
        let mut raw = RawDir::new(dirfd, &mut buffer);
        let mut listing = Box::<Listing>::default();

        while let Some(entry) = raw.next() {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                listing.push(name, entry.file_type());
            }
        }
        Ok(listing)
    })
}

fn device_of(stat: &Statx) -> u64 {
    (u64::from(stat.stx_dev_major) << 32) | u64::from(stat.stx_dev_minor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs as unix_fs;

    use tempfile::TempDir;

    /// ```text
    /// root/
    ///   big          4 KiB
    ///   link_a       1 KiB   hard-linked pair, counted once
    ///   link_b               ↳
    ///   dangling ->  nowhere symlink, counted but not followed
    ///   empty/               no children
    ///   nested/
    ///     deep/
    ///       leaf     2 KiB
    ///   skipme/
    ///     junk       8 KiB
    /// ```
    fn fixture() -> TempDir {
        let root = TempDir::new().expect("temp dir");
        let path = root.path();

        fs::write(path.join("big"), vec![0u8; 4096]).unwrap();
        fs::write(path.join("link_a"), vec![0u8; 1024]).unwrap();
        fs::hard_link(path.join("link_a"), path.join("link_b")).unwrap();
        unix_fs::symlink("nowhere", path.join("dangling")).unwrap();

        fs::create_dir(path.join("empty")).unwrap();
        fs::create_dir_all(path.join("nested/deep")).unwrap();
        fs::write(path.join("nested/deep/leaf"), vec![0u8; 2048]).unwrap();
        fs::create_dir(path.join("skipme")).unwrap();
        fs::write(path.join("skipme/junk"), vec![0u8; 8192]).unwrap();

        root
    }

    fn scan(root: &Path, filters: &Filters, options: Options) -> Dir {
        scan_with(root, filters, options, false)
    }

    fn scan_with(root: &Path, filters: &Filters, options: Options, parallel: bool) -> Dir {
        let workers = if parallel { 4 } else { 1 };
        run(root, filters, options, &Progress::default(), workers).expect("scan succeeds")
    }

    #[test]
    fn progress_counters_update_when_enabled() {
        let root = fixture();
        let progress = Progress::new(true);
        let dir = run(
            root.path(),
            &Filters::default(),
            Options::default(),
            &progress,
            1,
        )
        .unwrap();

        assert_eq!(progress.items(), dir.items - 1);
        assert_eq!(progress.usage(), dir.usage);
    }

    fn child<'a>(dir: &'a Dir, name: &str) -> crate::tree::Entry<'a> {
        dir.entries()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no child named {name}"))
    }

    #[test]
    fn walks_the_whole_tree_and_counts_every_item_once() {
        let root = fixture();
        let dir = scan(root.path(), &Filters::default(), Options::default());

        assert_eq!(dir.items, 11, "every item is counted exactly once");

        let counted: i64 = 4096 + 1024 + 2048 + 8192;
        assert!(
            dir.size >= counted && dir.size < counted + 4096,
            "apparent size {} should be the counted bytes plus small change, \
             not {} (which would mean the hard link was double-counted)",
            dir.size,
            counted + 1024
        );
    }

    #[test]
    fn a_hard_linked_pair_is_charged_once() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("a"), vec![0u8; 4096]).unwrap();
        fs::hard_link(root.path().join("a"), root.path().join("b")).unwrap();

        let dir = scan(root.path(), &Filters::default(), Options::default());
        let a = child(&dir, "a");
        let b = child(&dir, "b");
        let (first, second) = if a.flag == Flag::HardLink {
            (b, a)
        } else {
            (a, b)
        };

        assert_eq!(first.size, 4096, "the first sighting carries the size");
        assert_eq!(second.size, 4096, "and so does the second");
        assert_eq!(first.flag, Flag::Ok);
        assert_eq!(
            second.flag,
            Flag::HardLink,
            "the repeat is marked as already counted"
        );

        assert_eq!(dir.size, 4096, "counted once, not twice");
        assert_eq!(dir.usage, first.usage);
        assert_eq!(dir.items, 3, "both names are still items");
    }

    #[test]
    fn hard_link_identity_includes_the_device() {
        let links = InodeSet::new();
        assert!(links.insert(1, 42));
        assert!(links.insert(2, 42));
        assert!(!links.insert(1, 42));
    }

    #[test]
    fn non_utf8_names_are_scanned_without_loss_or_collision() {
        let root = TempDir::new().unwrap();
        let first = OsString::from_vec(b"bad\xff".to_vec());
        let second = OsString::from_vec(b"bad\xfe".to_vec());
        fs::write(root.path().join(&first), vec![0u8; 1024]).unwrap();
        fs::write(root.path().join(&second), vec![0u8; 2048]).unwrap();

        let dir = scan(root.path(), &Filters::default(), Options::default());
        let raw_names: Vec<_> = dir
            .entries()
            .map(|entry| entry.fs_name.as_bytes().to_vec())
            .collect();
        assert!(raw_names.contains(&first.as_bytes().to_vec()));
        assert!(raw_names.contains(&second.as_bytes().to_vec()));
        assert_eq!(dir.items, 3);
        assert_eq!(dir.size, 3072);
    }

    #[test]
    fn large_directories_parallelize_stats_without_changing_totals() {
        let root = TempDir::new().unwrap();
        for index in 0..=STAT_PARALLEL_THRESHOLD {
            fs::write(root.path().join(format!("file{index}")), []).unwrap();
        }
        let full = scan_with(root.path(), &Filters::default(), Options::default(), true);
        let capped = scan_with(
            root.path(),
            &Filters::default(),
            Options {
                depth_cap: Some(0),
                ..Options::default()
            },
            true,
        );

        assert_eq!(full.items, capped.items);
        assert_eq!(full.size, capped.size);
        assert_eq!(full.usage, capped.usage);
        assert_eq!(full.entries().count(), STAT_PARALLEL_THRESHOLD + 1);
        assert_eq!(capped.entries().count(), 0);
    }

    #[test]
    fn symlinks_are_counted_but_not_followed() {
        let root = fixture();
        let dir = scan(root.path(), &Filters::default(), Options::default());

        let link = child(&dir, "dangling");
        assert_eq!(link.flag, Flag::NotRegular);
        assert!(!link.is_dir());
        assert!(link.size < 100, "the link itself, not its (missing) target");
    }

    #[test]
    fn directory_symlinks_are_not_followed() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("leaf"), vec![0u8; 1024]).unwrap();
        unix_fs::symlink(root.path(), outside.path().join("back")).unwrap();
        unix_fs::symlink(outside.path(), root.path().join("linked")).unwrap();

        let dir = scan(
            root.path(),
            &Filters::default(),
            Options {
                follow_symlinks: true,
                ..Options::default()
            },
        );
        let linked = child(&dir, "linked");
        assert!(!linked.is_dir(), "directory symlinks remain links");
        assert_eq!(linked.flag, Flag::NotRegular);
        assert_eq!(dir.items, 2, "root and the un-followed directory link");
    }

    #[test]
    fn followed_symlinks_to_the_same_file_are_charged_once() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("file"), vec![0u8; 4096]).unwrap();
        unix_fs::symlink("file", root.path().join("link-a")).unwrap();
        unix_fs::symlink("file", root.path().join("link-b")).unwrap();

        let dir = scan(
            root.path(),
            &Filters::default(),
            Options {
                follow_symlinks: true,
                ..Options::default()
            },
        );

        assert_eq!(dir.size, 4096, "the target inode is charged only once");
        assert_eq!(dir.items, 4, "all three names remain visible");
        assert_eq!(
            dir.entries()
                .filter(|entry| entry.flag == Flag::HardLink)
                .count(),
            2
        );
    }

    #[test]
    fn parallel_scans_assign_shared_inode_blocks_deterministically() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("source"), vec![0u8; 4096]).unwrap();
        for name in ["d4", "d3", "d2", "d1"] {
            fs::create_dir(root.path().join(name)).unwrap();
            fs::hard_link(
                root.path().join("source"),
                root.path().join(name).join("shared"),
            )
            .unwrap();
        }
        fs::remove_file(root.path().join("source")).unwrap();

        for _ in 0..10 {
            let dir = scan_with(root.path(), &Filters::default(), Options::default(), true);
            let owner = dir
                .entries()
                .filter(|entry| entry.usage > 0)
                .map(|entry| entry.name)
                .collect::<Vec<_>>();
            assert_eq!(owner, ["d1"]);
        }
    }

    #[test]
    fn directory_symlinks_do_not_cross_filesystems_when_not_followed() {
        use std::os::unix::fs::MetadataExt;

        let root = TempDir::new().unwrap();
        let Ok(other) = tempfile::Builder::new()
            .prefix("rdu-cross-device-")
            .tempdir_in("/dev/shm")
        else {
            return;
        };
        if fs::metadata(root.path()).unwrap().dev() == fs::metadata(other.path()).unwrap().dev() {
            return;
        }
        fs::write(other.path().join("leaf"), vec![0u8; 1024]).unwrap();
        unix_fs::symlink(other.path(), root.path().join("linked")).unwrap();

        let dir = scan(
            root.path(),
            &Filters::default(),
            Options {
                follow_symlinks: true,
                one_filesystem: true,
                ..Options::default()
            },
        );
        let linked = child(&dir, "linked");
        assert!(!linked.is_dir());
        assert_eq!(linked.flag, Flag::NotRegular);
        assert_eq!(dir.items, 2, "the link itself stays on the root filesystem");
    }

    #[test]
    fn empty_directories_get_the_sentinel_and_the_empty_flag() {
        let root = fixture();
        let dir = scan(root.path(), &Filters::default(), Options::default());

        let empty = child(&dir, "empty");
        assert_eq!(empty.flag, Flag::Empty);
        assert_eq!(empty.items, 1);
        assert_eq!(empty.usage, 0);
        assert_eq!(empty.size, crate::tree::EMPTY_DIR_SIZE);
    }

    #[test]
    fn ignored_directories_contribute_nothing() {
        let root = fixture();
        let mut filters = Filters::default();
        filters.ignore_paths(root.path(), ["skipme"]);

        let dir = scan(root.path(), &filters, Options::default());
        assert!(
            dir.entries().all(|e| e.name != "skipme"),
            "the ignored directory is absent"
        );

        let full = scan(root.path(), &Filters::default(), Options::default());
        assert_eq!(
            full.size - dir.size,
            child(&full, "skipme").size,
            "exactly the skipped subtree drops out of the total"
        );
    }

    #[test]
    fn the_depth_cap_keeps_totals_but_drops_deep_entries() {
        let root = fixture();
        let deep = scan(root.path(), &Filters::default(), Options::default());
        let capped = scan(
            root.path(),
            &Filters::default(),
            Options {
                depth_cap: Some(1),
                ..Options::default()
            },
        );

        assert_eq!(
            capped.size, deep.size,
            "capping storage must not change the totals"
        );
        assert_eq!(capped.items, deep.items);

        let nested = child(&capped, "nested").dir.expect("nested is a directory");
        assert_eq!(
            nested.entries().count(),
            0,
            "entries below the cap are counted but not stored"
        );
        assert!(nested.size >= 2048, "yet its size still includes the leaf");
    }

    #[test]
    fn deep_trees_do_not_exhaust_a_worker_stack() {
        let root = TempDir::new().unwrap();
        let mut path = root.path().to_path_buf();
        for level in 0..300 {
            path.push(format!("level{level}"));
        }
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("bottom"), vec![0u8; 1024]).unwrap();

        for parallel in [false, true] {
            let dir = scan_with(
                root.path(),
                &Filters::default(),
                Options::default(),
                parallel,
            );
            assert_eq!(dir.items, 302, "300 directories, the file, and the root");
            assert!(dir.size >= 1024, "the deepest file is still counted");
        }
    }

    #[test]
    fn type_filters_exclude_matching_files_from_the_totals() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("keep.txt"), vec![0u8; 4096]).unwrap();
        fs::write(root.path().join("drop.log"), vec![0u8; 4096]).unwrap();

        let mut filters = Filters::default();
        filters.types(Some(crate::filter::TypeFilter::Except(vec!["log".into()])));

        let dir = scan(root.path(), &filters, Options::default());
        assert!(dir.entries().any(|e| e.name == "keep.txt"));
        assert!(dir.entries().all(|e| e.name != "drop.log"));
        assert_eq!(dir.items, 2, "the root plus the one surviving file");
    }

    #[test]
    fn a_filter_accepting_every_child_does_not_empty_the_directory() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join("one.txt"), vec![0u8; 1024]).unwrap();
        fs::write(root.path().join("two.txt"), vec![0u8; 2048]).unwrap();

        let mut filters = Filters::default();
        filters.types(Some(crate::filter::TypeFilter::Only(vec!["txt".into()])));
        let dir = scan(root.path(), &filters, Options::default());

        assert_eq!(dir.items, 3);
        assert_eq!(dir.entries().count(), 2);
        assert_eq!(dir.size, 3072);
        assert_ne!(dir.flag, Flag::Empty);
    }

    #[test]
    fn an_unreadable_directory_is_flagged_and_taints_its_parent() {
        let root = TempDir::new().unwrap();
        let locked = root.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("secret"), vec![0u8; 4096]).unwrap();
        fs::set_permissions(
            &locked,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o000),
        )
        .unwrap();

        let dir = scan(root.path(), &Filters::default(), Options::default());

        if fs::read_dir(&locked).is_err() {
            assert_eq!(child(&dir, "locked").flag, Flag::Unreadable);
            assert_eq!(dir.flag, Flag::ChildUnreadable);
        }

        fs::set_permissions(
            &locked,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
    }
}
