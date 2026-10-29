//! Portable scanner built on `std::fs`. Used on platforms without a
//! dedicated backend, and as the Windows fallback when the MFT is unavailable
//! (on Windows `DirEntry::metadata` comes from the directory listing itself).

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use hogs_core::build::{Batch, RootInfo};
use hogs_core::{NodeId, Progress, Tree, flags};

use crate::{ScanOptions, Shared};

struct Ctx<'a> {
    shared: Shared<'a>,
    root_dev: u64,
    one_filesystem: bool,
    hardlinks: Mutex<HashSet<(u64, u64)>>,
}

pub(crate) fn scan(root: PathBuf, opts: &ScanOptions, progress: &Progress) -> io::Result<Tree> {
    let meta = fs::metadata(&root)?;
    let info = RootInfo {
        alloc: alloc_size(&meta),
        mtime: mtime(&meta),
    };
    let ctx = Ctx {
        shared: Shared::new(progress, info),
        root_dev: dev(&meta),
        one_filesystem: opts.one_filesystem,
        hardlinks: Mutex::new(HashSet::new()),
    };
    rayon::scope(|s| {
        let root = root.clone();
        s.spawn(|s| scan_dir(s, &ctx, root, Tree::ROOT));
    });
    Ok(ctx.shared.into_tree(root))
}

fn scan_dir<'s>(s: &rayon::Scope<'s>, ctx: &'s Ctx<'s>, path: PathBuf, id: NodeId) {
    let mut batch = Batch::new(id);
    let mut subdirs = Vec::new();
    match fs::read_dir(&path) {
        Ok(rd) => {
            for entry in rd {
                let Ok(entry) = entry else {
                    batch.dir_flags |= flags::ERROR;
                    break;
                };
                let name = entry.file_name();
                let name = name.as_encoded_bytes();
                let Ok(meta) = entry.metadata() else {
                    batch.push(name, 0, 0, 0, flags::ERROR);
                    continue;
                };
                let ft = meta.file_type();
                let (size, alloc, mtime) = (meta.len(), alloc_size(&meta), mtime(&meta));
                if ft.is_dir() {
                    if ctx.one_filesystem && dev(&meta) != ctx.root_dev {
                        batch.push(name, size, alloc, mtime, flags::DIR | flags::OTHER_FS);
                    } else {
                        batch.push(name, size, alloc, mtime, flags::DIR);
                        subdirs.push((batch.entries.len() - 1, entry.path()));
                    }
                } else {
                    let mut f = if ft.is_symlink() { flags::SYMLINK } else { 0 };
                    let (mut size, mut alloc) = (size, alloc);
                    if let Some(key) = hardlink_key(&meta)
                        && !ctx.hardlinks.lock().unwrap().insert(key)
                    {
                        f |= flags::HARDLINK_DUP;
                        (size, alloc) = (0, 0);
                    }
                    batch.push(name, size, alloc, mtime, f);
                }
            }
        }
        Err(_) => batch.dir_flags |= flags::ERROR,
    }
    batch.base = ctx.shared.ids.reserve(batch.entries.len());
    let base = batch.base;
    ctx.shared.finish(batch);
    for (i, child) in subdirs {
        s.spawn(move |s| scan_dir(s, ctx, child, base + i as NodeId));
    }
}

fn mtime(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(unix)]
fn alloc_size(meta: &Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::blocks(meta) * 512
}

#[cfg(not(unix))]
fn alloc_size(meta: &Metadata) -> u64 {
    meta.len()
}

#[cfg(unix)]
fn dev(meta: &Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::dev(meta)
}

#[cfg(not(unix))]
fn dev(_meta: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn hardlink_key(meta: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn hardlink_key(_meta: &Metadata) -> Option<(u64, u64)> {
    None
}
