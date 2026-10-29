//! Read-only queries over a scanned tree, shared by the CLI and the TUI.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::tree::{NodeId, Tree};

impl Tree {
    /// Visits every node below `under` depth-first; `visit` gets the node and
    /// its depth relative to `under` (children are depth 1) and returns whether
    /// to descend into it.
    pub fn walk(&self, under: NodeId, mut visit: impl FnMut(NodeId, usize) -> bool) {
        let mut stack = vec![(self.children(under), 1)];
        while let Some((range, depth)) = stack.last_mut() {
            let depth = *depth;
            let Some(id) = range.next() else {
                stack.pop();
                continue;
            };
            if visit(id, depth) && self.is_dir(id) {
                stack.push((self.children(id), depth + 1));
            }
        }
    }
}

/// Keeps the `n` items with the largest key.
struct TopN {
    n: usize,
    heap: BinaryHeap<Reverse<(u64, NodeId)>>,
}

impl TopN {
    fn new(n: usize) -> Self {
        TopN {
            n,
            heap: BinaryHeap::with_capacity(n + 1),
        }
    }

    fn push(&mut self, key: u64, id: NodeId) {
        if self.heap.len() < self.n {
            self.heap.push(Reverse((key, id)));
        } else if self.heap.peek().is_some_and(|Reverse((min, _))| key > *min) {
            self.heap.pop();
            self.heap.push(Reverse((key, id)));
        }
    }

    fn into_sorted(self) -> Vec<NodeId> {
        self.heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse((_, id))| id)
            .collect()
    }
}

/// The `n` largest files (by allocated size) below `under`.
pub fn largest_files(tree: &Tree, under: NodeId, n: usize) -> Vec<NodeId> {
    let mut top = TopN::new(n);
    tree.walk(under, |id, _| {
        if !tree.is_dir(id) {
            top.push(tree.alloc(id), id);
        }
        true
    });
    top.into_sorted()
}

/// The `n` largest directories below `under`, at most `max_depth` levels deep.
pub fn largest_dirs(tree: &Tree, under: NodeId, n: usize, max_depth: usize) -> Vec<NodeId> {
    let mut top = TopN::new(n);
    tree.walk(under, |id, depth| {
        if tree.is_dir(id) {
            top.push(tree.alloc(id), id);
        }
        depth < max_depth
    });
    top.into_sorted()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtStat {
    /// Lowercased extension without the dot; empty for files without one.
    pub ext: String,
    pub alloc: u64,
    pub files: u64,
}

/// Allocated size per file extension below `under`, largest first.
pub fn by_extension(tree: &Tree, under: NodeId) -> Vec<ExtStat> {
    let mut map: HashMap<Vec<u8>, (u64, u64)> = HashMap::new();
    tree.walk(under, |id, _| {
        if !tree.is_dir(id) {
            let e = map
                .entry(extension(tree.name(id)).to_ascii_lowercase())
                .or_default();
            e.0 += tree.alloc(id);
            e.1 += 1;
        }
        true
    });
    let mut stats: Vec<ExtStat> = map
        .into_iter()
        .map(|(ext, (alloc, files))| ExtStat {
            ext: String::from_utf8_lossy(&ext).into_owned(),
            alloc,
            files,
        })
        .collect();
    stats.sort_unstable_by_key(|s| Reverse(s.alloc));
    stats
}

fn extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(0) | None => b"",
        Some(i) => &name[i + 1..],
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Any,
    File,
    Dir,
}

#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Case-insensitive wildcard pattern (`*`, `?`) matched against the entry name.
    pub name: Option<String>,
    pub kind: Kind,
    pub min_alloc: u64,
    /// Only entries whose newest modification (anywhere inside, for directories) is older than this Unix time.
    pub modified_before: Option<i64>,
}

impl Filter {
    fn matches(&self, tree: &Tree, id: NodeId) -> bool {
        let kind_ok = match self.kind {
            Kind::Any => true,
            Kind::File => !tree.is_dir(id),
            Kind::Dir => tree.is_dir(id),
        };
        kind_ok
            && tree.alloc(id) >= self.min_alloc
            && self.modified_before.is_none_or(|t| tree.mtime(id) < t)
            && self
                .name
                .as_deref()
                .is_none_or(|p| wildcard_match(p.as_bytes(), tree.name(id)))
    }
}

pub struct FindResult {
    /// The largest matches first, at most the requested limit.
    pub matches: Vec<NodeId>,
    pub total_matches: usize,
    /// Allocated size of all matches; matches never overlap, so this is what deleting them all would free.
    pub total_alloc: u64,
}

/// Finds entries below `under` matching `filter`. Matched directories are not
/// searched further, so nested matches (e.g. `node_modules` inside
/// `node_modules`) are not double counted.
pub fn find(tree: &Tree, under: NodeId, filter: &Filter, limit: usize) -> FindResult {
    let mut top = TopN::new(limit);
    let (mut total_matches, mut total_alloc) = (0, 0);
    tree.walk(under, |id, _| {
        if filter.matches(tree, id) {
            total_matches += 1;
            total_alloc += tree.alloc(id);
            top.push(tree.alloc(id), id);
            return false;
        }
        // A directory smaller than min_alloc cannot contain a large enough match.
        tree.alloc(id) >= filter.min_alloc
    });
    FindResult {
        matches: top.into_sorted(),
        total_matches,
        total_alloc,
    }
}

/// Case-insensitive (ASCII) glob match supporting `*` and `?`.
pub fn wildcard_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let (mut star, mut star_t) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{Batch, IdAllocator, RootInfo, TreeBuilder};
    use crate::flags;

    /// root: a.log(10) b.LOG(20) node_modules/{x.js(5), node_modules/{y.js(7)}} src/{main.rs(1)}
    fn sample() -> Tree {
        let ids = IdAllocator::default();
        let mut root = Batch::new(0);
        root.base = ids.reserve(4);
        root.push(b"a.log", 10, 10, 100, 0);
        root.push(b"b.LOG", 20, 20, 100, 0);
        root.push(b"node_modules", 0, 0, 50, flags::DIR);
        root.push(b"src", 0, 0, 50, flags::DIR);
        let mut nm = Batch::new(root.base + 2);
        nm.base = ids.reserve(2);
        nm.push(b"x.js", 5, 5, 50, 0);
        nm.push(b"node_modules", 0, 0, 50, flags::DIR);
        let mut nm2 = Batch::new(nm.base + 1);
        nm2.base = ids.reserve(1);
        nm2.push(b"y.js", 7, 7, 50, 0);
        let mut src = Batch::new(root.base + 3);
        src.base = ids.reserve(1);
        src.push(b"main.rs", 1, 1, 500, 0);
        let mut b = TreeBuilder::new(RootInfo::default());
        for batch in [root, nm, nm2, src] {
            b.add(batch);
        }
        b.finish("/r".into(), ids.count())
    }

    fn names(t: &Tree, ids: &[NodeId]) -> Vec<String> {
        ids.iter().map(|&i| t.name_lossy(i).into_owned()).collect()
    }

    #[test]
    fn largest() {
        let t = sample();
        assert_eq!(
            names(&t, &largest_files(&t, Tree::ROOT, 2)),
            ["b.LOG", "a.log"]
        );
        assert_eq!(
            names(&t, &largest_dirs(&t, Tree::ROOT, 5, 1)),
            ["node_modules", "src"]
        );
        assert_eq!(largest_dirs(&t, Tree::ROOT, 5, 9).len(), 3);
    }

    #[test]
    fn extensions() {
        let t = sample();
        let stats = by_extension(&t, Tree::ROOT);
        assert_eq!(
            stats[0],
            ExtStat {
                ext: "log".into(),
                alloc: 30,
                files: 2
            }
        );
        assert_eq!(
            stats[1],
            ExtStat {
                ext: "js".into(),
                alloc: 12,
                files: 2
            }
        );
    }

    #[test]
    fn find_prunes_nested_matches() {
        let t = sample();
        let f = Filter {
            name: Some("node_modules".into()),
            kind: Kind::Dir,
            ..Default::default()
        };
        let r = find(&t, Tree::ROOT, &f, 10);
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.total_alloc, 12);

        let f = Filter {
            name: Some("*.log".into()),
            min_alloc: 15,
            ..Default::default()
        };
        assert_eq!(names(&t, &find(&t, Tree::ROOT, &f, 10).matches), ["b.LOG"]);

        let f = Filter {
            modified_before: Some(200),
            kind: Kind::Dir,
            ..Default::default()
        };
        assert_eq!(
            names(&t, &find(&t, Tree::ROOT, &f, 10).matches),
            ["node_modules"]
        );
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match(b"*.LOG", b"x.log"));
        assert!(wildcard_match(b"a?c*", b"abcdef"));
        assert!(wildcard_match(b"*", b""));
        assert!(!wildcard_match(b"*.rs", b"main.rsx"));
        assert!(wildcard_match(b"*a*b*", b"xxaxxbxx"));
    }
}
