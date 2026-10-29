//! Query and staging subcommands. Each prints plain text to stdout; with
//! `--paths`/`-0` row-based commands print only paths, for piping.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use hogs_core::Tree;
use hogs_core::fmt::human_size;
use hogs_core::protect::is_protected;
use hogs_core::query::{self, Filter};
use hogs_core::staging::{self, StageOutcome, StagedItem, StagingStore};

use crate::report::{Output, Rows};
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
