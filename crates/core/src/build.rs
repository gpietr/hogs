//! Building a [`Tree`] from the batches produced by parallel scanners.
//!
//! A scanner reads one directory at a time, reserves a contiguous id range for
//! its entries from an [`IdAllocator`] and emits a [`Batch`]. Because a
//! directory's id is reserved before its own children are, every child id is
//! larger than its parent's, which lets aggregation run as one reverse pass.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::tree::{NO_PARENT, NodeId, Tree, flags};

#[derive(Clone, Copy, Debug, Default)]
pub struct RawEntry {
    /// End offset of the name in [`Batch::names`].
    pub name_end: u32,
    pub size: u64,
    pub alloc: u64,
    pub mtime: i64,
    pub flags: u8,
}

/// All entries of one directory, with ids `base..base + entries.len()`.
#[derive(Debug, Default)]
pub struct Batch {
    pub parent: NodeId,
    pub base: NodeId,
    /// Flags to OR into the parent directory (e.g. [`flags::ERROR`] if it was only partially read).
    pub dir_flags: u8,
    pub names: Vec<u8>,
    pub entries: Vec<RawEntry>,
}

impl Batch {
    pub fn new(parent: NodeId) -> Self {
        Batch {
            parent,
            ..Default::default()
        }
    }

    pub fn push(&mut self, name: &[u8], size: u64, alloc: u64, mtime: i64, flags: u8) {
        self.names.extend_from_slice(name);
        let name_end = u32::try_from(self.names.len()).expect("directory names exceed 4 GiB");
        self.entries.push(RawEntry {
            name_end,
            size,
            alloc,
            mtime,
            flags,
        });
    }
}

/// Hands out node ids; id 0 is the root.
pub struct IdAllocator(AtomicU32);

impl Default for IdAllocator {
    fn default() -> Self {
        IdAllocator(AtomicU32::new(1))
    }
}

impl IdAllocator {
    pub fn reserve(&self, n: usize) -> NodeId {
        let n = u32::try_from(n).expect("too many entries");
        let base = self.0.fetch_add(n, Ordering::Relaxed);
        assert!(
            base.checked_add(n).is_some_and(|end| end < NO_PARENT),
            "too many entries"
        );
        base
    }

    pub fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Stat data of the scanned root directory itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct RootInfo {
    pub alloc: u64,
    pub mtime: i64,
}

/// Places batches into node arrays as they arrive (in any order), then
/// aggregates subtree totals in [`TreeBuilder::finish`].
pub struct TreeBuilder {
    names: Vec<u8>,
    name_off: Vec<u32>,
    name_len: Vec<u16>,
    parent: Vec<NodeId>,
    first_child: Vec<NodeId>,
    child_count: Vec<u32>,
    size: Vec<u64>,
    alloc: Vec<u64>,
    mtime: Vec<i64>,
    flags: Vec<u8>,
    /// Flags for directories whose own entry may not have been placed yet.
    dir_flags: Vec<(NodeId, u8)>,
}

impl TreeBuilder {
    pub fn new(root: RootInfo) -> Self {
        TreeBuilder {
            names: Vec::new(),
            name_off: vec![0],
            name_len: vec![0],
            parent: vec![NO_PARENT],
            first_child: vec![0],
            child_count: vec![0],
            size: vec![0],
            alloc: vec![root.alloc],
            mtime: vec![root.mtime],
            flags: vec![flags::DIR],
            dir_flags: Vec::new(),
        }
    }

    fn grow(&mut self, len: usize) {
        if len <= self.parent.len() {
            return;
        }
        let len = len.max(self.parent.len() * 3 / 2);
        self.name_off.resize(len, 0);
        self.name_len.resize(len, 0);
        self.parent.resize(len, NO_PARENT);
        self.first_child.resize(len, 0);
        self.child_count.resize(len, 0);
        self.size.resize(len, 0);
        self.alloc.resize(len, 0);
        self.mtime.resize(len, 0);
        self.flags.resize(len, 0);
    }

    pub fn add(&mut self, b: Batch) {
        let base = b.base as usize;
        self.grow(base + b.entries.len());
        let p = b.parent as usize;
        self.first_child[p] = b.base;
        self.child_count[p] = b.entries.len() as u32;
        if b.dir_flags != 0 {
            self.dir_flags.push((b.parent, b.dir_flags));
        }
        let pool_start = u32::try_from(self.names.len()).expect("name pool exceeds 4 GiB");
        self.names.extend_from_slice(&b.names);
        let mut start = 0;
        for (i, e) in b.entries.iter().enumerate() {
            let id = base + i;
            self.parent[id] = b.parent;
            // Like `du --apparent-size`, a directory's own st_size is not content.
            self.size[id] = if e.flags & flags::DIR != 0 { 0 } else { e.size };
            self.alloc[id] = e.alloc;
            self.mtime[id] = e.mtime;
            self.flags[id] = e.flags;
            self.name_off[id] = pool_start + start;
            self.name_len[id] = (e.name_end - start) as u16;
            start = e.name_end;
        }
    }

    /// Aggregates subtree totals; `count` is the number of ids handed out.
    pub fn finish(mut self, root_path: PathBuf, count: u32) -> Tree {
        let n = count as usize;
        self.grow(n);
        for (id, f) in std::mem::take(&mut self.dir_flags) {
            self.flags[id as usize] |= f;
        }
        let mut files: Vec<u32> = self.flags[..n]
            .iter()
            .map(|&f| u32::from(f & flags::DIR == 0))
            .collect();
        files[0] = 0;
        for id in (1..n).rev() {
            let p = self.parent[id];
            if p == NO_PARENT {
                continue; // reserved but never filled; cannot happen with well-behaved scanners
            }
            let p = p as usize;
            self.size[p] += self.size[id];
            self.alloc[p] += self.alloc[id];
            files[p] += files[id];
            self.mtime[p] = self.mtime[p].max(self.mtime[id]);
        }
        let mut t = Tree {
            root: root_path,
            scanned_at: crate::staging::now(),
            names: self.names,
            name_off: self.name_off,
            name_len: self.name_len,
            parent: self.parent,
            first_child: self.first_child,
            child_count: self.child_count,
            size: self.size,
            alloc: self.alloc,
            mtime: self.mtime,
            files,
            flags: self.flags,
        };
        t.truncate(n);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_aggregates() {
        let ids = IdAllocator::default();
        // root: a.txt (10), sub/ (dir) ; sub: big (100), small (1)
        let mut root = Batch::new(0);
        root.base = ids.reserve(2);
        root.push(b"a.txt", 10, 10, 5, 0);
        root.push(b"sub", 0, 0, 1, flags::DIR);
        let sub_id = root.base + 1;
        let mut sub = Batch::new(sub_id);
        sub.base = ids.reserve(2);
        sub.push(b"small", 1, 1, 7, 0);
        sub.push(b"big", 100, 100, 3, 0);
        sub.dir_flags = flags::ERROR;

        // Out of order on purpose: a directory's batch may arrive before its parent's.
        let mut builder = TreeBuilder::new(RootInfo::default());
        builder.add(sub);
        builder.add(root);
        let t = builder.finish("/r".into(), ids.count());
        assert_eq!(t.len(), 5);
        assert_eq!(t.size(Tree::ROOT), 111);
        assert_eq!(t.files(Tree::ROOT), 3);
        assert_eq!(t.mtime(Tree::ROOT), 7);

        let kids: Vec<_> = t
            .children_by_size(Tree::ROOT)
            .iter()
            .map(|&c| t.name_lossy(c).into_owned())
            .collect();
        assert_eq!(kids, ["sub", "a.txt"]);
        let sub = sub_id;
        assert!(t.flags(sub) & flags::ERROR != 0);
        let kids: Vec<_> = t
            .children_by_size(sub)
            .iter()
            .map(|&c| t.name_lossy(c).into_owned())
            .collect();
        assert_eq!(kids, ["big", "small"]);

        let big = t.children_by_size(sub)[0];
        assert_eq!(t.path(big), PathBuf::from("/r/sub/big"));
        assert_eq!(t.lookup(&PathBuf::from("/r/sub/big")), Some(big));
        assert_eq!(t.lookup(&PathBuf::from("/r/nope")), None);
        assert_eq!(t.parent(big), Some(sub));
    }
}
