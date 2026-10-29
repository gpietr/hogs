//! The staged-deletion list, shared by the CLI (used by people and agents)
//! and the TUI. Agents can only add to it; deleting requires a human to apply it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedItem {
    pub path: PathBuf,
    /// Allocated size when staged.
    pub alloc: u64,
    /// Newest modification time inside the item when staged (Unix seconds);
    /// applying refuses items modified after staging.
    pub newest_mtime: i64,
    pub reason: String,
    /// Who staged it, e.g. `agent` or `user`.
    pub staged_by: String,
    pub staged_at: i64,
}

pub struct StagingStore {
    path: PathBuf,
}

impl StagingStore {
    /// The per-user store: `$HOGS_DATA_DIR`, else the platform data directory.
    pub fn open_default() -> io::Result<Self> {
        let dir =
            data_dir().ok_or_else(|| io::Error::other("cannot determine the data directory"))?;
        crate::user::create_dir_all(&dir)?;
        Ok(StagingStore::at(dir.join("staged.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        StagingStore { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<Vec<StagedItem>> {
        match fs::read(&self.path) {
            Ok(data) => serde_json::from_slice(&data).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Read-modify-write under an exclusive lock, replacing the file atomically.
    pub fn update<R>(&self, f: impl FnOnce(&mut Vec<StagedItem>) -> R) -> io::Result<R> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension("lock"))?;
        crate::user::give_back(&self.path.with_extension("lock"));
        lock.lock()?;
        let mut items = self.load()?;
        let result = f(&mut items);
        let tmp = self.path.with_extension("tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&items).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        crate::user::give_back(&self.path);
        Ok(result)
    }
}

pub enum StageOutcome {
    Added,
    /// Already staged itself or through a staged parent.
    AlreadyCovered(PathBuf),
    /// Added, replacing this many staged items inside it.
    AddedReplacing(usize),
}

/// Adds an item, keeping the list free of overlapping entries.
pub fn stage(items: &mut Vec<StagedItem>, item: StagedItem) -> StageOutcome {
    if let Some(cover) = items.iter().find(|i| item.path.starts_with(&i.path)) {
        return StageOutcome::AlreadyCovered(cover.path.clone());
    }
    let before = items.len();
    items.retain(|i| !i.path.starts_with(&item.path));
    let replaced = before - items.len();
    items.push(item);
    if replaced > 0 {
        StageOutcome::AddedReplacing(replaced)
    } else {
        StageOutcome::Added
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Directory for hogs' persistent data (`~/.local/share/hogs`, `%LOCALAPPDATA%\hogs`, ...).
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HOGS_DATA_DIR") {
        return Some(dir.into());
    }
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        crate::user::home().map(|h| h.join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            // Under sudo this belongs to root's session, not the invoking user.
            .filter(|p| p.is_absolute() && crate::user::invoker().is_none())
            .or_else(|| crate::user::home().map(|h| h.join(".local/share")))
    };
    base.map(|b| b.join("hogs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str) -> StagedItem {
        StagedItem {
            path: path.into(),
            alloc: 1,
            newest_mtime: 0,
            reason: "test".into(),
            staged_by: "agent".into(),
            staged_at: 0,
        }
    }

    #[test]
    fn no_overlaps() {
        let mut items = vec![item("/a/b/c"), item("/a/b/d"), item("/x")];
        assert!(matches!(
            stage(&mut items, item("/a/b/c/e")),
            StageOutcome::AlreadyCovered(_)
        ));
        assert!(matches!(
            stage(&mut items, item("/a/b")),
            StageOutcome::AddedReplacing(2)
        ));
        assert!(matches!(
            stage(&mut items, item("/a/bc")),
            StageOutcome::Added
        ));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StagingStore::at(dir.path().join("staged.json"));
        assert!(store.load().unwrap().is_empty());
        store.update(|items| stage(items, item("/a"))).unwrap();
        store.update(|items| stage(items, item("/b"))).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|i| i.path.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["/a", "/b"]
        );
    }
}
