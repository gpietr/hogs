//! The scanned file tree, stored as a struct-of-arrays arena.
//!
//! Nodes are stored in scan order: the children of every directory are
//! contiguous (in directory order) and always have larger ids than their parent.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

pub type NodeId = u32;

/// Sentinel parent of the root node.
pub const NO_PARENT: NodeId = NodeId::MAX;

pub mod flags {
    pub const DIR: u8 = 1;
    pub const SYMLINK: u8 = 1 << 1;
    /// Hardlink to an inode already counted elsewhere; contributes no size.
    pub const HARDLINK_DUP: u8 = 1 << 2;
    /// The entry could not be fully read (permission denied, I/O error, ...).
    pub const ERROR: u8 = 1 << 3;
    /// Mount point or pseudo filesystem that was not descended into.
    pub const OTHER_FS: u8 = 1 << 4;
}

pub struct Tree {
    pub(crate) root: PathBuf,
    /// When the scan finished (Unix seconds).
    pub(crate) scanned_at: i64,
    pub(crate) names: Vec<u8>,
    pub(crate) name_off: Vec<u32>,
    pub(crate) name_len: Vec<u16>,
    pub(crate) parent: Vec<NodeId>,
    pub(crate) first_child: Vec<NodeId>,
    pub(crate) child_count: Vec<u32>,
    /// Apparent size in bytes (subtree total for directories, which count no size of their own).
    pub(crate) size: Vec<u64>,
    /// Allocated (on-disk) size in bytes (subtree total for directories).
    pub(crate) alloc: Vec<u64>,
    /// Modification time in Unix seconds; for directories, the newest in the subtree.
    pub(crate) mtime: Vec<i64>,
    /// Number of non-directory entries in the subtree (1 for a file).
    pub(crate) files: Vec<u32>,
    pub(crate) flags: Vec<u8>,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    pub fn scanned_at(&self) -> i64 {
        self.scanned_at
    }

    /// Raw name bytes; empty for the root.
    pub fn name(&self, id: NodeId) -> &[u8] {
        let off = self.name_off[id as usize] as usize;
        &self.names[off..off + self.name_len[id as usize] as usize]
    }

    pub fn name_lossy(&self, id: NodeId) -> Cow<'_, str> {
        String::from_utf8_lossy(self.name(id))
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        Some(self.parent[id as usize]).filter(|&p| p != NO_PARENT)
    }

    pub fn children(&self, id: NodeId) -> Range<NodeId> {
        let first = self.first_child[id as usize];
        first..first + self.child_count[id as usize]
    }

    /// Children sorted by allocated size, largest first.
    pub fn children_by_size(&self, id: NodeId) -> Vec<NodeId> {
        let mut kids: Vec<NodeId> = self.children(id).collect();
        kids.sort_unstable_by_key(|&c| std::cmp::Reverse((self.alloc(c), self.size(c))));
        kids
    }

    pub fn size(&self, id: NodeId) -> u64 {
        self.size[id as usize]
    }

    pub fn alloc(&self, id: NodeId) -> u64 {
        self.alloc[id as usize]
    }

    pub fn mtime(&self, id: NodeId) -> i64 {
        self.mtime[id as usize]
    }

    pub fn files(&self, id: NodeId) -> u32 {
        self.files[id as usize]
    }

    pub fn flags(&self, id: NodeId) -> u8 {
        self.flags[id as usize]
    }

    pub fn is_dir(&self, id: NodeId) -> bool {
        self.flags(id) & flags::DIR != 0
    }

    pub fn depth(&self, mut id: NodeId) -> usize {
        let mut depth = 0;
        while let Some(p) = self.parent(id) {
            depth += 1;
            id = p;
        }
        depth
    }

    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut chain = Vec::new();
        let mut cur = id;
        while let Some(p) = self.parent(cur) {
            chain.push(cur);
            cur = p;
        }
        let mut path = self.root.clone();
        for &n in chain.iter().rev() {
            path.push(os_str_from_bytes(self.name(n)));
        }
        path
    }

    pub(crate) fn truncate(&mut self, n: usize) {
        self.name_off.truncate(n);
        self.name_len.truncate(n);
        self.parent.truncate(n);
        self.first_child.truncate(n);
        self.child_count.truncate(n);
        self.size.truncate(n);
        self.alloc.truncate(n);
        self.mtime.truncate(n);
        self.files.truncate(n);
        self.flags.truncate(n);
    }

    /// Like [`Tree::lookup`], but only when the scan has the node's contents: a
    /// mount point that was not descended into is in the tree, but empty.
    pub fn lookup_scanned(&self, path: &Path) -> Option<NodeId> {
        self.lookup(path)
            .filter(|&id| self.flags(id) & flags::OTHER_FS == 0)
    }

    /// A copy with only the nodes `keep` accepts (and always the root). Kept
    /// nodes keep their original totals, so directory sizes stay exact even
    /// when their small files are left out. Children of a dropped node are
    /// dropped too.
    pub fn pruned(&self, keep: impl Fn(NodeId) -> bool) -> Tree {
        let mut order: Vec<NodeId> = vec![Self::ROOT];
        let mut parent = vec![NO_PARENT];
        let mut first_child = Vec::new();
        let mut child_count = Vec::new();
        let mut head = 0;
        while head < order.len() {
            let old = order[head];
            let start = order.len();
            order.extend(self.children(old).filter(|&c| keep(c)));
            first_child.push(start as NodeId);
            child_count.push((order.len() - start) as u32);
            parent.resize(order.len(), head as NodeId);
            head += 1;
        }
        let mut names = Vec::new();
        let mut name_off = Vec::with_capacity(order.len());
        let mut name_len = Vec::with_capacity(order.len());
        for &id in &order {
            name_off.push(names.len() as u32);
            name_len.push(self.name_len[id as usize]);
            names.extend_from_slice(self.name(id));
        }
        let pick = |v: &Vec<u64>| order.iter().map(|&i| v[i as usize]).collect();
        Tree {
            root: self.root.clone(),
            scanned_at: self.scanned_at,
            names,
            name_off,
            name_len,
            parent,
            first_child,
            child_count,
            size: pick(&self.size),
            alloc: pick(&self.alloc),
            mtime: order.iter().map(|&i| self.mtime[i as usize]).collect(),
            files: order.iter().map(|&i| self.files[i as usize]).collect(),
            flags: order.iter().map(|&i| self.flags[i as usize]).collect(),
        }
    }

    /// Finds the node for an absolute path inside the scanned root.
    pub fn lookup(&self, path: &Path) -> Option<NodeId> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut id = Self::ROOT;
        for comp in rel.components() {
            match comp {
                Component::Normal(name) => {
                    let name = name.as_encoded_bytes();
                    id = self.children(id).find(|&c| self.name(c) == name)?;
                }
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(id)
    }
}

/// Converts stored name bytes back to an `OsStr`.
pub fn os_str_from_bytes(bytes: &[u8]) -> Cow<'_, OsStr> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Cow::Borrowed(OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        match std::str::from_utf8(bytes) {
            Ok(s) => Cow::Borrowed(OsStr::new(s)),
            Err(_) => Cow::Owned(String::from_utf8_lossy(bytes).into_owned().into()),
        }
    }
}
