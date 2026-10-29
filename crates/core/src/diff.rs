//! Comparing directories between two scans of the same place.

use std::collections::HashMap;

use crate::history::MIN_FILE;
use crate::tree::{NodeId, Tree};

/// One entry of a directory, matched by name between a newer and an older scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    /// The entry in the newer scan; `None` if it is gone.
    pub new: Option<NodeId>,
    /// The entry in the older scan; `None` if it is new.
    pub old: Option<NodeId>,
    /// Allocated size now and before.
    pub now: u64,
    pub before: u64,
}

impl Change {
    pub fn delta(&self) -> i64 {
        self.now as i64 - self.before as i64
    }

    pub fn is_dir(&self, new: &Tree, old: &Tree) -> bool {
        match (self.new, self.old) {
            (Some(n), _) => new.is_dir(n),
            (None, Some(o)) => old.is_dir(o),
            (None, None) => false,
        }
    }
}

/// The children of a directory in both scans, largest change first. Either
/// side may be missing (a new or gone directory).
///
/// History scans leave out files under [`MIN_FILE`], so a small file with no
/// older counterpart may have existed before: those are skipped rather than
/// reported as new (their directory's total still includes them).
pub fn children(
    new: &Tree,
    new_dir: Option<NodeId>,
    old: &Tree,
    old_dir: Option<NodeId>,
) -> Vec<Change> {
    let mut old_by_name: HashMap<&[u8], NodeId> = old_dir
        .map(|d| old.children(d).map(|c| (old.name(c), c)).collect())
        .unwrap_or_default();
    let mut changes = Vec::new();
    if let Some(d) = new_dir {
        for c in new.children(d) {
            let before = old_by_name.remove(new.name(c));
            if before.is_none() && old_dir.is_some() && !new.is_dir(c) && new.alloc(c) < MIN_FILE {
                continue;
            }
            changes.push(Change {
                new: Some(c),
                old: before,
                now: new.alloc(c),
                before: before.map_or(0, |o| old.alloc(o)),
            });
        }
    }
    changes.extend(old_by_name.into_values().map(|o| Change {
        new: None,
        old: Some(o),
        now: 0,
        before: old.alloc(o),
    }));
    changes.sort_by_key(|c| std::cmp::Reverse(c.delta().unsigned_abs()));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{Batch, IdAllocator, RootInfo, TreeBuilder};
    use crate::flags;

    /// root: entries given as (name, alloc, is_dir)
    fn tree(entries: &[(&str, u64, bool)]) -> Tree {
        let ids = IdAllocator::default();
        let mut b = Batch::new(0);
        b.base = ids.reserve(entries.len());
        for &(name, alloc, dir) in entries {
            b.push(
                name.as_bytes(),
                alloc,
                alloc,
                0,
                if dir { flags::DIR } else { 0 },
            );
        }
        let mut builder = TreeBuilder::new(RootInfo::default());
        builder.add(b);
        builder.finish("/r".into(), ids.count())
    }

    #[test]
    fn matches_children_by_name() {
        let old = tree(&[
            ("grew", 10 << 20, false),
            ("gone", 3 << 20, false),
            ("same", 5 << 20, false),
            ("cache", 4096, true),
        ]);
        let new = tree(&[
            ("grew", 50 << 20, false),
            ("same", 5 << 20, false),
            ("added", 2 << 20, false),
            ("small-new", 4096, false),
            ("cache", 4096, true),
        ]);
        let changes = children(&new, Some(Tree::ROOT), &old, Some(Tree::ROOT));
        let named: Vec<(String, i64)> = changes
            .iter()
            .map(|c| {
                let name = match (c.new, c.old) {
                    (Some(n), _) => new.name_lossy(n).into_owned(),
                    (None, Some(o)) => old.name_lossy(o).into_owned(),
                    _ => unreachable!(),
                };
                (name, c.delta())
            })
            .collect();
        assert_eq!(named[0], ("grew".to_string(), 40 << 20));
        assert_eq!(named[1], ("gone".to_string(), -(3 << 20)));
        assert_eq!(named[2], ("added".to_string(), 2 << 20));
        assert!(
            !named.iter().any(|(n, _)| n == "small-new"),
            "small files history cannot have seen are skipped"
        );
        assert!(named.iter().any(|(n, d)| n == "same" && *d == 0));
        assert!(
            changes
                .iter()
                .find(|c| c.new.is_some_and(|n| new.name(n) == b"cache"))
                .unwrap()
                .is_dir(&new, &old)
        );
    }

    #[test]
    fn new_directory_lists_everything() {
        let new = tree(&[("a", 4096, false)]);
        let old = tree(&[]);
        assert_eq!(children(&new, Some(Tree::ROOT), &old, None).len(), 1);
    }
}
