//! Parsing NTFS master file table (MFT) records, just enough for disk usage:
//! names and parents, sizes, allocated sizes, modification times, hardlinks
//! and reparse points. Platform independent, so it is tested everywhere; the
//! Windows I/O lives in `windows_mft.rs`.
//!
//! Every read is bounds-checked: the MFT comes from a live volume and may
//! contain torn or garbage records, which are skipped rather than trusted.

use std::path::PathBuf;

use hogs_core::build::{Batch, IdAllocator, RootInfo, TreeBuilder};
use hogs_core::{Tree, flags};

/// Record number of the root directory.
pub const ROOT_RECORD: u32 = 5;
/// MFT fixups protect each 512-byte block of a record, whatever the sector size.
const FIXUP_BLOCK: usize = 512;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const NAMESPACE_DOS: u8 = 2;
const REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// A run of clusters from a non-resident attribute's mapping pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// First cluster on the volume; `None` for a sparse (unallocated) run.
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// Decodes mapping pairs (data runs).
pub fn parse_runs(data: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut pos = 0;
    let mut lcn: i64 = 0;
    loop {
        let header = *data.get(pos)?;
        if header == 0 {
            return Some(runs);
        }
        let (len_size, off_size) = ((header & 0x0F) as usize, (header >> 4) as usize);
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return None;
        }
        let len_bytes = data.get(pos + 1..pos + 1 + len_size)?;
        let clusters = len_bytes
            .iter()
            .rev()
            .fold(0u64, |acc, &b| acc << 8 | u64::from(b));
        let off_bytes = data.get(pos + 1 + len_size..pos + 1 + len_size + off_size)?;
        let run_lcn = if off_size == 0 {
            None
        } else {
            // Signed, relative to the previous run's LCN.
            let mut delta = off_bytes
                .iter()
                .rev()
                .fold(0i64, |acc, &b| acc << 8 | i64::from(b));
            let shift = 64 - 8 * off_size as u32;
            delta = (delta << shift) >> shift;
            lcn = lcn.checked_add(delta)?;
            Some(u64::try_from(lcn).ok()?)
        };
        runs.push(Run {
            lcn: run_lcn,
            clusters,
        });
        pos += 1 + len_size + off_size;
    }
}

/// Applies the update sequence array: the last two bytes of every 512-byte
/// block are stored in the array and replaced by a check value. Returns false
/// for records that are not FILE records or are torn (check value mismatch).
pub fn apply_fixups(rec: &mut [u8]) -> bool {
    if rec.get(0..4) != Some(b"FILE") {
        return false;
    }
    let (Some(usa_off), Some(usa_count)) = (u16_at(rec, 4), u16_at(rec, 6)) else {
        return false;
    };
    let (usa_off, usa_count) = (usa_off as usize, usa_count as usize);
    if usa_count == 0
        || usa_count - 1 > rec.len() / FIXUP_BLOCK
        || usa_off + usa_count * 2 > rec.len()
    {
        return false;
    }
    let check = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * FIXUP_BLOCK;
        if rec[end - 2..end] != check {
            return false;
        }
        let saved = [rec[usa_off + 2 * i], rec[usa_off + 2 * i + 1]];
        rec[end - 2..end].copy_from_slice(&saved);
    }
    true
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub parent: u32,
    pub parent_seq: u16,
    /// WTF-8, matching `OsStr::as_encoded_bytes` on Windows.
    pub name: Vec<u8>,
}

/// What one FILE record contributes. Extension records carry attributes of
/// their base record and are merged into it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// Base record number for extension records, `None` for base records.
    pub base: Option<u32>,
    pub sequence: u16,
    pub is_dir: bool,
    /// Symlink, junction or volume mount point: never descended into.
    pub is_link: bool,
    pub names: Vec<Name>,
    /// Apparent size of the unnamed data stream.
    pub size: Option<u64>,
    /// Allocated bytes: non-resident streams (compressed size when compressed
    /// or sparse) plus directory index allocations.
    pub alloc: u64,
    /// Modification time (Unix seconds) from $STANDARD_INFORMATION.
    pub mtime: Option<i64>,
}

/// Parses a FILE record whose fixups were applied. `None` for records that are
/// not in use or malformed.
pub fn parse_record(rec: &[u8], cluster_size: u64) -> Option<Record> {
    let flags = u16_at(rec, 22)?;
    if flags & 0x01 == 0 {
        return None;
    }
    let base = u64_at(rec, 32)? & 0x0000_FFFF_FFFF_FFFF;
    let mut out = Record {
        base: (base != 0).then_some(base as u32),
        sequence: u16_at(rec, 16)?,
        is_dir: flags & 0x02 != 0,
        ..Default::default()
    };
    for_each_attribute(rec, |kind, attr| {
        parse_attribute(kind, attr, cluster_size, &mut out)
    })?;
    Some(out)
}

/// Calls `f(type, attribute)` for each attribute; `None` if the record is
/// malformed or `f` returns `None`.
fn for_each_attribute(rec: &[u8], mut f: impl FnMut(u32, &[u8]) -> Option<()>) -> Option<()> {
    let used = (u32_at(rec, 24)? as usize).min(rec.len());
    let mut pos = u16_at(rec, 20)? as usize;
    while pos + 16 <= used {
        let kind = u32_at(rec, pos)?;
        if kind == ATTR_END {
            break;
        }
        let len = u32_at(rec, pos + 4)? as usize;
        if len < 16 || pos + len > used {
            return None;
        }
        f(kind, &rec[pos..pos + len])?;
        pos += len;
    }
    Some(())
}

/// Data runs of the unnamed $DATA stream of a record whose fixups were
/// applied. Used on record 0, which describes where the MFT itself lives.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn mft_data_runs(rec: &[u8]) -> Option<Vec<Run>> {
    let mut runs = None;
    for_each_attribute(rec, |kind, attr| {
        if runs.is_none()
            && kind == ATTR_DATA
            && attr[8] != 0
            && attr[9] == 0
            && u64_at(attr, 16)? == 0
        {
            let pairs = u16_at(attr, 32)? as usize;
            runs = Some(parse_runs(attr.get(pairs..)?)?);
        }
        Some(())
    })?;
    runs
}

fn parse_attribute(kind: u32, attr: &[u8], cluster_size: u64, out: &mut Record) -> Option<()> {
    let non_resident = attr[8] != 0;
    let name_len = attr[9];
    if non_resident {
        if !matches!(kind, ATTR_DATA | ATTR_INDEX_ALLOCATION) {
            return Some(());
        }
        // Sizes are only valid in the first segment of a multi-segment attribute.
        let first_segment = u64_at(attr, 16)? == 0;
        if kind == ATTR_DATA && name_len == 0 && first_segment {
            out.size = Some(u64_at(attr, 48)?);
        }
        // On-disk size from the clusters that really exist. The allocated-size
        // field includes sparse ranges ($BadClus:$Bad spans the whole volume and
        // is not even flagged sparse), and each segment lists its own runs.
        let pairs = u16_at(attr, 32)? as usize;
        out.alloc += match attr.get(pairs..).and_then(parse_runs) {
            Some(runs) => runs
                .iter()
                .filter(|r| r.lcn.is_some())
                .map(|r| r.clusters)
                .sum::<u64>()
                .saturating_mul(cluster_size),
            None if first_segment => u64_at(attr, 40)?,
            None => 0,
        };
        return Some(());
    }
    let value_len = u32_at(attr, 16)? as usize;
    let value_off = u16_at(attr, 20)? as usize;
    let value = attr.get(value_off..value_off + value_len)?;
    match kind {
        ATTR_STANDARD_INFORMATION => {
            let filetime = u64_at(value, 8)? as i64;
            out.mtime = Some(filetime / 10_000_000 - FILETIME_UNIX_OFFSET);
        }
        ATTR_FILE_NAME => {
            let parent_ref = u64_at(value, 0)?;
            let chars = *value.get(64)? as usize;
            let namespace = *value.get(65)?;
            if namespace != NAMESPACE_DOS {
                let units: Vec<u16> = value
                    .get(66..66 + chars * 2)?
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                out.names.push(Name {
                    parent: (parent_ref & 0xFFFF_FFFF) as u32,
                    parent_seq: (parent_ref >> 48) as u16,
                    name: wtf8(&units),
                });
            }
        }
        ATTR_DATA if name_len == 0 => out.size = Some(value_len as u64),
        ATTR_REPARSE_POINT => {
            let tag = u32_at(value, 0)?;
            out.is_link |= tag == REPARSE_TAG_SYMLINK || tag == REPARSE_TAG_MOUNT_POINT;
        }
        _ => {}
    }
    Some(())
}

/// UTF-16 to WTF-8: like UTF-8, but unpaired surrogates are kept (encoded as
/// three bytes), exactly as Rust's `OsStr` stores Windows names.
pub fn wtf8(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len());
    for c in char::decode_utf16(units.iter().copied()) {
        match c {
            Ok(c) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            Err(e) => {
                let u = u32::from(e.unpaired_surrogate());
                out.extend([
                    0xE0 | (u >> 12) as u8,
                    0x80 | ((u >> 6) & 0x3F) as u8,
                    0x80 | (u & 0x3F) as u8,
                ]);
            }
        }
    }
    out
}

const IN_USE: u8 = 1;
const DIR: u8 = 2;
const LINK: u8 = 4;

struct Link {
    record: u32,
    parent: u32,
    parent_seq: u16,
    name: std::ops::Range<usize>,
}

/// All records of a volume, merged from base and extension records.
#[derive(Default)]
pub struct Mft {
    seq: Vec<u16>,
    state: Vec<u8>,
    size: Vec<u64>,
    alloc: Vec<u64>,
    mtime: Vec<i64>,
    links: Vec<Link>,
    names: Vec<u8>,
}

impl Mft {
    pub fn with_capacity(records: usize) -> Self {
        Mft {
            seq: vec![0; records],
            state: vec![0; records],
            size: vec![0; records],
            alloc: vec![0; records],
            mtime: vec![0; records],
            ..Default::default()
        }
    }

    fn grow(&mut self, len: usize) {
        if len > self.state.len() {
            self.seq.resize(len, 0);
            self.state.resize(len, 0);
            self.size.resize(len, 0);
            self.alloc.resize(len, 0);
            self.mtime.resize(len, 0);
        }
    }

    pub fn add(&mut self, index: u32, rec: Record) {
        let target = rec.base.unwrap_or(index);
        let t = target as usize;
        self.grow(t + 1);
        if rec.base.is_none() {
            self.seq[t] = rec.sequence;
            self.state[t] |= IN_USE | if rec.is_dir { DIR } else { 0 };
        }
        if rec.is_link {
            self.state[t] |= LINK;
        }
        if let Some(size) = rec.size {
            self.size[t] = size;
        }
        if let Some(mtime) = rec.mtime {
            self.mtime[t] = mtime;
        }
        self.alloc[t] += rec.alloc;
        for n in rec.names {
            let start = self.names.len();
            self.names.extend_from_slice(&n.name);
            self.links.push(Link {
                record: target,
                parent: n.parent,
                parent_seq: n.parent_seq,
                name: start..self.names.len(),
            });
        }
    }

    fn is(&self, record: u32, bit: u8) -> bool {
        self.state
            .get(record as usize)
            .is_some_and(|s| s & bit != 0)
    }

    /// Link indices grouped by parent: `children(p)` lists the links inside directory `p`.
    fn children_index(&self) -> (Vec<u32>, Vec<u32>) {
        let valid = |l: &Link| {
            l.record != l.parent
                && self.is(l.record, IN_USE)
                && self.is(l.parent, DIR)
                // A reused parent record means the link is stale.
                && self.seq.get(l.parent as usize) == Some(&l.parent_seq)
        };
        let mut order: Vec<u32> = (0..self.links.len() as u32)
            .filter(|&i| valid(&self.links[i as usize]))
            .collect();
        order.sort_by_key(|&i| self.links[i as usize].parent);
        let mut start = vec![0u32; self.state.len() + 1];
        for &i in &order {
            start[self.links[i as usize].parent as usize + 1] += 1;
        }
        for p in 0..self.state.len() {
            start[p + 1] += start[p];
        }
        (start, order)
    }

    /// Builds the tree below `root_path`, which must be `C:\` or a directory on
    /// the volume (matched case-insensitively, as NTFS does).
    pub fn build_tree(&self, root_path: PathBuf, rel: &[Vec<u8>]) -> Option<Tree> {
        let (start, order) = self.children_index();
        let children = |p: u32| {
            let p = p as usize;
            order[start[p] as usize..start[p + 1] as usize]
                .iter()
                .map(|&i| &self.links[i as usize])
        };
        let mut root = ROOT_RECORD;
        for comp in rel {
            let want = lowercase(comp);
            root = children(root)
                .find(|l| lowercase(&self.names[l.name.clone()]) == want)?
                .record;
            if !self.is(root, DIR) || self.is(root, LINK) {
                return None;
            }
        }

        let ids = IdAllocator::default();
        let info = RootInfo {
            alloc: self.alloc[root as usize],
            mtime: self.mtime[root as usize],
        };
        let mut builder = TreeBuilder::new(info);
        let mut seen = vec![false; self.state.len()];
        seen[root as usize] = true;
        let mut queue = std::collections::VecDeque::from([(root, Tree::ROOT)]);
        while let Some((dir, id)) = queue.pop_front() {
            let kids: Vec<&Link> = children(dir).collect();
            let mut batch = Batch::new(id);
            batch.base = ids.reserve(kids.len());
            for (i, l) in kids.iter().enumerate() {
                let r = l.record as usize;
                let link = self.is(l.record, LINK);
                let is_dir = self.is(l.record, DIR) && !link;
                // Hardlinks: the first link reached carries the size, like on Linux.
                let dup = std::mem::replace(&mut seen[r], true);
                let mut f = if is_dir { flags::DIR } else { 0 };
                if link {
                    f |= flags::SYMLINK;
                }
                let (size, alloc) = if dup {
                    f |= flags::HARDLINK_DUP;
                    (0, 0)
                } else {
                    (self.size[r], self.alloc[r])
                };
                batch.push(&self.names[l.name.clone()], size, alloc, self.mtime[r], f);
                if is_dir && !dup {
                    queue.push_back((l.record, batch.base + i as u32));
                }
            }
            builder.add(batch);
        }
        Some(builder.finish(root_path, ids.count()))
    }
}

/// Case folding for name lookups (an approximation of NTFS's $UpCase table).
fn lowercase(name: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(name) {
        Ok(s) => s.to_lowercase().into_bytes(),
        Err(_) => name.to_ascii_lowercase(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a valid 1024-byte FILE record with the given attributes and fixups applied in reverse
    /// (as stored on disk).
    pub fn record(sequence: u16, is_dir: bool, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&48u16.to_le_bytes()); // update sequence offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // 1 check value + 2 blocks
        r[16..18].copy_from_slice(&sequence.to_le_bytes());
        r[20..22].copy_from_slice(&56u16.to_le_bytes()); // first attribute
        r[22..24].copy_from_slice(&(1u16 | if is_dir { 2 } else { 0 }).to_le_bytes());
        r[32..40].copy_from_slice(&base.to_le_bytes());
        let mut pos = 56;
        for a in attrs {
            r[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        r[pos..pos + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        r[24..28].copy_from_slice(&((pos + 8) as u32).to_le_bytes());
        // Store the fixups: move each block's last two bytes into the array.
        let check = [0xAB, 0xCD];
        r[48..50].copy_from_slice(&check);
        for i in 1..3 {
            let end = i * 512;
            let saved = [r[end - 2], r[end - 1]];
            r[48 + 2 * i..50 + 2 * i].copy_from_slice(&saved);
            r[end - 2..end].copy_from_slice(&check);
        }
        r
    }

    fn resident(kind: u32, name_len: u8, value: &[u8]) -> Vec<u8> {
        let len = (24 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = name_len;
        a[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[20..22].copy_from_slice(&24u16.to_le_bytes());
        a[24..24 + value.len()].copy_from_slice(value);
        a
    }

    const CLUSTER: u64 = 4096;

    /// A non-resident attribute with the given mapping pairs (data runs).
    fn non_resident(kind: u32, name_len: u8, lowest_vcn: u64, runs: &[u8], size: u64) -> Vec<u8> {
        let len = (72 + runs.len() + 1).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[9] = name_len;
        a[16..24].copy_from_slice(&lowest_vcn.to_le_bytes());
        a[32..34].copy_from_slice(&72u16.to_le_bytes());
        // The allocated-size field is deliberately bogus: on-disk size must come from the runs.
        a[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        a[48..56].copy_from_slice(&size.to_le_bytes());
        a[72..72 + runs.len()].copy_from_slice(runs);
        a
    }

    pub fn file_name(parent: u32, parent_seq: u16, name: &str, namespace: u8) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 66 + units.len() * 2];
        v[0..8].copy_from_slice(&(u64::from(parent) | u64::from(parent_seq) << 48).to_le_bytes());
        v[64] = units.len() as u8;
        v[65] = namespace;
        for (i, u) in units.iter().enumerate() {
            v[66 + 2 * i..68 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, 0, &v)
    }

    fn std_info(unix_secs: i64) -> Vec<u8> {
        let ft = ((unix_secs + FILETIME_UNIX_OFFSET) * 10_000_000) as u64;
        let mut v = vec![0u8; 48];
        v[8..16].copy_from_slice(&ft.to_le_bytes());
        resident(ATTR_STANDARD_INFORMATION, 0, &v)
    }

    fn parse(mut r: Vec<u8>) -> Record {
        assert!(apply_fixups(&mut r));
        parse_record(&r, CLUSTER).expect("valid record")
    }

    #[test]
    fn data_runs() {
        // 0x21: 1-byte length, 2-byte offset. Second run goes backwards; third is sparse.
        let runs = parse_runs(&[
            0x21, 0x10, 0x00, 0x01, 0x21, 0x08, 0xF0, 0xFF, 0x01, 0x04, 0x00,
        ])
        .unwrap();
        assert_eq!(
            runs,
            [
                Run {
                    lcn: Some(0x100),
                    clusters: 0x10
                },
                Run {
                    lcn: Some(0x100 - 0x10),
                    clusters: 8
                },
                Run {
                    lcn: None,
                    clusters: 4
                },
            ]
        );
        assert!(parse_runs(&[0x21, 0x10]).is_none());
    }

    #[test]
    fn finds_the_mft_runs() {
        let data = non_resident(ATTR_DATA, 0, 0, &[0x11, 0x40, 0x10], 1 << 20); // 0x40 clusters at LCN 0x10
        let mut r = record(1, false, 0, &[file_name(5, 5, "$MFT", 3), data]);
        assert!(apply_fixups(&mut r));
        assert_eq!(
            mft_data_runs(&r),
            Some(vec![Run {
                lcn: Some(0x10),
                clusters: 0x40
            }])
        );
    }

    #[test]
    fn fixups_detect_torn_records() {
        let mut ok = record(1, false, 0, &[]);
        assert!(apply_fixups(&mut ok));
        let mut torn = record(1, false, 0, &[]);
        torn[510] = 0; // a block written only partly
        assert!(!apply_fixups(&mut torn));
        assert!(!apply_fixups(&mut vec![0u8; 1024]));
    }

    #[test]
    fn parses_files() {
        let r = parse(record(
            7,
            false,
            0,
            &[
                std_info(1_700_000_000),
                file_name(5, 5, "PROGRA~1", NAMESPACE_DOS),
                file_name(5, 5, "Program file.txt", 1),
                non_resident(ATTR_DATA, 0, 0, &[0x11, 0x02, 0x10], 5000),
                // An alternate data stream adds to the allocation but not the size.
                non_resident(ATTR_DATA, 4, 0, &[0x11, 0x01, 0x20], 100),
            ],
        ));
        assert_eq!(r.sequence, 7);
        assert_eq!(r.mtime, Some(1_700_000_000));
        assert_eq!(r.names.len(), 1, "DOS names are skipped");
        assert_eq!(r.names[0].name, b"Program file.txt");
        assert_eq!((r.size, r.alloc), (Some(5000), 8192 + 4096));

        // Compressed: 3 real clusters, the rest of the compression unit sparse.
        let compressed = parse(record(
            1,
            false,
            0,
            &[non_resident(
                ATTR_DATA,
                0,
                0,
                &[0x11, 0x03, 0x20, 0x01, 0x0D],
                60000,
            )],
        ));
        assert_eq!((compressed.size, compressed.alloc), (Some(60000), 12288));
        // $BadClus:$Bad: a named stream as large as the volume, all sparse.
        let bad = parse(record(
            1,
            false,
            0,
            &[non_resident(
                ATTR_DATA,
                4,
                0,
                &[0x04, 0x00, 0x00, 0x00, 0x08],
                0,
            )],
        ));
        assert_eq!((bad.size, bad.alloc), (None, 0));
        // A data stream split across two segments: both count, the size comes from the first.
        let first = parse(record(
            1,
            false,
            0,
            &[non_resident(ATTR_DATA, 0, 0, &[0x11, 0x02, 0x10], 30000)],
        ));
        let rest = parse(record(
            1,
            false,
            40,
            &[non_resident(ATTR_DATA, 0, 2, &[0x11, 0x05, 0x40], 999)],
        ));
        assert_eq!((first.size, first.alloc), (Some(30000), 2 * CLUSTER));
        assert_eq!((rest.size, rest.alloc), (None, 5 * CLUSTER));
        let small = parse(record(1, false, 0, &[resident(ATTR_DATA, 0, b"hello")]));
        assert_eq!((small.size, small.alloc), (Some(5), 0));
        let mut junction = vec![0u8; 8];
        junction[0..4].copy_from_slice(&REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        assert!(
            parse(record(
                1,
                true,
                0,
                &[resident(ATTR_REPARSE_POINT, 0, &junction)]
            ))
            .is_link
        );
    }

    #[test]
    fn names_keep_unpaired_surrogates() {
        assert_eq!(wtf8(&[0x61, 0xD800, 0x62]), b"a\xED\xA0\x80b");
        assert_eq!(
            wtf8(&"zażółć".encode_utf16().collect::<Vec<_>>()),
            "zażółć".as_bytes()
        );
    }

    /// root(5) ─ Users(30) ─ me(31) ─ big.bin(40), link.bin(41 → hardlink of 40)
    ///         └ old.txt(42, stale parent seq) └ Junction(43)
    pub fn sample() -> Mft {
        let mut mft = Mft::with_capacity(64);
        let mut add = |i: u32, r: Vec<u8>| mft.add(i, parse(r));
        add(
            5,
            record(5, true, 0, &[std_info(10), file_name(5, 5, ".", 1)]),
        );
        add(
            30,
            record(1, true, 0, &[std_info(20), file_name(5, 5, "Users", 1)]),
        );
        add(
            31,
            record(
                1,
                true,
                0,
                &[
                    std_info(30),
                    file_name(30, 1, "me", 1),
                    non_resident(ATTR_INDEX_ALLOCATION, 4, 0, &[0x11, 0x01, 0x30], 4096),
                ],
            ),
        );
        add(
            40,
            record(
                1,
                false,
                0,
                &[
                    std_info(40),
                    file_name(31, 1, "big.bin", 1),
                    non_resident(ATTR_DATA, 0, 0, &[0x22, 0x00, 0x01, 0x40, 0x00], 1_000_000),
                ],
            ),
        );
        // A second hard link to record 40, stored in an extension record.
        add(50, record(1, false, 40, &[file_name(31, 1, "link.bin", 1)]));
        add(
            42,
            record(
                1,
                false,
                0,
                &[file_name(30, 9, "old.txt", 1), resident(ATTR_DATA, 0, b"x")],
            ),
        );
        let mut tag = vec![0u8; 8];
        tag[0..4].copy_from_slice(&REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        add(
            43,
            record(
                1,
                true,
                0,
                &[
                    file_name(31, 1, "Junction", 1),
                    resident(ATTR_REPARSE_POINT, 0, &tag),
                ],
            ),
        );
        mft
    }

    #[test]
    fn builds_trees() {
        let mft = sample();
        let t = mft.build_tree(PathBuf::from("C:\\"), &[]).unwrap();
        let users = t
            .children(Tree::ROOT)
            .find(|&c| t.name(c) == b"Users")
            .unwrap();
        assert_eq!(
            t.children(users).len(),
            1,
            "the stale old.txt link is dropped"
        );
        let me = t.children(users).next().unwrap();
        let names: Vec<_> = t
            .children(me)
            .map(|c| t.name_lossy(c).into_owned())
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert_eq!(
            t.alloc(me),
            4096 + (1 << 20),
            "hardlink counted once, plus the index allocation"
        );
        assert_eq!(t.size(Tree::ROOT), 1_000_000);
        let junction = t.children(me).find(|&c| t.name(c) == b"Junction").unwrap();
        assert!(t.flags(junction) & flags::SYMLINK != 0 && !t.is_dir(junction));
        let dups = t
            .children(me)
            .filter(|&c| t.flags(c) & flags::HARDLINK_DUP != 0)
            .count();
        assert_eq!(dups, 1);
        assert_eq!(t.mtime(me), 40);

        // Re-rooted at a subdirectory, matched case-insensitively.
        let sub = mft
            .build_tree(
                PathBuf::from("C:\\Users\\me"),
                &[b"users".to_vec(), b"ME".to_vec()],
            )
            .unwrap();
        assert_eq!(sub.children(Tree::ROOT).len(), 3);
        assert_eq!(sub.alloc(Tree::ROOT), 4096 + (1 << 20));
        assert!(
            mft.build_tree(PathBuf::from("C:\\x"), &[b"nope".to_vec()])
                .is_none()
        );
    }
}
