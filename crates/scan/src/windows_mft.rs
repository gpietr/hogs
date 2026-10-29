//! Windows fast path: read the NTFS master file table straight from the
//! volume, the way WizTree does, instead of listing directories. Needs
//! Administrator rights (to open `\\.\C:`) and an NTFS volume; callers fall
//! back to the directory walk otherwise.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, Prefix};

use hogs_core::{Progress, Tree};
use rayon::prelude::*;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};

use crate::ntfs::{self, Mft};

/// Bytes read from the volume at a time; a multiple of every cluster size.
const CHUNK: usize = 16 << 20;

struct Volume {
    file: File,
    bytes_per_cluster: u64,
    record_size: usize,
    mft_start: u64,
    mft_valid: u64,
}

/// Drive letter and path components below the drive root, for `C:\...` paths.
fn split_drive(root: &Path) -> Option<(char, Vec<Vec<u8>>)> {
    let mut comps = root.components();
    let drive = match comps.next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d as char,
            _ => return None,
        },
        _ => return None,
    };
    let rest = comps
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.as_encoded_bytes().to_vec()),
            _ => None,
        })
        .collect();
    Some((drive, rest))
}

fn open_volume(drive: char) -> io::Result<Volume> {
    let file = OpenOptions::new()
        .read(true)
        .open(format!(r"\\.\{drive}:"))?;
    // SAFETY: an all-zero NTFS_VOLUME_DATA_BUFFER is valid, and every pointer
    // passed to DeviceIoControl is valid for the sizes given.
    let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            (&raw mut data).cast(),
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // Most likely not NTFS (FAT, exFAT, ReFS).
        return Err(io::Error::last_os_error());
    }
    let record_size = data.BytesPerFileRecordSegment as usize;
    if !(512..=65536).contains(&record_size)
        || !record_size.is_multiple_of(512)
        || data.BytesPerCluster == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected NTFS geometry",
        ));
    }
    Ok(Volume {
        file,
        bytes_per_cluster: u64::from(data.BytesPerCluster),
        record_size,
        mft_start: data.MftStartLcn as u64,
        mft_valid: data.MftValidDataLength as u64,
    })
}

/// The drive root (`C:\\`) for `dir` when the MFT of its volume can be read,
/// remembered per drive so the check costs one volume open per process.
pub(crate) fn drive_root_if_fast(dir: &Path) -> Option<std::path::PathBuf> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static FAST: OnceLock<Mutex<HashMap<char, bool>>> = OnceLock::new();
    let (drive, _) = split_drive(dir)?;
    let drive = drive.to_ascii_uppercase();
    let fast = *FAST
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .entry(drive)
        .or_insert_with(|| open_volume(drive).is_ok());
    fast.then(|| std::path::PathBuf::from(format!("{drive}:\\")))
}

fn read_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

/// Where the MFT lives: record 0 describes the MFT itself.
fn mft_runs(vol: &Volume) -> io::Result<Vec<ntfs::Run>> {
    let invalid = |m: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot read the MFT: {m}"),
        )
    };
    let mut rec = vec![0u8; vol.record_size.max(vol.bytes_per_cluster as usize)];
    read_exact_at(&vol.file, vol.mft_start * vol.bytes_per_cluster, &mut rec)?;
    let rec = &mut rec[..vol.record_size];
    if !ntfs::apply_fixups(rec) {
        return Err(invalid("record 0 is damaged"));
    }
    ntfs::mft_data_runs(rec).ok_or_else(|| invalid("no data runs in record 0"))
}

pub(crate) fn scan(root: &Path, progress: &Progress) -> io::Result<Tree> {
    let (drive, rel) = split_drive(root).ok_or_else(|| {
        io::Error::new(io::ErrorKind::Unsupported, "not a path on a lettered drive")
    })?;
    let vol = open_volume(drive)?;
    let runs = mft_runs(&vol)?;
    let total = (vol.mft_valid / vol.record_size as u64) as usize;
    let mut mft = Mft::with_capacity(total);
    let mut buf = vec![0u8; CHUNK];
    // Records can span run boundaries when clusters are smaller than records.
    let mut carry: Vec<u8> = Vec::new();
    let mut next_record = 0usize;
    'runs: for run in runs {
        let run_bytes = run.clusters * vol.bytes_per_cluster;
        let Some(lcn) = run.lcn else {
            // Sparse MFT ranges hold no records.
            carry.clear();
            next_record += (run_bytes / vol.record_size as u64) as usize;
            continue;
        };
        let mut offset = lcn * vol.bytes_per_cluster;
        let end = offset + run_bytes;
        while offset < end {
            if next_record >= total {
                break 'runs;
            }
            let len = ((end - offset) as usize).min(CHUNK);
            read_exact_at(&vol.file, offset, &mut buf[..len])?;
            offset += len as u64;
            carry.extend_from_slice(&buf[..len]);
            let whole = carry.len() / vol.record_size * vol.record_size;
            let first = next_record;
            let parsed: Vec<(u32, ntfs::Record)> = carry[..whole]
                .par_chunks_mut(vol.record_size)
                .enumerate()
                .filter(|(i, _)| first + i < total)
                .filter_map(|(i, rec)| {
                    if !ntfs::apply_fixups(rec) {
                        return None;
                    }
                    Some((
                        (first + i) as u32,
                        ntfs::parse_record(rec, vol.bytes_per_cluster)?,
                    ))
                })
                .collect();
            let (mut files, mut bytes) = (0, 0);
            for (index, rec) in parsed {
                files += u64::from(rec.base.is_none());
                bytes += rec.alloc;
                mft.add(index, rec);
            }
            progress.add(files, 0, bytes, 0);
            next_record += whole / vol.record_size;
            carry.drain(..whole);
        }
    }
    mft.build_tree(root.to_path_buf(), &rel).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} not found in the MFT", root.display()),
        )
    })
}
