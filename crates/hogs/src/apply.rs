//! Applying the staged-deletion list. This is the only code that deletes
//! anything, and it re-verifies every item first.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Result, bail};
use hogs_core::fmt::human_size;
use hogs_core::protect::is_protected;
use hogs_core::snapshot::SnapshotStore;
use hogs_core::staging::{StagedItem, StagingStore};
use hogs_core::{Progress, Tree, flags};
use hogs_scan::ScanOptions;

use crate::session::canonical;

/// Checks that an item is still safe to delete: still present, still resolving
/// to the same place, not protected, not a mount point or containing one, and
/// not modified since it was staged.
pub fn verify(item: &StagedItem) -> Result<(), String> {
    let meta = fs::symlink_metadata(&item.path).map_err(|_| "no longer exists".to_string())?;
    // The staged list is a plain file anyone with the user's rights can edit, and
    // parents can be swapped for symlinks after staging: check where the path
    // resolves now, not just what it says.
    let real = canonical(&item.path).map_err(|e| format!("cannot resolve: {e}"))?;
    if real != item.path {
        return Err(format!(
            "now resolves to {}; stage it again if it should still go",
            real.display()
        ));
    }
    if let Some(why) = is_protected(&real, &hogs_core::user::protected_homes()) {
        return Err(format!("refused: it {why}"));
    }
    if meta.is_dir()
        && let Some(mount) = mount_inside(&real, &meta)
    {
        return Err(format!(
            "is or contains the mount point {}",
            mount.display()
        ));
    }
    let newest = if meta.is_dir() {
        let tree = hogs_scan::scan(&item.path, &ScanOptions::default(), &Progress::default())
            .map_err(|e| format!("cannot rescan: {e}"))?;
        if contains_other_fs(&tree) {
            return Err("contains a mount point of another filesystem".into());
        }
        tree.mtime(Tree::ROOT)
    } else {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64)
    };
    if newest > item.newest_mtime {
        return Err("modified since it was staged; stage it again if it should still go".into());
    }
    Ok(())
}

/// A mount point at or below `dir`, if any. Deleting through one would wipe
/// another filesystem (or, for bind mounts, files that live elsewhere).
fn mount_inside(dir: &Path, meta: &fs::Metadata) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(m) = mount_points().into_iter().find(|m| m.starts_with(dir)) {
        return Some(m);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Fallback without a mount table: a different device than the parent's.
        let parent_dev = dir
            .parent()
            .and_then(|p| fs::symlink_metadata(p).ok())
            .map(|m| m.dev());
        if parent_dev != Some(meta.dev()) {
            return Some(dir.to_path_buf());
        }
    }
    // No mount detection on other platforms yet.
    let _ = (dir, meta);
    None
}

/// Mount points from `/proc/self/mountinfo` (field 5, with octal escapes like `\040`).
#[cfg(target_os = "linux")]
fn mount_points() -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let Ok(info) = fs::read("/proc/self/mountinfo") else {
        return Vec::new();
    };
    info.split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(4))
        .map(|field| PathBuf::from(std::ffi::OsString::from_vec(unescape_octal(field))))
        .collect()
}

#[cfg(target_os = "linux")]
fn unescape_octal(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let octal = s
            .get(i + 1..i + 4)
            .filter(|d| s[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal {
            Some(d) => {
                out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                i += 4;
            }
            None => {
                out.push(s[i]);
                i += 1;
            }
        }
    }
    out
}

fn contains_other_fs(tree: &Tree) -> bool {
    let mut found = false;
    tree.walk(Tree::ROOT, |id, _| {
        found |= tree.flags(id) & flags::OTHER_FS != 0;
        !found
    });
    found
}

fn delete(path: &Path, permanent: bool) -> Result<(), String> {
    let result = if !permanent {
        trash::delete(path).map_err(|e| e.to_string())
    } else if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        fs::remove_file(path).map_err(|e| e.to_string())
    };
    result.map_err(|e| format!("delete failed: {e}"))
}

/// Shown before trashing as root: the trash crate uses the process's own trash.
pub const ROOT_TRASH_NOTE: &str = "Running as root via sudo: items go to root's trash (e.g. /root/.local/share/Trash), \
not yours, and the space is freed only once that is emptied. Use --permanent to delete directly.";

pub enum Outcome {
    Deleted,
    /// Already gone before applying.
    Gone,
    /// Not deleted, with the reason.
    Kept(String),
}

/// Verifies and deletes `items`, drops the finished ones from the staged
/// list and invalidates cached scans that contained them. Shared by
/// `hogs staged apply` and the TUI, so both apply exactly the same checks.
pub fn apply_items(
    store: &StagingStore,
    items: &[StagedItem],
    permanent: bool,
) -> Result<Vec<Outcome>> {
    let outcomes: Vec<Outcome> = items
        .iter()
        .map(
            |item| match verify(item).and_then(|()| delete(&item.path, permanent)) {
                Ok(()) => Outcome::Deleted,
                Err(e) if e == "no longer exists" => Outcome::Gone,
                Err(e) => Outcome::Kept(e),
            },
        )
        .collect();
    let done: Vec<PathBuf> = items
        .iter()
        .zip(&outcomes)
        .filter(|(_, o)| !matches!(o, Outcome::Kept(_)))
        .map(|(i, _)| i.path.clone())
        .collect();
    store.update(|all| all.retain(|i| !done.contains(&i.path)))?;
    if let Ok(snapshots) = SnapshotStore::open_default() {
        snapshots.invalidate(&done);
    }
    Ok(outcomes)
}

pub fn list_text(items: &[StagedItem]) -> String {
    let mut out = String::new();
    for i in items {
        out.push_str(&format!(
            "{:>10}  {}\n            {}: {}\n",
            human_size(i.alloc),
            i.path.display(),
            i.staged_by,
            i.reason
        ));
    }
    let total: u64 = items.iter().map(|i| i.alloc).sum();
    out.push_str(&format!(
        "{} items, {} total\n",
        items.len(),
        human_size(total)
    ));
    out
}

fn confirm(prompt: &str, expected: &str) -> io::Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case(expected))
}

pub fn apply(store: &StagingStore, permanent: bool) -> Result<()> {
    let items = store.load()?;
    if items.is_empty() {
        println!("Nothing is staged.");
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!(
            "applying deletions needs a person at an interactive terminal; run `hogs staged apply` yourself"
        );
    }
    print!("{}", list_text(&items));
    if !permanent && hogs_core::user::invoker().is_some() {
        println!("\n{ROOT_TRASH_NOTE}");
    }
    let total = human_size(items.iter().map(|i| i.alloc).sum());
    let ok = if permanent {
        confirm(
            &format!(
                "\nPermanently delete {} items ({total})? This cannot be undone. Type 'delete' to confirm: ",
                items.len()
            ),
            "delete",
        )?
    } else {
        confirm(
            &format!(
                "\nMove {} items ({total}) to the trash? [y/N] ",
                items.len()
            ),
            "y",
        )?
    };
    if !ok {
        println!("Nothing was deleted.");
        return Ok(());
    }

    let outcomes = apply_items(store, &items, permanent)?;
    let mut freed = 0;
    for (item, outcome) in items.iter().zip(&outcomes) {
        match outcome {
            Outcome::Deleted => {
                println!("  deleted  {}", item.path.display());
                freed += item.alloc;
            }
            Outcome::Gone => println!("  gone     {} (already removed)", item.path.display()),
            Outcome::Kept(e) => println!("  KEPT     {}: {e}", item.path.display()),
        }
    }
    if permanent {
        println!("Freed {}.", human_size(freed));
    } else {
        println!(
            "Moved {} to the trash; the space is freed once the trash is emptied.",
            human_size(freed)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: PathBuf, newest_mtime: i64) -> StagedItem {
        StagedItem {
            path,
            alloc: 0,
            newest_mtime,
            reason: "test".into(),
            staged_by: "test".into(),
            staged_at: 0,
        }
    }

    #[test]
    fn refuses_paths_that_resolve_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("real/x")).unwrap();
        let ok = item(root.join("real/x"), i64::MAX);
        assert_eq!(verify(&ok), Ok(()));
        // `..` in a hand-edited staged list.
        let dotdot = item(root.join("real/x/../x"), i64::MAX);
        assert!(verify(&dotdot).unwrap_err().contains("now resolves to"));
        // A parent swapped for a symlink after staging.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
            let through_link = item(root.join("link/x"), i64::MAX);
            assert!(
                verify(&through_link)
                    .unwrap_err()
                    .contains("now resolves to")
            );
        }
        assert_eq!(
            verify(&item(root.join("gone"), i64::MAX)),
            Err("no longer exists".into())
        );
    }

    #[test]
    fn refuses_items_changed_since_staging() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        fs::write(root.join("f"), b"x").unwrap();
        assert!(
            verify(&item(root.join("f"), 0))
                .unwrap_err()
                .contains("modified since")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detects_mount_points() {
        let proc = Path::new("/proc");
        let meta = fs::symlink_metadata(proc).unwrap();
        assert_eq!(mount_inside(proc, &meta), Some(proc.to_path_buf()));
        let dir = tempfile::tempdir().unwrap();
        let meta = fs::symlink_metadata(dir.path()).unwrap();
        assert_eq!(mount_inside(dir.path(), &meta), None);
        assert_eq!(unescape_octal(br"/mnt/my\040disk"), b"/mnt/my disk");
        assert_eq!(unescape_octal(br"a\\b\04"), br"a\\b\04");
    }
}
