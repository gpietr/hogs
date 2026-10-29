use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use hogs_core::fmt::human_size;
use hogs_core::query::{Filter, Kind};
use hogs_core::staging::{self, StagingStore};
use hogs_core::{Tree, flags};
use hogs_scan::{Backend, ScanOptions};

use crate::commands::Ctx;
use crate::report::{Output, parse_duration, parse_size};
use crate::session::Session;

mod apply;
mod commands;
mod report;
mod session;
mod tui;

const WORKFLOW: &str = "\
Workflow (for people and coding agents):
  1. hogs ls ~ -d 2             where the space goes, largest first
  2. hogs dirs PATH, hogs files PATH, hogs ext PATH, hogs find PATH --name ... --older 180d
                               drill down; look inside anything unfamiliar before judging it
     hogs diff PATH [--since 1d]   what grew (or shrank) since an earlier scan; hogs keeps a history
  3. hogs stage PATH... --reason TEXT
                               stage items the user agreed to (paths can also come from stdin:
                               hogs find ~ --name node_modules --type d --older 180d --paths | hogs stage --reason ...)
  4. hogs staged apply          the user reviews and deletes (to the trash by default),
                               or interactively: run `hogs` to browse, review and apply in the TUI

Rows show: on-disk size, % of the parent, file count, age of the newest change inside, and path.

Agents: never delete files yourself (no rm/del). Explain what each suggestion is, what deleting it
costs the user (a rebuild, a re-download, lost data) and whether a tool should clean it instead
(e.g. docker system prune, cargo clean). Stage what the user agrees to and let them run
`hogs staged apply`, which only works in an interactive terminal. Filesystem roots, the home
directory and OS directories can never be staged.

Scans are fast and cached for --max-age (default 10m); commands scan automatically when needed.";

#[derive(Parser)]
#[command(version, about, after_help = "Run `hogs --help` for the cleanup workflow.", after_long_help = WORKFLOW)]
struct Cli {
    /// Without a subcommand, hogs opens the interactive browser on the current directory.
    #[command(subcommand)]
    command: Option<Command>,
    /// Print only paths, one per line (for piping into `hogs stage`, xargs, ...).
    #[arg(long = "paths", global = true)]
    paths_only: bool,
    /// Like --paths but NUL-separated; also makes `hogs stage` read NUL-separated input.
    #[arg(short = '0', long = "null", global = true)]
    null: bool,
    /// Ignore cached scans and scan again.
    #[arg(long, global = true)]
    rescan: bool,
    /// Reuse cached scans younger than this (e.g. 30s, 10m, 1h).
    #[arg(long, global = true, default_value = "10m", value_parser = parse_duration)]
    max_age: i64,
}

#[derive(Subcommand)]
enum Command {
    /// Browse disk usage interactively, review staged items and apply them (default).
    Tui {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Scan a directory, cache the result and print a summary.
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Number of largest entries to list.
        #[arg(short = 'n', long, default_value_t = 20)]
        top: usize,
        /// Descend into other mounted filesystems.
        #[arg(long)]
        cross_mounts: bool,
        /// Worker threads (0 = auto).
        #[arg(long, default_value_t = 0)]
        threads: usize,
        #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
        backend: BackendArg,
        /// Print exact byte counts instead of human-readable sizes.
        #[arg(long)]
        bytes: bool,
    },
    /// List a directory's contents, largest first.
    Ls {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Levels to expand.
        #[arg(short, long, default_value_t = 1)]
        depth: usize,
        /// Entries per directory.
        #[arg(short = 'n', long, default_value_t = 25)]
        limit: usize,
    },
    /// The largest files below a directory.
    Files {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// The largest directories below a directory (nested results are expected).
    Dirs {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
        /// Maximum depth below PATH.
        #[arg(short, long, default_value_t = 4)]
        depth: usize,
    },
    /// On-disk size per file extension.
    Ext {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: usize,
    },
    /// Find entries by name, type, size and age. Matching directories are not searched further.
    Find {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Case-insensitive name pattern with * and ?, e.g. '*.iso' or node_modules.
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "type", value_enum)]
        kind: Option<KindArg>,
        /// Minimum on-disk size, e.g. 100M, 2G.
        #[arg(long, value_parser = parse_size)]
        min_size: Option<u64>,
        /// Only entries with nothing changed inside for this long, e.g. 90d, 1y.
        #[arg(long, value_parser = parse_duration)]
        older: Option<i64>,
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
    },
    /// Past scans: when, how big, and how they changed. With PATH, its size in each scan.
    History {
        path: Option<PathBuf>,
        /// Delete the whole history.
        #[arg(long)]
        clear: bool,
    },
    /// What changed since an earlier scan, largest change first: what is actively hogging space.
    Diff {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Compare with the newest scan at least this old (e.g. 1d, 1w); default: the previous scan.
        #[arg(long, value_parser = parse_duration)]
        since: Option<i64>,
        /// Levels to expand.
        #[arg(short, long, default_value_t = 3)]
        depth: usize,
        /// Entries per level (or files with --files).
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// List the files that changed most instead of a tree.
        #[arg(long)]
        files: bool,
    },
    /// Stage paths for deletion (reads paths from stdin if none are given). Deletes nothing.
    Stage {
        paths: Vec<PathBuf>,
        /// Why these can be deleted; shown to the user when they apply.
        #[arg(short, long)]
        reason: String,
        /// Who is staging (default: claude-code inside Claude Code, else user).
        #[arg(long)]
        by: Option<String>,
    },
    /// Review and apply staged deletions.
    Staged {
        #[command(subcommand)]
        action: Option<StagedAction>,
    },
}

#[derive(Subcommand)]
enum StagedAction {
    /// Show the staged items (default).
    List,
    /// Verify and delete the staged items after confirmation (interactive terminal only).
    Apply {
        /// Delete permanently instead of moving to the trash.
        #[arg(long)]
        permanent: bool,
    },
    /// Remove items from the list without deleting them.
    Remove { paths: Vec<PathBuf> },
    /// Empty the list without deleting anything.
    Clear,
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendArg {
    Auto,
    Generic,
    /// NTFS master file table (Windows, Administrator).
    Mft,
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    #[value(name = "f")]
    File,
    #[value(name = "d")]
    Dir,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("hogs: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns false when a command partly failed (exit code 2).
fn run() -> Result<bool> {
    let cli = Cli::parse();
    let output = match (cli.paths_only, cli.null) {
        (_, true) => Output::Paths { nul: true },
        (true, false) => Output::Paths { nul: false },
        _ => Output::Table,
    };
    let scan_opts = ScanOptions::default();
    let mut ctx = Ctx {
        session: Session::new(cli.max_age, cli.rescan, scan_opts),
        output,
    };
    let command = cli.command.unwrap_or(Command::Tui { path: ".".into() });
    if let Command::Tui { path } = &command {
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "the interactive browser needs a terminal; run `hogs --help` for the commands"
            );
        }
        tui::run(path)?;
        return Ok(true);
    }
    match command {
        Command::Tui { .. } => unreachable!(),
        Command::Scan {
            path,
            top,
            cross_mounts,
            threads,
            backend,
            bytes,
        } => {
            let opts = ScanOptions {
                one_filesystem: !cross_mounts,
                threads,
                backend: match backend {
                    BackendArg::Auto => Backend::Auto,
                    BackendArg::Generic => Backend::Generic,
                    BackendArg::Mft => Backend::Mft,
                },
            };
            let mut session = Session::new(cli.max_age, true, opts);
            let (tree, secs) = session.scan(&session::canonical(&path)?)?;
            print_summary(&tree, secs, top, bytes, session.last_backend)?;
        }
        Command::Ls { path, depth, limit } => commands::ls(&mut ctx, &path, depth, limit)?,
        Command::Files { path, limit } => commands::files(&mut ctx, &path, limit)?,
        Command::Dirs { path, limit, depth } => commands::dirs(&mut ctx, &path, limit, depth)?,
        Command::Ext { path, limit } => commands::ext(&mut ctx, &path, limit)?,
        Command::Find {
            path,
            name,
            kind,
            min_size,
            older,
            limit,
        } => {
            let filter = Filter {
                name,
                kind: match kind {
                    Some(KindArg::File) => Kind::File,
                    Some(KindArg::Dir) => Kind::Dir,
                    None => Kind::Any,
                },
                min_alloc: min_size.unwrap_or(0),
                modified_before: older.map(|secs| staging::now() - secs),
            };
            commands::find(&mut ctx, &path, &filter, limit)?;
        }
        Command::History { path, clear } => commands::history(&mut ctx, path.as_deref(), clear)?,
        Command::Diff {
            path,
            since,
            depth,
            limit,
            files,
        } => commands::diff(
            &mut ctx,
            &path,
            &commands::DiffOptions {
                since,
                depth,
                limit,
                files,
            },
        )?,
        Command::Stage { paths, reason, by } => {
            let paths = commands::input_paths(paths, cli.null)?;
            let by = by.unwrap_or_else(|| {
                if std::env::var_os("CLAUDECODE").is_some_and(|v| !v.is_empty()) {
                    "claude-code"
                } else {
                    "user"
                }
                .to_string()
            });
            return commands::stage(&mut ctx, &paths, &reason, &by);
        }
        Command::Staged { action } => {
            let store = StagingStore::open_default()?;
            match action.unwrap_or(StagedAction::List) {
                StagedAction::List => {
                    let items = store.load()?;
                    if items.is_empty() {
                        commands::emit("Nothing is staged.\n")?;
                    } else {
                        commands::emit(&apply::list_text(&items))?;
                    }
                }
                StagedAction::Apply { permanent } => apply::apply(&store, permanent)?,
                StagedAction::Remove { paths } => {
                    let paths: Vec<PathBuf> = paths
                        .iter()
                        // Staged items may be gone already, so fall back to the absolute path.
                        .map(|p| {
                            session::canonical(p)
                                .or_else(|_| std::path::absolute(p))
                                .unwrap_or_else(|_| p.clone())
                        })
                        .collect();
                    let removed = store.update(|items| {
                        let before = items.len();
                        items.retain(|i| !paths.contains(&i.path));
                        before - items.len()
                    })?;
                    println!("Removed {removed} items.");
                }
                StagedAction::Clear => {
                    store.update(Vec::clear)?;
                    println!("Cleared the staged list.");
                }
            }
        }
    }
    Ok(true)
}

fn print_summary(tree: &Tree, secs: f64, top: usize, bytes: bool, backend: &str) -> Result<()> {
    let fmt = |n: u64| if bytes { n.to_string() } else { human_size(n) };
    let root = Tree::ROOT;
    let (mut dirs, mut errors) = (0, 0);
    for id in 1..tree.len() as u32 {
        dirs += u64::from(tree.is_dir(id));
        errors += u64::from(tree.is_dir(id) && tree.flags(id) & flags::ERROR != 0);
    }
    let mut out = format!(
        "{}  {} on disk ({} apparent), {} files, {dirs} dirs, {errors} unreadable dirs, {secs:.2}s ({backend})\n",
        tree.root_path().display(),
        fmt(tree.alloc(root)),
        fmt(tree.size(root)),
        tree.files(root),
    );
    let total = tree.alloc(root).max(1);
    for id in tree.children_by_size(root).into_iter().take(top) {
        let suffix = if tree.is_dir(id) { "/" } else { "" };
        let mark = if tree.flags(id) & flags::OTHER_FS != 0 {
            "  [other fs, skipped]"
        } else {
            ""
        };
        out.push_str(&format!(
            "{:>10}  {:>5.1}%  {}{suffix}{mark}\n",
            fmt(tree.alloc(id)),
            tree.alloc(id) as f64 * 100.0 / total as f64,
            tree.name_lossy(id),
        ));
    }
    commands::emit(&out)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }
}
