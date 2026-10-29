//! The NTFS MFT backend against the directory walk, on a real volume.
#![cfg(windows)]

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use hogs_core::{Progress, Tree, flags};
use hogs_scan::{Backend, ScanOptions, scan};

/// Every node as (path relative to the root) → (apparent size, flags).
fn entries(t: &Tree) -> HashMap<PathBuf, (u64, u8)> {
    let mut out = HashMap::new();
    t.walk(Tree::ROOT, |id, _| {
        let rel = t
            .path(id)
            .strip_prefix(t.root_path())
            .unwrap()
            .to_path_buf();
        out.insert(rel, (t.size(id), t.flags(id)));
        true
    });
    out
}

#[test]
fn mft_matches_directory_walk() {
    // Static system files, so both scans see the same thing.
    let dir = Path::new(r"C:\Windows\System32\drivers");
    let opts = |backend| ScanOptions {
        backend,
        ..Default::default()
    };
    let mft = match scan(dir, &opts(Backend::Mft), &Progress::default()) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping: the MFT backend needs Administrator rights ({e})");
            return;
        }
        Err(e) => panic!("MFT scan failed: {e}"),
    };
    let walk = scan(dir, &opts(Backend::Generic), &Progress::default()).unwrap();
    let (m, w) = (entries(&mft), entries(&walk));
    println!(
        "walk: {} entries, {} bytes; MFT: {} entries, {} bytes",
        w.len(),
        walk.size(Tree::ROOT),
        m.len(),
        mft.size(Tree::ROOT)
    );

    let missing: Vec<_> = w.keys().filter(|p| !m.contains_key(*p)).collect();
    assert!(
        missing.is_empty(),
        "{} entries missing from the MFT scan, e.g. {:?}",
        missing.len(),
        &missing[..missing.len().min(10)]
    );

    let skip = flags::DIR | flags::SYMLINK | flags::ERROR;
    let mismatched: Vec<_> = w
        .iter()
        .filter(|(_, (_, f))| f & skip == 0)
        .filter_map(|(p, &(size, _))| {
            let (msize, mflags) = m[p];
            (mflags & flags::HARDLINK_DUP == 0 && msize != size).then_some((p, size, msize))
        })
        .collect();
    assert!(
        mismatched.is_empty(),
        "{} size mismatches (path, walk, MFT), e.g. {:?}",
        mismatched.len(),
        &mismatched[..mismatched.len().min(10)]
    );
    assert!(m.len() >= w.len());
}
