//! Query and staging subcommands. Each prints plain text to stdout; with
//! `--paths`/`-0` row-based commands print only paths, for piping.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use hogs_core::diff;
use hogs_core::fmt::human_size;
use hogs_core::history::MIN_FILE;
use hogs_core::protect::is_protected;
use hogs_core::query::{self, Filter};
use hogs_core::staging::{self, StageOutcome, StagedItem, StagingStore};
use hogs_core::{NodeId, Tree};

use crate::report::{Output, Rows, ago, count, datetime, signed};
use crate::session::{Session, canonical};

pub struct Ctx {
    pub session: Session,
    pub output: Output,
}

impl Ctx {
    fn rows<'a>(&self, tree: &'a Tree) -> Rows<'a> {
        Rows {
            tree,
            now: staging::now(),
            output: self.output,
        }
    }
}

/// Writes to stdout, treating a closed pipe (e.g. `| head`) as success.
pub fn emit(out: &str) -> Result<()> {
    match io::stdout().lock().write_all(out.as_bytes()) {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}

pub fn ls(ctx: &mut Ctx, path: &Path, depth: usize, limit: usize) -> Result<()> {
    let (tree, id) = ctx.session.resolve(path)?;
    let mut out = String::new();
    let rows = ctx.rows(&tree);
    if rows.is_table() {
        rows.row(&mut out, id, tree.alloc(id), 0);
    }
    rows.listing(&mut out, id, depth.max(1), limit.max(1));
    emit(&out)
}

pub fn files(ctx: &mut Ctx, path: &Path, limit: usize) -> Result<()> {
    let (tree, id) = ctx.session.resolve(path)?;
    let mut out = String::new();
    let rows = ctx.rows(&tree);
    for f in query::largest_files(&tree, id, limit) {
        rows.row(&mut out, f, tree.alloc(id), 0);
    }
    emit(&out)
}

pub fn dirs(ctx: &mut Ctx, path: &Path, limit: usize, depth: usize) -> Result<()> {
    let (tree, id) = ctx.session.resolve(path)?;
    let mut out = String::new();
    let rows = ctx.rows(&tree);
    for d in query::largest_dirs(&tree, id, limit, depth) {
        rows.row(&mut out, d, tree.alloc(id), 0);
    }
    emit(&out)
}

pub fn ext(ctx: &mut Ctx, path: &Path, limit: usize) -> Result<()> {
    let (tree, id) = ctx.session.resolve(path)?;
    let total = tree.alloc(id).max(1);
    let stats = query::by_extension(&tree, id);
    let mut out = String::new();
    for s in stats.iter().take(limit) {
        let ext = if s.ext.is_empty() {
            "(none)".to_string()
        } else {
            format!(".{}", s.ext)
        };
        let pct = s.alloc as f64 * 100.0 / total as f64;
        let _ = writeln!(
            out,
            "{:>10} {pct:>5.1}% {:>8} files  {ext}",
            human_size(s.alloc),
            s.files
        );
    }
    if stats.len() > limit {
        let _ = writeln!(out, "… {} more extensions", stats.len() - limit);
    }
    emit(&out)
}

pub fn find(ctx: &mut Ctx, path: &Path, filter: &Filter, limit: usize) -> Result<()> {
    let (tree, id) = ctx.session.resolve(path)?;
    let r = query::find(&tree, id, filter, limit);
    let mut out = String::new();
    let rows = ctx.rows(&tree);
    if rows.is_table() {
        let _ = writeln!(
            out,
            "{} matches, {} on disk in total.",
            r.total_matches,
            human_size(r.total_alloc)
        );
    }
    for m in &r.matches {
        rows.row(&mut out, *m, tree.alloc(id), 0);
    }
    if r.total_matches > r.matches.len() && rows.is_table() {
        let _ = writeln!(
            out,
            "… {} more matches (raise -n to see them)",
            r.total_matches - r.matches.len()
        );
    }
    emit(&out)
}

/// Past scans: grouped by root, or the size of `path` in every scan covering it.
pub fn history(ctx: &mut Ctx, path: Option<&Path>, clear: bool) -> Result<()> {
    let history = ctx
        .session
        .history
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the scan history is unavailable"))?;
    if clear {
        let n = history.clear()?;
        return emit(&format!(
            "Deleted {}.\n",
            count(n, "saved scan", "saved scans")
        ));
    }
    let now = staging::now();
    let mut out = String::new();
    let row = |out: &mut String, at: i64, size: u64, older: Option<u64>, note: &str| {
        let change = older.map_or(String::new(), |o| signed(size as i64 - o as i64));
        let _ = writeln!(
            out,
            "  {}  {:>9}  {:>10}  {:>11}{note}",
            datetime(at),
            ago(now - at),
            human_size(size),
            change
        );
    };
    match path {
        None => {
            let entries = history.entries();
            if entries.is_empty() {
                return emit(&format!("No saved scans yet. {HISTORY_NOTE}"));
            }
            let bytes: u64 = entries.iter().map(|e| e.bytes).sum();
            let _ = writeln!(
                out,
                "{} using {}.",
                count(entries.len(), "saved scan", "saved scans"),
                human_size(bytes)
            );
            let mut roots: Vec<&Path> = Vec::new();
            for e in &entries {
                if !roots.contains(&e.root.as_path()) {
                    roots.push(&e.root);
                }
            }
            for root in roots {
                let _ = writeln!(out, "\n{}", root.display());
                let series: Vec<_> = entries.iter().filter(|e| e.root == root).collect();
                for (i, e) in series.iter().enumerate() {
                    row(
                        &mut out,
                        e.scanned_at,
                        e.alloc,
                        series.get(i + 1).map(|o| o.alloc),
                        "",
                    );
                }
            }
        }
        Some(p) => {
            let p = canonical(p)?;
            let entries = history.covering(&p);
            if entries.is_empty() {
                return emit(&format!("No saved scans yet. {HISTORY_NOTE}"));
            }
            let _ = writeln!(
                out,
                "{} in {}:",
                p.display(),
                count(entries.len(), "saved scan", "saved scans")
            );
            let sizes: Vec<Option<u64>> = entries
                .iter()
                .map(|e| {
                    history
                        .load(e)
                        .ok()
                        .and_then(|t| t.lookup(&p).map(|id| t.alloc(id)))
                })
                .collect();
            for (i, (e, size)) in entries.iter().zip(&sizes).enumerate() {
                let note = if e.root == p {
                    String::new()
                } else {
                    format!("  (scan of {})", e.root.display())
                };
                match size {
                    Some(size) => row(
                        &mut out,
                        e.scanned_at,
                        *size,
                        sizes.get(i + 1).copied().flatten(),
                        &note,
                    ),
                    None => {
                        let _ = writeln!(
                            out,
                            "  {}  {:>9}  {:>10}{note}",
                            datetime(e.scanned_at),
                            ago(now - e.scanned_at),
                            "absent"
                        );
                    }
                }
            }
        }
    }
    emit(&out)
}

const HISTORY_NOTE: &str = "hogs keeps a history of its scans (at most one per 10 minutes, thinned out \
over time), so check again after it has scanned for a while, e.g. tomorrow.\n";

pub struct DiffOptions {
    pub since: Option<i64>,
    pub depth: usize,
    pub limit: usize,
    pub files: bool,
}

/// What changed below `path` since an earlier scan, largest change first.
pub fn diff(ctx: &mut Ctx, path: &Path, opts: &DiffOptions) -> Result<()> {
    let (tree, id) = ctx.session.resolve(path)?;
    let history = ctx
        .session
        .history
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the scan history is unavailable"))?;
    let target = tree.path(id);
    let now = staging::now();
    // The newest earlier scan: before this one, or at least `since` old.
    let cutoff = opts.since.map_or(tree.scanned_at() - 1, |s| now - s);
    let Some(entry) = history
        .covering(&target)
        .into_iter()
        .find(|e| e.scanned_at <= cutoff)
    else {
        let msg = match opts.since {
            Some(_) => format!(
                "No saved scan of {} that old yet; see `hogs history`.\n",
                target.display()
            ),
            None => format!(
                "No earlier scan of {} yet. {HISTORY_NOTE}",
                target.display()
            ),
        };
        return emit(&msg);
    };
    let old = history.load(&entry)?;
    let old_id = old.lookup(&target);
    let before = old_id.map_or(0, |o| old.alloc(o));
    let total = tree.alloc(id) as i64 - before as i64;
    let mut out = String::new();
    let table = ctx.output == Output::Table;
    if table {
        let _ = writeln!(
            out,
            "{}: {} now, {} since {} ({}){}",
            target.display(),
            human_size(tree.alloc(id)),
            signed(total),
            datetime(entry.scanned_at),
            ago(now - entry.scanned_at),
            if old_id.is_none() {
                ", when it did not exist"
            } else {
                ""
            },
        );
    }
    // Ignore changes too small to matter relative to the total.
    let threshold = MIN_FILE.max(total.unsigned_abs() / 100);
    let d = Differ {
        new: &tree,
        old: &old,
        threshold,
        limit: opts.limit,
        output: ctx.output,
    };
    let shown = if opts.files {
        d.files(&mut out, id, old_id)
    } else {
        d.tree(&mut out, Some(id), old_id, opts.depth.max(1), 0)
    };
    if shown == 0 && table {
        let _ = writeln!(out, "No changes of {} or more.", human_size(threshold));
    }
    emit(&out)
}

struct Differ<'a> {
    new: &'a Tree,
    old: &'a Tree,
    threshold: u64,
    limit: usize,
    output: Output,
}

impl Differ<'_> {
    fn path(&self, c: &diff::Change) -> PathBuf {
        match (c.new, c.old) {
            (Some(n), _) => self.new.path(n),
            (None, Some(o)) => self.old.path(o),
            (None, None) => PathBuf::new(),
        }
    }

    fn line(&self, out: &mut String, c: &diff::Change, indent: usize, is_dir: bool) {
        let path = self.path(c);
        if let Output::Paths { nul } = self.output {
            let _ = write!(out, "{}", path.display());
            out.push(if nul { '\0' } else { '\n' });
            return;
        }
        let tag = match (c.new, c.old) {
            (_, None) => "  [new]",
            (None, _) => "  [gone]",
            _ => "",
        };
        let slash = if is_dir { "/" } else { "" };
        let _ = writeln!(
            out,
            "{:indent$}{:>11} {:>10}  {}{slash}{tag}",
            "",
            signed(c.delta()),
            human_size(c.now),
            path.display()
        );
    }

    /// Changed entries as a tree; returns how many were shown. In path mode
    /// only the innermost shown entries are printed.
    fn tree(
        &self,
        out: &mut String,
        n: Option<NodeId>,
        o: Option<NodeId>,
        depth: usize,
        indent: usize,
    ) -> usize {
        let changes = diff::children(self.new, n, self.old, o);
        let big: Vec<_> = changes
            .iter()
            .filter(|c| c.delta().unsigned_abs() >= self.threshold)
            .collect();
        for c in big.iter().take(self.limit) {
            let is_dir = c.is_dir(self.new, self.old);
            let mut inner = String::new();
            let below = if depth > 1 && is_dir {
                self.tree(&mut inner, c.new, c.old, depth - 1, indent + 2)
            } else {
                0
            };
            if self.output == Output::Table || below == 0 {
                self.line(out, c, indent, is_dir);
            }
            out.push_str(&inner);
        }
        if big.len() > self.limit && self.output == Output::Table {
            let _ = writeln!(
                out,
                "{:indent$}… {} more changes",
                "",
                big.len() - self.limit
            );
        }
        big.len().min(self.limit)
    }

    /// The files that changed most, at any depth.
    fn files(&self, out: &mut String, n: NodeId, o: Option<NodeId>) -> usize {
        let (new, old) = (self.new, self.old);
        let mut changes = Vec::new();
        new.walk(n, |c, _| {
            if !new.is_dir(c) && new.alloc(c) >= MIN_FILE {
                let before = old.lookup(&new.path(c));
                changes.push(diff::Change {
                    new: Some(c),
                    old: before,
                    now: new.alloc(c),
                    before: before.map_or(0, |b| old.alloc(b)),
                });
            }
            true
        });
        if let Some(o) = o {
            old.walk(o, |c, _| {
                if !old.is_dir(c) && new.lookup(&old.path(c)).is_none() {
                    changes.push(diff::Change {
                        new: None,
                        old: Some(c),
                        now: 0,
                        before: old.alloc(c),
                    });
                }
                true
            });
        }
        changes.retain(|c| c.delta().unsigned_abs() >= self.threshold);
        changes.sort_by_key(|c| std::cmp::Reverse(c.delta().unsigned_abs()));
        for c in changes.iter().take(self.limit) {
            self.line(out, c, 0, false);
        }
        if changes.len() > self.limit && self.output == Output::Table {
            let _ = writeln!(out, "… {} more changed files", changes.len() - self.limit);
        }
        changes.len().min(self.limit)
    }
}

/// Paths from the arguments, or from stdin when none are given (or `-`).
pub fn input_paths(args: Vec<PathBuf>, nul: bool) -> Result<Vec<PathBuf>> {
    if !(args.is_empty() || args.len() == 1 && args[0] == Path::new("-")) {
        return Ok(args);
    }
    let mut input = Vec::new();
    io::stdin().lock().read_to_end(&mut input)?;
    let sep = if nul { b'\0' } else { b'\n' };
    Ok(input
        .split(|&b| b == sep)
        .map(|p| {
            if nul {
                p
            } else {
                p.strip_suffix(b"\r").unwrap_or(p)
            }
        })
        .filter(|p| !p.is_empty())
        .map(|p| hogs_core::tree::os_str_from_bytes(p).into_owned().into())
        .collect())
}

/// Stages paths for deletion. Returns false if any path was refused or skipped.
pub fn stage(ctx: &mut Ctx, paths: &[PathBuf], reason: &str, by: &str) -> Result<bool> {
    if reason.trim().is_empty() {
        anyhow::bail!("--reason must say why these can be deleted");
    }
    let now = staging::now();
    let mut out = String::new();
    let mut all_ok = true;
    let mut new_items = Vec::new();
    for p in paths {
        // Refuse protected paths before resolving, which may have to scan them.
        let homes = hogs_core::user::protected_homes();
        let refusal = canonical(p)
            .ok()
            .and_then(|c| Some((is_protected(&c, &homes)?, c)));
        if let Some((why, path)) = refusal {
            let _ = writeln!(out, "refused {}: it {why}", path.display());
            all_ok = false;
            continue;
        }
        let (tree, id) = match ctx.session.resolve(p) {
            Ok(found) => found,
            Err(e) => {
                let _ = writeln!(out, "skipped {}: {e}", p.display());
                all_ok = false;
                continue;
            }
        };
        let path = tree.path(id);
        new_items.push(StagedItem {
            path,
            alloc: tree.alloc(id),
            newest_mtime: tree.mtime(id),
            reason: reason.to_string(),
            staged_by: by.to_string(),
            staged_at: now,
        });
    }
    let store = StagingStore::open_default()?;
    let (count, total) = store.update(|items| {
        for item in new_items {
            let (path, alloc) = (item.path.clone(), item.alloc);
            let _ = match staging::stage(items, item) {
                StageOutcome::Added => {
                    writeln!(out, "staged {} ({})", path.display(), human_size(alloc))
                }
                StageOutcome::AddedReplacing(n) => writeln!(
                    out,
                    "staged {} ({}), replacing {n} staged items inside it",
                    path.display(),
                    human_size(alloc)
                ),
                StageOutcome::AlreadyCovered(by) => {
                    writeln!(
                        out,
                        "already staged: {} (via {})",
                        path.display(),
                        by.display()
                    )
                }
            };
        }
        (items.len(), items.iter().map(|i| i.alloc).sum::<u64>())
    })?;
    let _ = writeln!(
        out,
        "{count} items ({}) are staged. Nothing was deleted: the user reviews and applies them by \
running `hogs staged apply` in their own terminal.",
        human_size(total)
    );
    emit(&out)?;
    Ok(all_ok)
}
