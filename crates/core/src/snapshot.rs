//! Saved scans, so separate `hogs` invocations (and the TUI) can reuse a
//! recent scan instead of walking the filesystem again.
//!
//! A snapshot is a little-endian dump of the tree arrays behind a small
//! header. Loading validates every index, so a corrupt file is rejected
//! instead of causing out-of-bounds panics later.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::tree::{NO_PARENT, Tree, os_str_from_bytes};

const MAGIC: &[u8; 8] = b"HOGSNAP\0";
const VERSION: u32 = 1;

/// The part of a snapshot needed to decide whether to load it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub root: PathBuf,
    pub scanned_at: i64,
}

pub fn write(tree: &Tree, w: &mut impl Write) -> io::Result<()> {
    let root = tree.root.as_os_str().as_encoded_bytes();
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&tree.scanned_at.to_le_bytes())?;
    w.write_all(&(root.len() as u32).to_le_bytes())?;
    w.write_all(root)?;
    w.write_all(&(tree.len() as u64).to_le_bytes())?;
    w.write_all(&(tree.names.len() as u64).to_le_bytes())?;
    macro_rules! put {
        ($($v:expr),*) => {$(for x in &$v { w.write_all(&x.to_le_bytes())?; })*};
    }
    put!(
        tree.name_off,
        tree.name_len,
        tree.parent,
        tree.first_child,
        tree.child_count
    );
    put!(tree.size, tree.alloc, tree.mtime, tree.files);
    w.write_all(&tree.flags)?;
    w.write_all(&tree.names)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupt snapshot: {msg}"),
    )
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(invalid("truncated"));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    fn vec<T, const N: usize>(&mut self, n: usize, f: fn([u8; N]) -> T) -> io::Result<Vec<T>> {
        let bytes = self.take(n.checked_mul(N).ok_or_else(|| invalid("size overflow"))?)?;
        Ok(bytes.as_chunks::<N>().0.iter().map(|&c| f(c)).collect())
    }
}

fn read_header(c: &mut Cursor) -> io::Result<Header> {
    if c.take(8)? != MAGIC {
        return Err(invalid("not a hogs snapshot"));
    }
    if u32::from_le_bytes(c.array()?) != VERSION {
        return Err(invalid("unsupported version"));
    }
    let scanned_at = i64::from_le_bytes(c.array()?);
    let len = u32::from_le_bytes(c.array()?) as usize;
    let root = os_str_from_bytes(c.take(len)?).into_owned().into();
    Ok(Header { root, scanned_at })
}

pub fn read(data: &[u8]) -> io::Result<Tree> {
    let mut c = Cursor(data);
    let h = read_header(&mut c)?;
    let n = usize::try_from(u64::from_le_bytes(c.array()?)).map_err(|_| invalid("too large"))?;
    let names_len =
        usize::try_from(u64::from_le_bytes(c.array()?)).map_err(|_| invalid("too large"))?;
    let tree = Tree {
        root: h.root,
        scanned_at: h.scanned_at,
        name_off: c.vec(n, u32::from_le_bytes)?,
        name_len: c.vec(n, u16::from_le_bytes)?,
        parent: c.vec(n, u32::from_le_bytes)?,
        first_child: c.vec(n, u32::from_le_bytes)?,
        child_count: c.vec(n, u32::from_le_bytes)?,
        size: c.vec(n, u64::from_le_bytes)?,
        alloc: c.vec(n, u64::from_le_bytes)?,
        mtime: c.vec(n, i64::from_le_bytes)?,
        files: c.vec(n, u32::from_le_bytes)?,
        flags: c.take(n)?.to_vec(),
        names: c.take(names_len)?.to_vec(),
    };
    if n == 0 || tree.parent[0] != NO_PARENT {
        return Err(invalid("bad root"));
    }
    for i in 0..n {
        let in_names = tree.name_off[i] as usize + tree.name_len[i] as usize <= names_len;
        let children_ok = tree.first_child[i] as usize + tree.child_count[i] as usize <= n;
        // Children always have larger ids than their parent, which also rules out cycles.
        let parent_ok = i == 0 || (tree.parent[i] as usize) < i;
        if !(in_names && children_ok && parent_ok) {
            return Err(invalid("index out of range"));
        }
    }
    // Every child must point back at its parent. With parent < child this makes
    // the child ranges a proper tree, so walks cannot revisit an ancestor.
    for i in 0..n {
        let first = tree.first_child[i] as usize;
        let count = tree.child_count[i] as usize;
        if (first..first + count).any(|c| tree.parent[c] as usize != i) {
            return Err(invalid("child range does not match parents"));
        }
    }
    Ok(tree)
}

/// Writes `target` through a temporary file renamed into place. The temporary
/// file is removed if anything fails, so a full disk leaves nothing behind.
pub(crate) fn write_atomic(
    target: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    let tmp = target.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let mut w = BufWriter::new(File::create(&tmp)?);
        write(&mut w)?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        fs::rename(&tmp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// FNV-1a of a path: stable across Rust versions, unlike `DefaultHasher`.
pub(crate) fn path_hash(path: &Path) -> u64 {
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
}

/// A directory of snapshots, one per scanned root.
pub struct SnapshotStore {
    dir: PathBuf,
}

impl SnapshotStore {
    /// The per-user store: `$HOGS_CACHE_DIR`, else the platform cache directory.
    pub fn open_default() -> io::Result<Self> {
        let dir =
            cache_dir().ok_or_else(|| io::Error::other("cannot determine the cache directory"))?;
        crate::user::create_dir_all(&dir)?;
        Ok(SnapshotStore::at(dir))
    }

    pub fn at(dir: PathBuf) -> Self {
        SnapshotStore { dir }
    }

    fn file_for(&self, root: &Path) -> PathBuf {
        self.dir.join(format!("{:016x}.snap", path_hash(root)))
    }

    /// Saves a tree, dropping snapshots of directories inside it (now redundant).
    pub fn save(&self, tree: &Tree) -> io::Result<()> {
        let target = self.file_for(tree.root_path());
        write_atomic(&target, |w| write(tree, w))?;
        crate::user::give_back(&target);
        for (file, h) in self.headers() {
            if file != target && h.root.starts_with(tree.root_path()) {
                let _ = fs::remove_file(file);
            }
        }
        Ok(())
    }

    /// Headers of all readable snapshots.
    pub fn headers(&self) -> Vec<(PathBuf, Header)> {
        let Ok(dir) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        dir.filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension()? != "snap" {
                return None;
            }
            let mut buf = vec![0; 4096];
            let n = File::open(&path).and_then(|mut f| f.read(&mut buf)).ok()?;
            let header = read_header(&mut Cursor(&buf[..n])).ok()?;
            Some((path, header))
        })
        .collect()
    }

    /// The newest-enough snapshot whose root contains `path`; the deepest such root wins
    /// because it is the smallest to load. Unreadable snapshots are deleted.
    pub fn load_covering(&self, path: &Path, max_age_secs: i64, now: i64) -> Option<Tree> {
        let mut candidates: Vec<_> = self
            .headers()
            .into_iter()
            .filter(|(_, h)| path.starts_with(&h.root) && now - h.scanned_at <= max_age_secs)
            .collect();
        candidates.sort_by_key(|(_, h)| std::cmp::Reverse(h.root.components().count()));
        candidates
            .into_iter()
            .find_map(|(file, _)| match fs::read(&file).and_then(|d| read(&d)) {
                Ok(tree) => Some(tree),
                Err(_) => {
                    let _ = fs::remove_file(&file);
                    None
                }
            })
    }

    /// Drops snapshots that contain any of `paths` or lie inside one of them.
    pub fn invalidate(&self, paths: &[PathBuf]) {
        for (file, h) in self.headers() {
            if paths
                .iter()
                .any(|p| p.starts_with(&h.root) || h.root.starts_with(p))
            {
                let _ = fs::remove_file(file);
            }
        }
    }
}

/// Directory for the hogs cache (`~/.cache/hogs`, `%LOCALAPPDATA%\hogs\cache`, ...).
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HOGS_CACHE_DIR") {
        return Some(dir.into());
    }
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("hogs").join("cache"))
    } else if cfg!(target_os = "macos") {
        crate::user::home().map(|h| h.join("Library/Caches/hogs"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            // Under sudo this belongs to root's session, not the invoking user.
            .filter(|p| p.is_absolute() && crate::user::invoker().is_none())
            .or_else(|| crate::user::home().map(|h| h.join(".cache")))
            .map(|d| d.join("hogs"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{Batch, IdAllocator, RootInfo, TreeBuilder};
    use crate::flags;

    fn tree(root: &str) -> Tree {
        let ids = IdAllocator::default();
        let mut b = Batch::new(0);
        b.base = ids.reserve(2);
        b.push(b"bad\xffname", 10, 4096, 7, 0);
        b.push(b"sub", 0, 4096, 9, flags::DIR);
        let mut sub = Batch::new(b.base + 1);
        sub.base = ids.reserve(1);
        sub.push("zażółć".as_bytes(), 5, 4096, 3, 0);
        let mut builder = TreeBuilder::new(RootInfo {
            alloc: 4096,
            mtime: 1,
        });
        builder.add(b);
        builder.add(sub);
        builder.finish(root.into(), ids.count())
    }

    fn round_trip(t: &Tree) -> Tree {
        let mut buf = Vec::new();
        write(t, &mut buf).unwrap();
        read(&buf).unwrap()
    }

    #[test]
    fn round_trips_exactly() {
        let t = tree("/r");
        let u = round_trip(&t);
        assert_eq!(u.len(), t.len());
        assert_eq!(u.root_path(), t.root_path());
        assert_eq!(u.scanned_at(), t.scanned_at());
        for id in 0..t.len() as u32 {
            assert_eq!(u.name(id), t.name(id));
            assert_eq!(u.path(id), t.path(id));
            assert_eq!(
                (u.size(id), u.alloc(id), u.mtime(id)),
                (t.size(id), t.alloc(id), t.mtime(id))
            );
            assert_eq!(
                (u.files(id), u.flags(id), u.parent(id)),
                (t.files(id), t.flags(id), t.parent(id))
            );
            assert_eq!(u.children(id), t.children(id));
        }
    }

    #[test]
    fn rejects_corruption() {
        let mut buf = Vec::new();
        write(&tree("/r"), &mut buf).unwrap();
        assert!(read(&buf[..buf.len() - 1]).is_err());
        assert!(read(b"nope").is_err());

        let write_bad = |corrupt: fn(&mut Tree)| {
            let mut t = tree("/r");
            corrupt(&mut t);
            let mut buf = Vec::new();
            write(&t, &mut buf).unwrap();
            read(&buf)
        };
        assert!(write_bad(|t| t.parent[1] = 3).is_err());
        assert!(write_bad(|t| t.child_count[0] = 9).is_err());
        assert!(write_bad(|t| t.name_len[1] = 999).is_err());
        // A directory whose child range includes its ancestors would loop walks forever.
        assert!(
            write_bad(|t| {
                t.flags[1] |= crate::flags::DIR;
                t.first_child[1] = 0;
                t.child_count[1] = 2;
            })
            .is_err()
        );
        assert!(write_bad(|_| ()).is_ok());
    }

    #[test]
    fn failed_writes_leave_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.snap");
        let err = write_atomic(&target, |w| {
            w.write_all(b"partial")?;
            Err(io::Error::other("disk full"))
        });
        assert!(err.is_err());
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "temporary file left behind"
        );
        write_atomic(&target, |w| w.write_all(b"ok")).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"ok");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn store_picks_deepest_fresh_and_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::at(dir.path().into());
        let outer = tree("/r");
        let now = outer.scanned_at();
        store.save(&tree("/r/sub")).unwrap();
        store.save(&outer).unwrap();
        // Saving /r dropped the redundant /r/sub snapshot.
        assert_eq!(store.headers().len(), 1);
        store.save(&tree("/r/sub")).unwrap();

        let t = store
            .load_covering(Path::new("/r/sub/x"), 600, now)
            .unwrap();
        assert_eq!(t.root_path(), Path::new("/r/sub"));
        let t = store
            .load_covering(Path::new("/r/other"), 600, now)
            .unwrap();
        assert_eq!(t.root_path(), Path::new("/r"));
        assert!(
            store
                .load_covering(Path::new("/elsewhere"), 600, now)
                .is_none()
        );
        assert!(
            store
                .load_covering(Path::new("/r"), 600, now + 601)
                .is_none()
        );

        store.invalidate(&[PathBuf::from("/r/sub/x")]);
        assert!(store.headers().is_empty());
    }
}
