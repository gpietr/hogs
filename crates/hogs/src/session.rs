//! Resolving paths to scanned trees. Recent scans are reused across
//! invocations through the snapshot cache, so a sequence of `hogs` commands
//! (typically run by an agent) scans the filesystem only once.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use hogs_core::snapshot::SnapshotStore;
use hogs_core::staging;
use hogs_core::{NodeId, Progress, Tree};
use hogs_scan::ScanOptions;

pub struct Session {
    store: Option<SnapshotStore>,
    max_age: i64,
    rescan: bool,
    opts: ScanOptions,
    trees: Vec<Rc<Tree>>,
    /// Backend of the most recent scan, for reporting.
    pub last_backend: &'static str,
}

impl Session {
    pub fn new(max_age: i64, rescan: bool, opts: ScanOptions) -> Self {
        let store = SnapshotStore::open_default()
            .map_err(|e| eprintln!("hogs: scan cache unavailable: {e}"))
            .ok();
        Session::with_store(store, max_age, rescan, opts)
    }

    pub fn with_store(
        store: Option<SnapshotStore>,
        max_age: i64,
        rescan: bool,
        opts: ScanOptions,
    ) -> Self {
        Session {
            store,
            max_age,
            rescan,
            opts,
            trees: Vec::new(),
            last_backend: "",
        }
    }

    /// Scans `dir` and saves the result to the snapshot cache.
    pub fn scan(&mut self, dir: &Path) -> Result<(Rc<Tree>, f64)> {
        let start = Instant::now();
        let (tree, report) = hogs_scan::scan_report(dir, &self.opts, &Progress::default())
            .with_context(|| format!("cannot scan {}", dir.display()))?;
        let secs = start.elapsed().as_secs_f64();
        if let Some(why) = &report.fallback {
            eprintln!("hogs: {why}; using the slower directory walk");
        }
        self.last_backend = report.backend;
        if let Some(store) = &self.store
            && let Err(e) = store.save(&tree)
        {
            eprintln!("hogs: could not save the scan: {e}");
        }
        let tree = Rc::new(tree);
        self.trees
            .retain(|t| !t.root_path().starts_with(tree.root_path()));
        self.trees.push(tree.clone());
        Ok((tree, secs))
    }

    /// The tree and node for `path`: from this process, a fresh snapshot, or a new scan.
    pub fn resolve(&mut self, path: &Path) -> Result<(Rc<Tree>, NodeId)> {
        let path = canonical(path)?;
        let in_memory = self
            .trees
            .iter()
            .filter(|t| path.starts_with(t.root_path()))
            .find_map(|t| Some((t.clone(), t.lookup_scanned(&path)?)));
        if let Some(found) = in_memory {
            return Ok(found);
        }
        if !self.rescan
            && let Some(store) = &self.store
        {
            let now = staging::now();
            if let Some(tree) = store.load_covering(&path, self.max_age, now)
                && let Some(id) = tree.lookup_scanned(&path)
            {
                let age = now - tree.scanned_at();
                if age >= 60 {
                    eprintln!(
                        "hogs: using a scan from {} min ago (--rescan to refresh)",
                        age / 60
                    );
                }
                let tree = Rc::new(tree);
                self.trees.push(tree.clone());
                return Ok((tree, id));
            }
        }
        // Not `is_dir()`: that follows symlinks, but a symlink is an entry of its parent.
        let dir = if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            path.as_path()
        } else {
            path.parent().unwrap_or(&path)
        };
        // With the NTFS fast path this is the whole drive, saved for later commands.
        let (tree, secs) = self.scan(&hogs_scan::preferred_root(dir))?;
        eprintln!(
            "hogs: scanned {} ({} files) in {secs:.2}s",
            tree.root_path().display(),
            tree.files(Tree::ROOT)
        );
        let id = tree
            .lookup(&path)
            .ok_or_else(|| anyhow!("{} was not found in the scan", path.display()))?;
        Ok((tree, id))
    }
}

/// Makes `path` absolute and resolves symlinks in its parent only, so a
/// symlink itself can be inspected and staged.
pub fn canonical(path: &Path) -> Result<PathBuf> {
    let abs = std::path::absolute(path)?;
    let resolved = match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => dunce::canonicalize(parent).map(|d| d.join(name)),
        _ => dunce::canonicalize(&abs),
    };
    resolved
        .ok()
        .filter(|p| p.symlink_metadata().is_ok())
        .ok_or_else(|| anyhow!("{} does not exist", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use hogs_core::flags;

    use super::*;

    fn session(cache: &Path) -> Session {
        Session::with_store(
            Some(SnapshotStore::at(cache.into())),
            600,
            false,
            ScanOptions::default(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_resolve_to_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        fs::create_dir(root.join("target_dir")).unwrap();
        std::os::unix::fs::symlink(root.join("target_dir"), root.join("link")).unwrap();
        let cache = tempfile::tempdir().unwrap();
        let (tree, id) = session(cache.path()).resolve(&root.join("link")).unwrap();
        assert_eq!(tree.path(id), root.join("link"));
        assert!(tree.flags(id) & flags::SYMLINK != 0);
    }

    #[test]
    fn unscanned_mount_points_are_not_covered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("mnt/data")).unwrap();
        fs::write(root.join("mnt/data/file"), vec![0u8; 5000]).unwrap();
        let cache = tempfile::tempdir().unwrap();
        // A saved scan of the parent in which mnt/ looks like an unscanned mount point.
        let store = SnapshotStore::at(cache.path().into());
        let ids = hogs_core::build::IdAllocator::default();
        let mut b = hogs_core::build::Batch::new(0);
        b.base = ids.reserve(1);
        b.push(b"mnt", 0, 4096, 0, flags::DIR | flags::OTHER_FS);
        let mut builder = hogs_core::build::TreeBuilder::new(Default::default());
        builder.add(b);
        store
            .save(&builder.finish(root.clone(), ids.count()))
            .unwrap();

        let (tree, id) = session(cache.path()).resolve(&root.join("mnt")).unwrap();
        // A fresh scan: of mnt/ itself, or of the whole drive with the NTFS fast path.
        assert_eq!(tree.path(id), root.join("mnt"));
        assert!(
            tree.root_path() != root,
            "should not reuse the empty node from the saved scan"
        );
        assert_eq!(tree.files(id), 1);
    }
}
