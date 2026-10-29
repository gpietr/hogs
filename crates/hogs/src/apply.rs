//! Applying the staged-deletion list. This is the only code that deletes
//! anything, and it re-verifies every item first.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

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

/// Progress of an apply, reported as it goes.
pub enum Event<'a> {
    /// Re-checking item `n` (rescans directories, which can take a while).
    Checking(usize, &'a StagedItem),
    /// Item `n` passed the checks and is being deleted.
    Deleting(usize, &'a StagedItem),
    /// The item is done.
    Finished(&'a StagedItem, &'a Outcome),
}

/// Verifies and deletes `items` one by one, removing each finished one from
/// the staged list right away (so an interrupted apply leaves an accurate
/// list), then invalidates cached scans that contained them. Shared by
/// `hogs staged apply` and the TUI, so both apply exactly the same checks.
pub fn apply_items(
    store: &StagingStore,
    snapshots: Option<&SnapshotStore>,
    items: &[StagedItem],
    permanent: bool,
    mut report: impl FnMut(Event<'_>),
) -> Result<Vec<Outcome>> {
    let mut outcomes = Vec::with_capacity(items.len());
    let mut done = Vec::new();
    let mut list_error = None;
    for (n, item) in items.iter().enumerate() {
        report(Event::Checking(n, item));
        let outcome = match verify(item) {
            Ok(()) => {
                report(Event::Deleting(n, item));
                match delete(&item.path, permanent) {
                    Ok(()) => Outcome::Deleted,
                    Err(e) => Outcome::Kept(e),
                }
            }
            Err(e) if e == "no longer exists" => Outcome::Gone,
            Err(e) => Outcome::Kept(e),
        };
        if !matches!(outcome, Outcome::Kept(_)) {
            // Keep going if the list cannot be written: every item is re-checked
            // anyway, and stopping half-way would leave even more stale.
            if let Err(e) = store.update(|all| all.retain(|i| i.path != item.path)) {
                list_error.get_or_insert(e);
            }
            done.push(item.path.clone());
        }
        report(Event::Finished(item, &outcome));
        outcomes.push(outcome);
    }
    if let Some(snapshots) = snapshots {
        snapshots.invalidate(&done);
    }
    match list_error {
        Some(e) => Err(anyhow::Error::from(e)
            .context("items were applied, but the staged list could not be updated")),
        None => Ok(outcomes),
    }
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

/// Terminal output for `hogs staged apply`: a status line rewritten in place
/// while an item is checked and deleted, and one result line per item.
struct StatusLine {
    live: bool,
    total: usize,
    total_bytes: u64,
    done_bytes: u64,
    started: Instant,
    deleting: &'static str,
}

impl StatusLine {
    fn new(items: &[StagedItem], permanent: bool) -> Self {
        StatusLine {
            live: io::stdout().is_terminal(),
            total: items.len(),
            total_bytes: items.iter().map(|i| i.alloc).sum(),
            done_bytes: 0,
            started: Instant::now(),
            deleting: if permanent {
                "deleting"
            } else {
                "moving to trash"
            },
        }
    }

    fn on(&mut self, event: Event<'_>) {
        match event {
            Event::Checking(n, item) => self.status(n, "checking", item),
            Event::Deleting(n, item) => self.status(n, self.deleting, item),
            Event::Finished(item, outcome) => {
                self.clear_status();
                match outcome {
                    Outcome::Deleted => println!("  deleted  {}", item.path.display()),
                    Outcome::Gone => {
                        println!("  gone     {} (already removed)", item.path.display())
                    }
                    Outcome::Kept(e) => println!("  KEPT     {}: {e}", item.path.display()),
                }
                self.done_bytes += item.alloc;
            }
        }
    }

    fn status(&self, n: usize, doing: &str, item: &StagedItem) {
        if !self.live {
            return;
        }
        let head = format!(
            "[{}/{}] {} of {} · {doing} ",
            n + 1,
            self.total,
            human_size(self.done_bytes),
            human_size(self.total_bytes)
        );
        let tail = format!(
            " ({}) · {:.0}s",
            human_size(item.alloc),
            self.started.elapsed().as_secs_f64()
        );
        // One line only: a wrapped line could not be rewritten in place.
        // Some terminals (and ptys) report no size; assume 80 columns then.
        let width = ratatui::crossterm::terminal::size()
            .ok()
            .filter(|&(w, _)| w >= 20)
            .map_or(80, |(w, _)| w as usize)
            .saturating_sub(1);
        let room = width.saturating_sub(head.chars().count() + tail.chars().count());
        let path = fit(&item.path.display().to_string(), room);
        self.clear_status();
        print!("{head}{path}{tail}");
        let _ = io::stdout().flush();
    }

    fn clear_status(&self) {
        if self.live {
            let _ = ratatui::crossterm::execute!(
                io::stdout(),
                ratatui::crossterm::cursor::MoveToColumn(0),
                ratatui::crossterm::terminal::Clear(
                    ratatui::crossterm::terminal::ClearType::CurrentLine
                )
            );
        }
    }
}

/// `s` shortened to `width` characters by cutting its start: the end of a path says the most.
fn fit(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    format!("…{}", s.chars().skip(len - keep).collect::<String>())
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

    let mut progress = StatusLine::new(&items, permanent);
    let snapshots = SnapshotStore::open_default().ok();
    let outcomes = apply_items(store, snapshots.as_ref(), &items, permanent, |e| {
        progress.on(e)
    })?;
    let freed: u64 = items
        .iter()
        .zip(&outcomes)
        .filter(|(_, o)| matches!(o, Outcome::Deleted))
        .map(|(i, _)| i.alloc)
        .sum();
    println!("Done in {:.1}s.", progress.started.elapsed().as_secs_f64());
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

    #[test]
    fn apply_reports_progress_and_updates_the_list_per_item() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("old/deep")).unwrap();
        fs::write(root.join("old/deep/f"), b"x").unwrap();
        fs::write(root.join("changed"), b"x").unwrap();
        let items = [
            item(root.join("old"), i64::MAX),
            item(root.join("gone"), i64::MAX),
            item(root.join("changed"), 0),
        ];
        let data = tempfile::tempdir().unwrap();
        let store = StagingStore::at(data.path().join("staged.json"));
        store
            .update(|all| all.extend(items.iter().cloned()))
            .unwrap();

        let mut events = Vec::new();
        let outcomes = apply_items(&store, None, &items, true, |e| {
            let staged = store.load().unwrap().len();
            events.push(match e {
                Event::Checking(n, _) => format!("check {n} ({staged} staged)"),
                Event::Deleting(n, _) => format!("delete {n}"),
                Event::Finished(i, Outcome::Deleted) => {
                    format!("deleted {}", i.path.file_name().unwrap().display())
                }
                Event::Finished(i, Outcome::Gone) => {
                    format!("gone {}", i.path.file_name().unwrap().display())
                }
                Event::Finished(i, Outcome::Kept(_)) => {
                    format!("kept {}", i.path.file_name().unwrap().display())
                }
            });
        })
        .unwrap();
        assert_eq!(
            events,
            [
                "check 0 (3 staged)",
                "delete 0",
                "deleted old",
                // The list shrinks as items finish, not only at the end.
                "check 1 (2 staged)",
                "gone gone",
                "check 2 (1 staged)",
                "kept changed",
            ]
        );
        assert!(matches!(
            outcomes[..],
            [Outcome::Deleted, Outcome::Gone, Outcome::Kept(_)]
        ));
        assert!(!root.join("old").exists() && root.join("changed").exists());
        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[test]
    fn status_paths_keep_their_end() {
        assert_eq!(
            fit("/home/u/projects/app/node_modules", 18),
            "…/app/node_modules"
        );
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("zażółć", 4), "…ółć");
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
