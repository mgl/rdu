//! Staged, directory-relative deletion without following symlinks.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, ResolveFlags, Stat};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    kind: FileType,
}

impl Identity {
    fn of(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev,
            inode: stat.st_ino,
            kind: FileType::from_raw_mode(stat.st_mode),
        }
    }
}

struct Pending {
    identity: Identity,
    usage: i64,
}

pub struct Batch {
    root: PathBuf,
    source: PathBuf,
    dirfd: OwnedFd,
    device: u64,
    entries: BTreeMap<PathBuf, Pending>,
    usage: i64,
}

impl Batch {
    pub fn new(root: &Path) -> io::Result<Self> {
        let source = root.to_path_buf();
        let root = root.canonicalize()?;
        let dirfd = fs::openat(
            fs::CWD,
            &root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let device = fs::fstat(&dirfd)?.st_dev;
        Ok(Self {
            root,
            source,
            dirfd,
            device,
            entries: BTreeMap::new(),
            usage: 0,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn usage(&self) -> i64 {
        self.usage
    }

    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.entries.keys()
    }

    pub fn covers(&self, path: &Path) -> bool {
        path.ancestors().any(|path| self.entries.contains_key(path))
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.usage = 0;
    }

    pub fn toggle(&mut self, path: &Path, usage: i64) -> io::Result<()> {
        validate_relative(path)?;
        if let Some(removed) = self.entries.remove(path) {
            self.usage -= removed.usage;
            return Ok(());
        }
        if self.covers(path) {
            return Err(io::Error::other(
                "Already included in a marked folder; unmark that folder first",
            ));
        }
        let (parent, name) = self.parent(path)?;
        let stat = fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let identity = Identity::of(&stat);
        let usage = if identity.kind == FileType::Symlink {
            stat.st_blocks
                .checked_mul(512)
                .filter(|usage| *usage >= 0)
                .ok_or_else(|| io::Error::other("Invalid symlink block count"))?
        } else {
            usage
        };
        if identity.device != self.device {
            return Err(io::Error::other("Refusing to delete across filesystems"));
        }
        self.entries.retain(|entry, pending| {
            if entry.starts_with(path) {
                self.usage -= pending.usage;
                false
            } else {
                true
            }
        });
        self.entries
            .insert(path.to_owned(), Pending { identity, usage });
        self.usage += usage;
        Ok(())
    }

    pub fn apply(&mut self) -> io::Result<usize> {
        self.validate_root()?;
        let mut completed = 0;
        while let Some((path, pending)) = self.entries.first_key_value() {
            let result = self.parent(path).and_then(|(parent, name)| {
                remove_entry(parent.as_fd(), name, pending.identity, self.device)
            });
            if let Err(err) = result {
                return Err(io::Error::other(format!(
                    "Deleted {completed} marked entries; failed at {:?}: {err}. \
                     A folder may have been partially deleted. Remaining marks were kept.",
                    self.root.join(path)
                )));
            }
            let path = path.clone();
            let removed = self.entries.remove(&path).expect("completed pending entry");
            self.usage -= removed.usage;
            completed += 1;
        }
        Ok(completed)
    }

    pub fn validate_root(&self) -> io::Result<()> {
        // A renamed/replaced scan root must not redirect a batch or rescan.
        let current = fs::statat(fs::CWD, &self.source, AtFlags::empty())?;
        if Identity::of(&current) != Identity::of(&fs::fstat(&self.dirfd)?) {
            return Err(io::Error::other(
                "The scan root changed; deletion is disabled",
            ));
        }
        Ok(())
    }

    pub fn update_estimates(&mut self, tree: &crate::tree::Dir) {
        for (path, pending) in &mut self.entries {
            if pending.identity.kind == FileType::Symlink {
                continue;
            }
            let mut dir = tree;
            let mut components = path.components().peekable();
            pending.usage = 0;
            while let Some(component) = components.next() {
                let Some(entry) = dir
                    .entries()
                    .find(|entry| entry.fs_name == component.as_os_str())
                else {
                    break;
                };
                if components.peek().is_none() {
                    pending.usage = if entry.flag == crate::tree::Flag::HardLink {
                        0
                    } else {
                        entry.usage
                    };
                } else if let Some(child) = entry.dir {
                    dir = child;
                } else {
                    break;
                }
            }
        }
        self.usage = self.entries.values().map(|entry| entry.usage).sum();
    }

    fn parent<'a>(&self, path: &'a Path) -> io::Result<(OwnedFd, &'a OsStr)> {
        validate_relative(path)?;
        let mut parent = open_dir(self.dirfd.as_fd(), OsStr::new("."))?;
        for component in path
            .parent()
            .expect("relative child has a parent")
            .components()
        {
            parent = open_dir(parent.as_fd(), component.as_os_str())?;
            if fs::fstat(&parent)?.st_dev != self.device {
                return Err(io::Error::other("Refusing to delete across filesystems"));
            }
        }
        Ok((parent, path.file_name().expect("validated child name")))
    }
}

fn validate_relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Deletion requires a child of the scan root",
        ));
    }
    Ok(())
}

fn open_dir(parent: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    Ok(fs::openat2(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )?)
}

fn remove_entry(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    expected: Identity,
    device: u64,
) -> io::Result<()> {
    verify_entry(parent, name, expected, device)?;
    if expected.kind != FileType::Directory {
        return Ok(fs::unlinkat(parent, name, AtFlags::empty())?);
    }

    struct Directory {
        entries: fs::Dir,
        name: OsString,
        identity: Identity,
    }

    impl Directory {
        fn open(parent: BorrowedFd<'_>, name: &OsStr, expected: Identity) -> io::Result<Self> {
            let dirfd = open_dir(parent, name)?;
            if Identity::of(&fs::fstat(&dirfd)?) != expected {
                return Err(io::Error::other("Directory changed before deletion"));
            }
            Ok(Self {
                entries: fs::Dir::new(dirfd)?,
                name: name.to_owned(),
                identity: expected,
            })
        }
    }

    // Keep traversal state on the heap so deep trees cannot exhaust the stack.
    let mut stack = vec![Directory::open(parent, name, expected)?];
    while let Some(directory) = stack.last_mut() {
        if let Some(entry) = directory.entries.next() {
            let entry = entry?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            let name = OsStr::from_bytes(name.to_bytes());
            let parent = directory.entries.fd()?;
            let stat = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let identity = Identity::of(&stat);
            verify_entry(parent, name, identity, device)?;
            if identity.kind == FileType::Directory {
                let child = Directory::open(parent, name, identity)?;
                stack.push(child);
            } else {
                fs::unlinkat(parent, name, AtFlags::empty())?;
            }
        } else {
            let directory = stack.pop().expect("finished directory");
            let parent = match stack.last() {
                Some(parent) => parent.entries.fd()?,
                None => parent,
            };
            verify_entry(parent, &directory.name, directory.identity, device)?;
            fs::unlinkat(parent, &directory.name, AtFlags::REMOVEDIR)?;
        }
    }
    Ok(())
}

fn verify_entry(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    expected: Identity,
    device: u64,
) -> io::Result<()> {
    let current = Identity::of(&fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?);
    if current != expected {
        return Err(io::Error::other(
            "Entry changed since it was marked; unmark and review it again",
        ));
    }
    if current.device != device {
        return Err(io::Error::other("Refusing to delete across filesystems"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as std_fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    #[test]
    fn marking_is_non_destructive_and_parent_marks_replace_descendants() {
        let root = tempfile::tempdir().unwrap();
        std_fs::create_dir(root.path().join("folder")).unwrap();
        std_fs::write(root.path().join("folder/file"), "data").unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("folder/file"), 4096).unwrap();
        assert!(root.path().join("folder/file").exists());
        assert_eq!(batch.usage(), 4096);
        batch.toggle(Path::new("folder"), 4096).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.usage(), 4096);
        assert!(batch.covers(Path::new("folder/file")));
        assert!(batch.toggle(Path::new("folder/file"), 4096).is_err());
        batch.toggle(Path::new("folder"), 4096).unwrap();
        assert_eq!(batch.len(), 0);
        assert_eq!(batch.usage(), 0);
        assert!(root.path().join("folder/file").exists());
    }

    #[test]
    fn deletes_a_batch_including_recursive_unlisted_contents() {
        let root = tempfile::tempdir().unwrap();
        std_fs::create_dir_all(root.path().join("folder/nested")).unwrap();
        std_fs::write(root.path().join("folder/nested/hidden"), "data").unwrap();
        std_fs::write(root.path().join("file"), "data").unwrap();
        std_fs::write(root.path().join("keep"), "data").unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("file"), 4096).unwrap();
        batch.toggle(Path::new("folder"), 4096).unwrap();
        assert_eq!(batch.apply().unwrap(), 2);
        assert!(!root.path().join("file").exists());
        assert!(!root.path().join("folder").exists());
        assert!(root.path().join("keep").exists());
        assert_eq!(batch.len(), 0);
        assert_eq!(batch.usage(), 0);
    }

    #[test]
    fn symlinks_are_unlinked_not_followed_even_inside_folders() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std_fs::write(outside.path().join("keep"), "data").unwrap();
        std_fs::create_dir(root.path().join("folder")).unwrap();
        symlink(outside.path(), root.path().join("folder/link")).unwrap();
        symlink(outside.path().join("keep"), root.path().join("link")).unwrap();
        symlink("missing", root.path().join("dangling")).unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        for path in ["folder", "link", "dangling"] {
            batch.toggle(Path::new(path), 0).unwrap();
        }
        assert_eq!(batch.apply().unwrap(), 3);
        assert_eq!(std_fs::read(outside.path().join("keep")).unwrap(), b"data");
        assert_eq!(std_fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn refuses_root_absolute_paths_and_parent_components() {
        let root = tempfile::tempdir().unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        for path in ["", ".", "..", "../outside", "folder/../file", "/"] {
            assert!(batch.toggle(Path::new(path), 0).is_err(), "{path:?}");
        }
        assert!(batch.toggle(root.path(), 0).is_err());
        assert!(root.path().exists());
    }

    #[test]
    fn replacing_a_marked_entry_does_not_delete_the_replacement() {
        let root = tempfile::tempdir().unwrap();
        std_fs::write(root.path().join("file"), "original").unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("file"), 4096).unwrap();
        std_fs::rename(root.path().join("file"), root.path().join("original")).unwrap();
        std_fs::write(root.path().join("file"), "replacement").unwrap();
        let err = batch.apply().unwrap_err();
        assert!(err.to_string().contains("changed since"));
        assert_eq!(
            std_fs::read(root.path().join("file")).unwrap(),
            b"replacement"
        );
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn a_symlinked_parent_cannot_redirect_marking_or_deletion() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std_fs::write(outside.path().join("file"), "outside").unwrap();
        std_fs::create_dir(root.path().join("folder")).unwrap();
        std_fs::write(root.path().join("folder/file"), "inside").unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("folder/file"), 4096).unwrap();
        std_fs::rename(root.path().join("folder"), root.path().join("original")).unwrap();
        symlink(outside.path(), root.path().join("folder")).unwrap();
        assert!(batch.apply().is_err());
        assert_eq!(
            std_fs::read(outside.path().join("file")).unwrap(),
            b"outside"
        );
        batch.clear();
        assert!(batch.toggle(Path::new("folder/file"), 4096).is_err());
    }

    #[test]
    fn a_replaced_scan_root_disables_the_batch() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        std_fs::create_dir(&root).unwrap();
        std_fs::write(root.join("file"), "original").unwrap();
        let mut batch = Batch::new(&root).unwrap();
        batch.toggle(Path::new("file"), 4096).unwrap();
        std_fs::rename(&root, parent.path().join("original")).unwrap();
        std_fs::create_dir(&root).unwrap();
        std_fs::write(root.join("file"), "replacement").unwrap();
        assert!(batch.apply().is_err());
        assert_eq!(std_fs::read(root.join("file")).unwrap(), b"replacement");
        assert!(parent.path().join("original/file").exists());
    }

    #[test]
    fn failure_keeps_remaining_marks_and_reports_completed_entries() {
        let root = tempfile::tempdir().unwrap();
        std_fs::write(root.path().join("a"), "a").unwrap();
        std_fs::write(root.path().join("b"), "b").unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("a"), 4096).unwrap();
        batch.toggle(Path::new("b"), 4096).unwrap();
        std_fs::remove_file(root.path().join("b")).unwrap();
        let err = batch.apply().unwrap_err();
        assert!(err.to_string().contains("Deleted 1 marked entries"));
        assert!(!root.path().join("a").exists());
        assert_eq!(batch.len(), 1);
        assert_eq!(batch.usage(), 4096);
    }

    #[test]
    fn raw_filename_bytes_are_preserved() {
        let root = tempfile::tempdir().unwrap();
        let name = OsStr::from_bytes(b"bad\xff");
        std_fs::write(root.path().join(name), "data").unwrap();
        std_fs::write(root.path().join("bad\u{fffd}"), "keep").unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new(name), 4096).unwrap();
        assert_eq!(batch.apply().unwrap(), 1);
        assert!(!root.path().join(name).exists());
        assert!(root.path().join("bad\u{fffd}").exists());
    }

    #[test]
    fn remaining_estimates_are_updated_after_a_partial_deletion() {
        let root = tempfile::tempdir().unwrap();
        std_fs::create_dir(root.path().join("folder")).unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("folder"), 8192).unwrap();
        let mut tree = crate::tree::Dir::new(root.path());
        let mut folder = crate::tree::Dir::new("folder");
        folder.usage = 4096;
        tree.push_dir(folder);
        batch.update_estimates(&tree);
        assert_eq!(batch.usage(), 4096);
    }

    #[test]
    fn deep_folders_are_deleted_without_recursive_stack_growth() {
        let root = tempfile::tempdir().unwrap();
        let mut path = root.path().join("folder");
        std_fs::create_dir(&path).unwrap();
        for _ in 0..300 {
            path.push("d");
            std_fs::create_dir(&path).unwrap();
        }
        std_fs::write(path.join("file"), "data").unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("folder"), 4096).unwrap();
        assert_eq!(batch.apply().unwrap(), 1);
        assert!(!root.path().join("folder").exists());
    }

    #[test]
    fn symlink_estimates_never_charge_the_followed_target() {
        let root = tempfile::tempdir().unwrap();
        std_fs::write(root.path().join("target"), vec![0u8; 8192]).unwrap();
        symlink("target", root.path().join("link")).unwrap();
        let mut batch = Batch::new(root.path()).unwrap();
        batch.toggle(Path::new("link"), 8192).unwrap();
        let stat =
            fs::statat(fs::CWD, root.path().join("link"), AtFlags::SYMLINK_NOFOLLOW).unwrap();
        assert_eq!(batch.usage(), stat.st_blocks * 512);
        let before = batch.usage();
        let mut tree = crate::tree::Dir::new(root.path());
        tree.push_file("link", crate::tree::FileStats::plain(8192, 8192, 0));
        batch.update_estimates(&tree);
        assert_eq!(batch.usage(), before);
        assert_eq!(batch.apply().unwrap(), 1);
        assert!(root.path().join("target").exists());
    }
}
