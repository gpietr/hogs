use std::sync::atomic::{AtomicU64, Ordering};

/// Live scan counters, updated by scanner threads once per directory.
#[derive(Debug, Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
}

impl Progress {
    pub fn add(&self, files: u64, dirs: u64, bytes: u64, errors: u64) {
        self.files.fetch_add(files, Ordering::Relaxed);
        self.dirs.fetch_add(dirs, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.errors.fetch_add(errors, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}
