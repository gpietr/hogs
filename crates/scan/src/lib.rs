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
#[cfg(any(windows, test))]
mod ntfs;
#[cfg(windows)]
mod windows_mft;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Backend {
    /// The fastest backend available on this platform.
    #[default]
    Auto,
    /// Portable `std::fs` walker.
    Generic,
    /// Read the NTFS master file table directly (Windows, Administrator only).
    Mft,
}

/// Which backend produced a tree, and why a faster one was not used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport {
    pub backend: &'static str,
    pub fallback: Option<String>,
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
    scan_report(root, opts, progress).map(|(tree, _)| tree)
}

/// Where to scan to answer questions about `dir`. With the NTFS fast path that
/// is the whole drive: reading the MFT costs the same for any folder on it, so
/// one saved scan of the drive then answers every later command (like WizTree).
pub fn preferred_root(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(root) = windows_mft::drive_root_if_fast(dir) {
        return root;
    }
    dir.to_path_buf()
}

/// Like [`scan`], also reporting which backend was used.
pub fn scan_report(
    root: &Path,
    opts: &ScanOptions,
    progress: &Progress,
) -> io::Result<(Tree, ScanReport)> {
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
    let report = |backend, fallback| ScanReport { backend, fallback };
    pool.install(|| match opts.backend {
        Backend::Generic => Ok((
            generic::scan(root, opts, progress)?,
            report("directory walk", None),
        )),
        Backend::Mft => Ok((mft(&root, progress)?, report("NTFS MFT", None))),
        Backend::Auto => {
            #[cfg(windows)]
            let fallback = match mft(&root, progress) {
                Ok(tree) => return Ok((tree, report("NTFS MFT", None))),
                Err(e) => {
                    progress.reset();
                    Some(match e.kind() {
                        io::ErrorKind::PermissionDenied => {
                            "the fast NTFS scan needs Administrator rights".to_string()
                        }
                        _ => format!("the fast NTFS scan is unavailable: {e}"),
                    })
                }
            };
            #[cfg(not(windows))]
            let fallback = None;
            #[cfg(target_os = "linux")]
            let tree = linux::scan(root, opts, progress)?;
            #[cfg(not(target_os = "linux"))]
            let tree = generic::scan(root, opts, progress)?;
            Ok((tree, report("directory walk", fallback)))
        }
    })
}

#[cfg(windows)]
fn mft(root: &Path, progress: &Progress) -> io::Result<Tree> {
    windows_mft::scan(root, progress)
}

#[cfg(not(windows))]
fn mft(_root: &Path, _progress: &Progress) -> io::Result<Tree> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the NTFS MFT backend only exists on Windows",
    ))
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
