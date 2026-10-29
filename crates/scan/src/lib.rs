//! Parallel filesystem scanners producing a [`hogs_core::Tree`].
//!
//! Every scanner walks directories as independent rayon tasks: a task reads
//! one directory, emits a [`hogs_core::build::Batch`] and spawns tasks for its
//! subdirectories. A collector thread builds the tree from the batches as they arrive.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;

use hogs_core::build::{Batch, IdAllocator, RootInfo, TreeBuilder};
use hogs_core::{Progress, Tree};

mod generic;
#[cfg(target_os = "linux")]
mod linux;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Backend {
    /// The fastest backend available on this platform.
    #[default]
    Auto,
    /// Portable `std::fs` walker.
    Generic,
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Do not descend into other filesystems (like `du -x`).
    pub one_filesystem: bool,
    /// Worker threads; 0 picks a default suited to I/O-bound walking.
    pub threads: usize,
    pub backend: Backend,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            one_filesystem: true,
            threads: 0,
            backend: Backend::Auto,
        }
    }
}

/// Scans `root` (a directory) and returns its full tree.
pub fn scan(root: &Path, opts: &ScanOptions, progress: &Progress) -> io::Result<Tree> {
    // dunce: plain `C:\...` paths on Windows rather than `\\?\C:\...` where possible.
    let root = dunce::canonicalize(root)?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", root.display()),
        ));
    }
    let threads = match opts.threads {
        0 => std::thread::available_parallelism()
            .map_or(8, |n| n.get() * 2)
            .clamp(4, 64),
        n => n,
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("hogs-scan-{i}"))
        .build()
        .map_err(io::Error::other)?;
    pool.install(|| match opts.backend {
        #[cfg(target_os = "linux")]
        Backend::Auto => linux::scan(root, opts, progress),
        #[cfg(not(target_os = "linux"))]
        Backend::Auto => generic::scan(root, opts, progress),
        Backend::Generic => generic::scan(root, opts, progress),
    })
}

/// State shared by the tasks of one scan. Finished batches stream to a
/// collector thread that builds the tree while the walk is still running.
struct Shared<'a> {
    ids: IdAllocator,
    tx: Sender<Batch>,
    collector: JoinHandle<TreeBuilder>,
    progress: &'a Progress,
}

impl<'a> Shared<'a> {
    fn new(progress: &'a Progress, root: RootInfo) -> Self {
        let (tx, rx) = mpsc::channel::<Batch>();
        let collector = std::thread::Builder::new()
            .name("hogs-collect".into())
            .spawn(move || {
                let mut builder = TreeBuilder::new(root);
                for batch in rx {
                    builder.add(batch);
                }
                builder
            })
            .expect("failed to spawn collector thread");
        Shared {
            ids: IdAllocator::default(),
            tx,
            collector,
            progress,
        }
    }

    /// Records a finished directory and updates progress counters.
    fn finish(&self, batch: Batch) {
        let (mut files, mut dirs, mut bytes) = (0, 0, 0);
        for e in &batch.entries {
            if e.flags & hogs_core::flags::DIR != 0 {
                dirs += 1;
            } else {
                files += 1;
            }
            bytes += e.alloc;
        }
        let errors = u64::from(batch.dir_flags & hogs_core::flags::ERROR != 0);
        self.progress.add(files, dirs, bytes, errors);
        self.tx.send(batch).expect("collector thread exited early");
    }

    /// Called once the walk is done.
    fn into_tree(self, root: PathBuf) -> Tree {
        let count = self.ids.count();
        drop(self.tx);
        let builder = self.collector.join().expect("collector thread panicked");
        builder.finish(root, count)
    }
}
