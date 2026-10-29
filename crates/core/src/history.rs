//! Past scans, kept to show what grows over time.
//!
//! Entries are pruned and compressed, so a scan of a home directory takes well
//! under a megabyte. Every directory is kept with its exact totals, but only
//! files of at least [`MIN_FILE`]. Old entries are thinned out: the latest few
//! are kept, then one per day for a month and one per week for half a year.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

use crate::snapshot::{self, path_hash};
use crate::tree::{Tree, os_str_from_bytes};

/// Files smaller than this are not kept individually (their directories still count them).
pub const MIN_FILE: u64 = 1 << 20;

const MAGIC: &[u8; 8] = b"HOGSHIST";
const VERSION: u32 = 1;
const KEEP_LATEST: usize = 3;
const DAY: i64 = 86_400;
const DAILY_FOR: i64 = 31 * DAY;
const WEEKLY_FOR: i64 = 183 * DAY;

/// One past scan, read from its header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub file: PathBuf,
    pub root: PathBuf,
    pub scanned_at: i64,
    /// Totals of the scanned root.
    pub alloc: u64,
    pub size: u64,
    pub files: u64,
    /// Size of the history file itself.
    pub bytes: u64,
}

pub struct History {
    dir: PathBuf,
}

impl History {
    /// `history/` in the cache directory (see [`snapshot::cache_dir`]).
    pub fn open_default() -> io::Result<Self> {
        let dir = snapshot::cache_dir()
            .ok_or_else(|| io::Error::other("cannot determine the cache directory"))?
            .join("history");
        crate::user::create_dir_all(&dir)?;
        Ok(History::at(dir))
    }

    pub fn at(dir: PathBuf) -> Self {
        History { dir }
    }

    /// Saves a scan, then thins out older ones of the same root.
    pub fn record(&self, tree: &Tree) -> io::Result<()> {
        let pruned = tree.pruned(|id| tree.is_dir(id) || tree.alloc(id) >= MIN_FILE);
        let root = tree.root_path();
        let name = format!("{:016x}-{}.hist", path_hash(root), tree.scanned_at());
        let target = self.dir.join(name);
        let tmp = target.with_extension(format!("tmp{}", std::process::id()));
        let mut out = io::BufWriter::new(File::create(&tmp)?);
        let root_bytes = root.as_os_str().as_encoded_bytes();
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        out.write_all(&tree.scanned_at().to_le_bytes())?;
        for n in [
            tree.alloc(Tree::ROOT),
            tree.size(Tree::ROOT),
            u64::from(tree.files(Tree::ROOT)),
        ] {
            out.write_all(&n.to_le_bytes())?;
        }
        out.write_all(&(root_bytes.len() as u32).to_le_bytes())?;
        out.write_all(root_bytes)?;
        let mut body = DeflateEncoder::new(out, Compression::fast());
        snapshot::write(&pruned, &mut body)?;
        body.finish()?
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        fs::rename(&tmp, &target)?;
        crate::user::give_back(&target);
        self.thin(root, crate::staging::now());
        Ok(())
    }

    /// All entries, newest first.
    pub fn entries(&self) -> Vec<Entry> {
        let Ok(dir) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut entries: Vec<Entry> = dir
            .filter_map(|e| {
                let path = e.ok()?.path();
                if path.extension()? != "hist" {
                    return None;
                }
                let mut buf = vec![0; 4096];
                let mut f = File::open(&path).ok()?;
                let n = f.read(&mut buf).ok()?;
                let bytes = f.metadata().ok()?.len();
                read_header(&buf[..n]).map(|(h, _)| Entry {
                    file: path,
                    bytes,
                    ..h
                })
            })
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.scanned_at));
        entries
    }

    /// Entries whose root contains `path`, newest first.
    pub fn covering(&self, path: &Path) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| path.starts_with(&e.root))
            .collect()
    }

    pub fn load(&self, entry: &Entry) -> io::Result<Tree> {
        let data = fs::read(&entry.file)?;
        let (_, body) = read_header(&data).ok_or_else(|| invalid("bad header"))?;
        let mut raw = Vec::new();
        DeflateDecoder::new(&data[body..]).read_to_end(&mut raw)?;
        snapshot::read(&raw)
    }

    /// Deletes every entry; returns how many there were.
    pub fn clear(&self) -> io::Result<usize> {
        let entries = self.entries();
        for e in &entries {
            fs::remove_file(&e.file)?;
        }
        Ok(entries.len())
    }

    fn thin(&self, root: &Path, now: i64) {
        let entries: Vec<Entry> = self
            .entries()
            .into_iter()
            .filter(|e| e.root == root)
            .collect();
        let times: Vec<i64> = entries.iter().map(|e| e.scanned_at).collect();
        for i in to_thin(&times, now) {
            let _ = fs::remove_file(&entries[i].file);
        }
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("corrupt history entry: {msg}"),
    )
}

/// Parses the uncompressed header; returns it and where the compressed body starts.
fn read_header(b: &[u8]) -> Option<(Entry, usize)> {
    let u64_at = |o: usize| Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?));
    if b.get(0..8)? != MAGIC || u32::from_le_bytes(b.get(8..12)?.try_into().ok()?) != VERSION {
        return None;
    }
    let scanned_at = u64_at(12)? as i64;
    let (alloc, size, files) = (u64_at(20)?, u64_at(28)?, u64_at(36)?);
    let len = u32::from_le_bytes(b.get(44..48)?.try_into().ok()?) as usize;
    let root = os_str_from_bytes(b.get(48..48 + len)?).into_owned().into();
    let entry = Entry {
        file: PathBuf::new(),
        root,
        scanned_at,
        alloc,
        size,
        files,
        bytes: 0,
    };
    Some((entry, 48 + len))
}

/// Which entries to delete, given their scan times newest first: keep the
/// latest few, then the newest per day for a month and per week for half a
/// year.
fn to_thin(times: &[i64], now: i64) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut delete = Vec::new();
    for (i, &t) in times.iter().enumerate() {
        let age = now - t;
        let bucket = if i < KEEP_LATEST {
            None
        } else if age <= DAILY_FOR {
            Some((0, t.div_euclid(DAY)))
        } else if age <= WEEKLY_FOR {
            Some((1, t.div_euclid(7 * DAY)))
        } else {
            delete.push(i);
            continue;
        };
        if let Some(b) = bucket
            && !seen.insert(b)
        {
            delete.push(i);
        }
    }
    delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{Batch, IdAllocator, RootInfo, TreeBuilder};
    use crate::flags;

    fn tree(root: &str, big: u64) -> Tree {
        let ids = IdAllocator::default();
        let mut b = Batch::new(0);
        b.base = ids.reserve(3);
        b.push(b"big.iso", big, big, 1, 0);
        b.push(b"small.txt", 10, 4096, 2, 0);
        b.push(b"sub", 0, 4096, 3, flags::DIR);
        let mut sub = Batch::new(b.base + 2);
        sub.base = ids.reserve(1);
        sub.push(b"tiny", 5, 4096, 4, 0);
        let mut builder = TreeBuilder::new(RootInfo::default());
        builder.add(b);
        builder.add(sub);
        builder.finish(root.into(), ids.count())
    }

    #[test]
    fn records_pruned_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::at(dir.path().into());
        let t = tree("/r", 5 << 20);
        h.record(&t).unwrap();
        let entries = h.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].root, Path::new("/r"));
        assert_eq!(entries[0].alloc, t.alloc(Tree::ROOT));
        let old = h.load(&entries[0]).unwrap();
        // Small files are left out, but every total is unchanged.
        assert!(old.lookup(Path::new("/r/small.txt")).is_none());
        assert!(old.lookup(Path::new("/r/sub/tiny")).is_none());
        assert!(old.lookup(Path::new("/r/big.iso")).is_some());
        let sub = old.lookup(Path::new("/r/sub")).unwrap();
        assert_eq!(old.alloc(sub), 8192);
        assert_eq!(old.alloc(Tree::ROOT), t.alloc(Tree::ROOT));
        assert_eq!(old.files(Tree::ROOT), 3);
        assert_eq!(h.covering(Path::new("/r/sub")).len(), 1);
        assert!(h.covering(Path::new("/other")).is_empty());
        assert_eq!(h.clear().unwrap(), 1);
    }

    #[test]
    fn thinning() {
        let now = 1000 * DAY;
        let hours = |h: i64| now - h * 3600;
        // Newest first: 5 scans today, 2 yesterday, one 40 days ago twice in a week, one a year ago.
        let times = [
            hours(1),
            hours(2),
            hours(3),
            hours(4),
            hours(5),
            hours(30),
            hours(31),
            now - 40 * DAY,
            now - 41 * DAY,
            now - 365 * DAY,
        ];
        let delete = to_thin(&times, now);
        // Kept: 3 latest, the newest remaining of today (index 3), newest of yesterday (5),
        // one of the 40/41-day week (7 or 8 depending on week boundaries).
        assert!(!delete.contains(&0) && !delete.contains(&1) && !delete.contains(&2));
        assert!(!delete.contains(&3) && delete.contains(&4), "{delete:?}");
        assert!(!delete.contains(&5) && delete.contains(&6), "{delete:?}");
        assert!(delete.contains(&9), "a year old");
    }
}
