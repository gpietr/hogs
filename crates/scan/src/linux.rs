//! Linux scanner: raw `getdents64` into a reused buffer plus `statx`
//! relative to the directory fd, so no per-entry path strings are built.

use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Mutex;

use hogs_core::build::{Batch, RootInfo};
use hogs_core::{NodeId, Progress, Tree, flags};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, RawDir, Statx, StatxFlags};

use crate::{ScanOptions, Shared};

const DIRENT_BUF: usize = 64 * 1024;

const STATX_MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::SIZE)
    .union(StatxFlags::BLOCKS)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::INO)
    .union(StatxFlags::NLINK);

/// `f_type` magics of virtual filesystems that are never worth scanning.
const PSEUDO_FS: &[i64] = &[
    0x9fa0,      // proc
    0x6265_6572, // sysfs
    0x1cd1,      // devpts
    0x27e0eb,    // cgroup
    0x6367_7270, // cgroup2
    0x6462_6720, // debugfs
    0x7472_6163, // tracefs
    0x7363_6673, // securityfs
    0x6165_676c, // pstore
    0xcafe_4a11, // bpf
    0x6265_6570, // configfs
    0x6573_5543, // fusectl
    0x0187,      // autofs
    0x1980_0202, // mqueue
    0x4249_4e4d, // binfmt_misc
    0xde5e_81e4, // efivarfs
];

struct Ctx<'a> {
    shared: Shared<'a>,
    root_dev: (u32, u32),
    one_filesystem: bool,
    /// (dev, ino) of multiply-linked files already counted.
    hardlinks: Mutex<HashSet<(u32, u32, u64)>>,
}

pub(crate) fn scan(root: PathBuf, opts: &ScanOptions, progress: &Progress) -> io::Result<Tree> {
    let st = rustix::fs::statx(CWD, &root, AtFlags::empty(), STATX_MASK)?;
    let info = RootInfo {
        alloc: st.stx_blocks * 512,
        mtime: st.stx_mtime.tv_sec,
    };
    let ctx = Ctx {
        shared: Shared::new(progress, info),
        root_dev: (st.stx_dev_major, st.stx_dev_minor),
        one_filesystem: opts.one_filesystem,
        hardlinks: Mutex::new(HashSet::new()),
    };
    let root_bytes = root.as_os_str().as_bytes().to_vec();
    rayon::scope(|s| s.spawn(|s| scan_dir(s, &ctx, root_bytes, Tree::ROOT, false)));
    Ok(ctx.shared.into_tree(root))
}

thread_local! {
    static DIRENT_BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(DIRENT_BUF));
}

/// A subdirectory found while reading a directory, to be scanned next.
struct SubDir {
    index: usize,
    crossed_fs: bool,
}

fn scan_dir<'s>(
    s: &rayon::Scope<'s>,
    ctx: &'s Ctx<'s>,
    path: Vec<u8>,
    id: NodeId,
    crossed_fs: bool,
) {
    let mut batch = Batch::new(id);
    let fd = match rustix::fs::open(
        OsStr::from_bytes(&path),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(_) => {
            batch.dir_flags |= flags::ERROR;
            ctx.shared.finish(batch);
            return;
        }
    };
    if crossed_fs && is_pseudo_fs(&fd) {
        batch.dir_flags |= flags::OTHER_FS;
        ctx.shared.finish(batch);
        return;
    }

    let mut subdirs = Vec::new();
    DIRENT_BUFFER.with_borrow_mut(|buf| {
        let mut iter = RawDir::new(&fd, buf.spare_capacity_mut());
        while let Some(entry) = iter.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    batch.dir_flags |= flags::ERROR;
                    break;
                }
            };
            let name = entry.file_name();
            if is_dot(name) {
                continue;
            }
            if let Some(crossed) = add_entry(ctx, &fd, name, entry.file_type(), &mut batch) {
                subdirs.push(SubDir {
                    index: batch.entries.len() - 1,
                    crossed_fs: crossed,
                });
            }
        }
    });
    drop(fd);

    batch.base = ctx.shared.ids.reserve(batch.entries.len());
    let base = batch.base;
    let mut start = 0;
    let mut child_paths = Vec::with_capacity(subdirs.len());
    let mut next = subdirs.iter().peekable();
    for (i, e) in batch.entries.iter().enumerate() {
        if next.peek().is_some_and(|d| d.index == i) {
            let d = next.next().unwrap();
            let mut child = Vec::with_capacity(path.len() + 1 + (e.name_end - start) as usize);
            child.extend_from_slice(&path);
            if !path.ends_with(b"/") {
                child.push(b'/');
            }
            child.extend_from_slice(&batch.names[start as usize..e.name_end as usize]);
            child_paths.push((child, base + i as NodeId, d.crossed_fs));
        }
        start = e.name_end;
    }
    ctx.shared.finish(batch);
    for (child, child_id, crossed) in child_paths {
        s.spawn(move |s| scan_dir(s, ctx, child, child_id, crossed));
    }
}

/// Stats one entry and appends it to the batch. Returns `Some(crossed_fs)`
/// when the entry is a directory that should be descended into.
fn add_entry(
    ctx: &Ctx,
    dir: &OwnedFd,
    name: &CStr,
    d_type: FileType,
    batch: &mut Batch,
) -> Option<bool> {
    let name_bytes = name.to_bytes();
    let st = match stat_at(dir, name) {
        Ok(st) => st,
        Err(_) => {
            let f = if d_type == FileType::Directory {
                flags::DIR
            } else {
                0
            } | flags::ERROR;
            batch.push(name_bytes, 0, 0, 0, f);
            return None;
        }
    };
    let kind = FileType::from_raw_mode(u32::from(st.stx_mode));
    let mtime = st.stx_mtime.tv_sec;
    let alloc = st.stx_blocks * 512;
    match kind {
        FileType::Directory => {
            let crossed = (st.stx_dev_major, st.stx_dev_minor) != ctx.root_dev;
            if crossed && ctx.one_filesystem {
                batch.push(
                    name_bytes,
                    st.stx_size,
                    alloc,
                    mtime,
                    flags::DIR | flags::OTHER_FS,
                );
                return None;
            }
            batch.push(name_bytes, st.stx_size, alloc, mtime, flags::DIR);
            Some(crossed)
        }
        _ => {
            let mut f = if kind == FileType::Symlink {
                flags::SYMLINK
            } else {
                0
            };
            let (mut size, mut alloc) = (st.stx_size, alloc);
            if st.stx_nlink > 1 {
                let key = (st.stx_dev_major, st.stx_dev_minor, st.stx_ino);
                if !ctx.hardlinks.lock().unwrap().insert(key) {
                    f |= flags::HARDLINK_DUP;
                    (size, alloc) = (0, 0);
                }
            }
            batch.push(name_bytes, size, alloc, mtime, f);
            None
        }
    }
}

fn stat_at(dir: &OwnedFd, name: &CStr) -> rustix::io::Result<Statx> {
    rustix::fs::statx(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC,
        STATX_MASK,
    )
}

fn is_dot(name: &CStr) -> bool {
    matches!(name.to_bytes(), b"." | b"..")
}

fn is_pseudo_fs(fd: &impl AsFd) -> bool {
    #[allow(clippy::unnecessary_cast)] // f_type's integer type varies by architecture
    rustix::fs::fstatfs(fd).is_ok_and(|st| PSEUDO_FS.contains(&(st.f_type as i64)))
}
