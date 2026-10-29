//! Interactive terminal UI: browse the scan, see what agents staged and why,
//! stage or unstage items yourself, and apply deletions after confirming.

mod ui;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use hogs_core::fmt::human_size;
use hogs_core::protect::is_protected;
use hogs_core::query::{self, ExtStat};
use hogs_core::snapshot::SnapshotStore;
use hogs_core::staging::{self, StageOutcome, StagedItem, StagingStore};
use hogs_core::{NodeId, Progress, Tree};
use hogs_scan::ScanOptions;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::apply::{self, Outcome};
use crate::report::count;
use crate::session::canonical;

/// Reuse saved scans younger than this when starting.
const MAX_AGE: i64 = 600;
const MESSAGE_TIME: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Size,
    Oldest,
    Files,
    Name,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Size => Sort::Oldest,
            Sort::Oldest => Sort::Files,
            Sort::Files => Sort::Name,
            Sort::Name => Sort::Size,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Size => "size",
            Sort::Oldest => "oldest change first",
            Sort::Files => "file count",
            Sort::Name => "name",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Browse,
    Staged,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    Help,
    ConfirmApply,
}

/// What a background apply reports: each item with its outcome.
type ApplyResult = Result<Vec<(StagedItem, Outcome)>, String>;

/// A scan running in the background.
pub struct Loading {
    rx: Receiver<std::io::Result<Tree>>,
    pub progress: Arc<Progress>,
    pub started: Instant,
    pub root: PathBuf,
    /// Where to go once loaded: a directory, and optionally an entry in it to select.
    target: PathBuf,
    select: Option<Vec<u8>>,
}

/// Facts about a directory that need a walk, cached per node.
pub struct Details {
    pub largest: Vec<NodeId>,
    pub exts: Vec<ExtStat>,
}

pub struct App {
    pub tree: Option<Tree>,
    pub loading: Option<Loading>,
    pub error: Option<String>,
    pub dir: NodeId,
    pub entries: Vec<NodeId>,
    pub selected: usize,
    pub offset: usize,
    /// Rows visible in the list, updated while drawing.
    pub list_height: usize,
    pub sort: Sort,
    pub filter: String,
    pub filtering: bool,
    pub view: View,
    pub popup: Option<Popup>,
    pub store: StagingStore,
    pub staged: Vec<StagedItem>,
    staged_mtime: Option<SystemTime>,
    last_staged_check: Instant,
    pub staged_selected: usize,
    pub staged_offset: usize,
    /// Staged items left out of the next apply.
    pub skip: HashSet<PathBuf>,
    /// Exactly the items shown in the open apply dialog.
    pending: Vec<StagedItem>,
    /// Why items were kept by the last apply.
    pub notes: HashMap<PathBuf, String>,
    applying: Option<Receiver<ApplyResult>>,
    pub busy: Option<&'static str>,
    message: Option<(String, Instant)>,
    details: HashMap<NodeId, Details>,
    pub home: Option<PathBuf>,
    quit: bool,
}

fn summary(items: &[StagedItem]) -> (usize, u64) {
    (items.len(), items.iter().map(|i| i.alloc).sum())
}

pub fn run(path: &Path) -> Result<()> {
    let mut app = App::new(path)?;
    ratatui::run(|terminal| -> Result<()> {
        while !app.quit {
            app.tick();
            terminal.draw(|f| ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(150))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                app.on_key(key);
            }
        }
        Ok(())
    })
}

fn start_scan(root: PathBuf, target: PathBuf, select: Option<Vec<u8>>) -> Loading {
    let progress = Arc::new(Progress::default());
    let (tx, rx) = mpsc::channel();
    let (p, r) = (progress.clone(), root.clone());
    std::thread::spawn(move || {
        let result = hogs_scan::scan(&r, &ScanOptions::default(), &p);
        if let Ok(tree) = &result
            && let Ok(store) = SnapshotStore::open_default()
        {
            let _ = store.save(tree);
        }
        let _ = tx.send(result);
    });
    Loading {
        rx,
        progress,
        started: Instant::now(),
        root,
        target,
        select,
    }
}

impl App {
    fn new(path: &Path) -> Result<Self> {
        let path = canonical(path)?;
        let (target, select) = if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            (path.clone(), None)
        } else {
            let parent = path.parent().unwrap_or(&path).to_path_buf();
            (
                parent,
                path.file_name().map(|n| n.as_encoded_bytes().to_vec()),
            )
        };
        let mut app = App::blank(StagingStore::open_default()?);
        let cached = SnapshotStore::open_default()
            .ok()
            .and_then(|s| s.load_covering(&target, MAX_AGE, staging::now()))
            .filter(|t| t.lookup_scanned(&target).is_some());
        match cached {
            Some(tree) => app.set_tree(tree, &target, select),
            None => {
                app.loading = Some(start_scan(
                    hogs_scan::preferred_root(&target),
                    target,
                    select,
                ))
            }
        }
        Ok(app)
    }

    /// An app with no tree loaded yet.
    fn blank(store: StagingStore) -> Self {
        let mut app = App {
            tree: None,
            loading: None,
            error: None,
            dir: Tree::ROOT,
            entries: Vec::new(),
            selected: 0,
            offset: 0,
            list_height: 20,
            sort: Sort::Size,
            filter: String::new(),
            filtering: false,
            view: View::Browse,
            popup: None,
            store,
            staged: Vec::new(),
            staged_mtime: None,
            last_staged_check: Instant::now(),
            staged_selected: 0,
            staged_offset: 0,
            skip: HashSet::new(),
            pending: Vec::new(),
            notes: HashMap::new(),
            applying: None,
            busy: None,
            message: None,
            details: HashMap::new(),
            home: hogs_core::user::home(),
            quit: false,
        };
        app.reload_staged();
        app
    }

    pub fn message(&self) -> Option<&str> {
        self.message
            .as_ref()
            .filter(|(_, at)| at.elapsed() < MESSAGE_TIME)
            .map(|(m, _)| m.as_str())
    }

    fn say(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    /// Background work and file watching, once per frame.
    fn tick(&mut self) {
        if let Some(loading) = &self.loading
            && let Ok(result) = loading.rx.try_recv()
        {
            let loading = self.loading.take().unwrap();
            match result {
                Ok(tree) => self.set_tree(tree, &loading.target, loading.select),
                Err(e) => {
                    let msg = format!("cannot scan {}: {e}", loading.root.display());
                    if self.tree.is_none() {
                        self.error = Some(msg);
                    } else {
                        self.say(msg);
                    }
                }
            }
        }
        if let Some(rx) = &self.applying
            && let Ok(result) = rx.try_recv()
        {
            self.applying = None;
            self.busy = None;
            self.finish_apply(result);
        }
        if self.last_staged_check.elapsed() > Duration::from_secs(1) {
            self.last_staged_check = Instant::now();
            let mtime = std::fs::metadata(self.store.path())
                .and_then(|m| m.modified())
                .ok();
            if mtime != self.staged_mtime {
                self.reload_staged();
            }
        }
    }

    fn reload_staged(&mut self) {
        self.staged_mtime = std::fs::metadata(self.store.path())
            .and_then(|m| m.modified())
            .ok();
        match self.store.load() {
            Ok(items) => self.staged = items,
            Err(e) => self.say(format!("cannot read the staged list: {e}")),
        }
        self.skip
            .retain(|p| self.staged.iter().any(|i| &i.path == p));
        self.staged_selected = self
            .staged_selected
            .min(self.staged.len().saturating_sub(1));
    }

    fn set_tree(&mut self, tree: Tree, target: &Path, select: Option<Vec<u8>>) {
        let dir = target
            .ancestors()
            .find_map(|p| tree.lookup(p))
            .unwrap_or(Tree::ROOT);
        self.tree = Some(tree);
        self.details.clear();
        self.dir = dir;
        self.selected = 0;
        self.offset = 0;
        self.refresh_entries();
        if let Some(name) = select {
            self.select_where(|t, id| t.name(id) == name.as_slice());
        }
    }

    fn select_where(&mut self, pred: impl Fn(&Tree, NodeId) -> bool) {
        let Some(tree) = &self.tree else { return };
        if let Some(i) = self.entries.iter().position(|&id| pred(tree, id)) {
            self.selected = i;
        }
    }

    pub fn refresh_entries(&mut self) {
        let Some(tree) = &self.tree else { return };
        let needle = self.filter.to_lowercase();
        let mut entries: Vec<NodeId> = tree
            .children(self.dir)
            .filter(|&id| needle.is_empty() || tree.name_lossy(id).to_lowercase().contains(&needle))
            .collect();
        match self.sort {
            Sort::Size => {
                entries.sort_by_key(|&id| std::cmp::Reverse((tree.alloc(id), tree.size(id))))
            }
            Sort::Oldest => entries.sort_by_key(|&id| tree.mtime(id)),
            Sort::Files => entries.sort_by_key(|&id| std::cmp::Reverse(tree.files(id))),
            Sort::Name => entries.sort_by_cached_key(|&id| tree.name_lossy(id).to_lowercase()),
        }
        self.entries = entries;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    pub fn selected_node(&self) -> Option<NodeId> {
        self.entries.get(self.selected).copied()
    }

    /// Largest files and extension breakdown of a directory, computed once.
    pub fn details(&mut self, id: NodeId) -> Option<&Details> {
        let tree = self.tree.as_ref()?;
        if !tree.is_dir(id) {
            return None;
        }
        Some(self.details.entry(id).or_insert_with(|| Details {
            largest: query::largest_files(tree, id, 8),
            exts: query::by_extension(tree, id).into_iter().take(6).collect(),
        }))
    }

    /// The staged item covering `path` (itself or an ancestor), if any.
    pub fn staged_cover(&self, path: &Path) -> Option<&StagedItem> {
        self.staged.iter().find(|i| path.starts_with(&i.path))
    }

    /// Staged items strictly inside `path`.
    pub fn staged_inside(&self, path: &Path) -> impl Iterator<Item = &StagedItem> {
        self.staged
            .iter()
            .filter(move |i| i.path != path && i.path.starts_with(path))
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(popup) = self.popup {
            self.on_popup_key(popup, key.code);
            return;
        }
        if self.filtering {
            self.on_filter_key(key.code);
            return;
        }
        if self.tree.is_none() {
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                self.quit = true;
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
            KeyCode::Tab | KeyCode::Char('S') => {
                self.view = if self.view == View::Browse {
                    View::Staged
                } else {
                    View::Browse
                };
            }
            KeyCode::Char('a') => self.ask_apply(),
            KeyCode::Char('r') => self.rescan(),
            _ => match self.view {
                View::Browse => self.on_browse_key(key.code),
                View::Staged => self.on_staged_key(key.code),
            },
        }
    }

    fn on_popup_key(&mut self, popup: Popup, code: KeyCode) {
        match (popup, code) {
            (Popup::ConfirmApply, KeyCode::Char('y' | 'Y')) => {
                self.popup = None;
                self.start_apply();
            }
            (Popup::ConfirmApply, KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Char('q')) => {
                self.popup = None;
                self.pending.clear();
                self.say("Nothing was deleted.");
            }
            (Popup::Help, _) => self.popup = None,
            _ => {}
        }
    }

    fn on_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => self.filtering = false,
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.clear();
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => return,
        }
        self.selected = 0;
        self.refresh_entries();
    }

    fn move_selection(selected: &mut usize, len: usize, page: usize, code: KeyCode) -> bool {
        let last = len.saturating_sub(1);
        *selected = match code {
            KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (*selected + 1).min(last),
            KeyCode::PageUp => selected.saturating_sub(page),
            KeyCode::PageDown => (*selected + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => return false,
        };
        true
    }

    fn on_browse_key(&mut self, code: KeyCode) {
        if Self::move_selection(
            &mut self.selected,
            self.entries.len(),
            self.list_height.max(1),
            code,
        ) {
            return;
        }
        match code {
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_selected(),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.go_up(),
            KeyCode::Char('s') | KeyCode::Char(' ') => self.toggle_stage(),
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refresh_entries();
            }
            KeyCode::Char('o') => {
                self.sort = self.sort.next();
                let current = self.selected_node();
                self.refresh_entries();
                if let Some(id) = current {
                    self.select_where(|_, n| n == id);
                }
            }
            _ => {}
        }
    }

    fn on_staged_key(&mut self, code: KeyCode) {
        if Self::move_selection(
            &mut self.staged_selected,
            self.staged.len(),
            self.list_height.max(1),
            code,
        ) {
            return;
        }
        let Some(item) = self.staged.get(self.staged_selected).cloned() else {
            if code == KeyCode::Esc {
                self.view = View::Browse;
            }
            return;
        };
        match code {
            KeyCode::Char(' ') => {
                if !self.skip.remove(&item.path) {
                    self.skip.insert(item.path);
                }
            }
            KeyCode::Char('u') | KeyCode::Delete => self.unstage(&item.path),
            KeyCode::Enter => self.jump_to(&item.path),
            KeyCode::Esc => self.view = View::Browse,
            _ => {}
        }
    }

    fn enter_dir(&mut self, dir: NodeId) {
        self.dir = dir;
        self.selected = 0;
        self.offset = 0;
        self.filter.clear();
        self.refresh_entries();
    }

    fn open_selected(&mut self) {
        if let (Some(tree), Some(id)) = (&self.tree, self.selected_node())
            && tree.is_dir(id)
        {
            self.enter_dir(id);
        }
    }

    fn go_up(&mut self) {
        let Some(parent) = self.tree.as_ref().and_then(|t| t.parent(self.dir)) else {
            self.say("This is the top of the scan.");
            return;
        };
        let came_from = self.dir;
        self.enter_dir(parent);
        self.select_where(|_, id| id == came_from);
    }

    /// Shows `path` in the browser: opens its parent and selects it.
    fn jump_to(&mut self, path: &Path) {
        let Some(tree) = &self.tree else { return };
        let Some(id) = tree.lookup(path) else {
            self.say("Not in the current scan (press r to rescan).");
            return;
        };
        let parent = tree.parent(id).unwrap_or(Tree::ROOT);
        self.view = View::Browse;
        self.enter_dir(parent);
        self.select_where(|_, n| n == id);
    }

    fn toggle_stage(&mut self) {
        let (Some(tree), Some(id)) = (&self.tree, self.selected_node()) else {
            return;
        };
        let path = tree.path(id);
        if self.staged.iter().any(|i| i.path == path) {
            self.unstage(&path);
            return;
        }
        if let Some(cover) = self.staged_cover(&path) {
            let msg = format!(
                "Already staged as part of {}",
                ui::tilde(&cover.path, self.home.as_deref())
            );
            self.say(msg);
            return;
        }
        if let Some(why) = is_protected(&path, &hogs_core::user::protected_homes()) {
            self.say(format!("Can't stage this: it {why}."));
            return;
        }
        let item = StagedItem {
            alloc: tree.alloc(id),
            newest_mtime: tree.mtime(id),
            reason: "selected in the hogs TUI".into(),
            staged_by: "user".into(),
            staged_at: staging::now(),
            path,
        };
        let name = tree.name_lossy(id).into_owned();
        let size = human_size(item.alloc);
        match self.store.update(|items| staging::stage(items, item)) {
            Ok(StageOutcome::AddedReplacing(n)) => self.say(format!(
                "Staged {name} ({size}), replacing {n} staged items inside it."
            )),
            Ok(_) => self.say(format!(
                "Staged {name} ({size}). Press a to apply, Tab to review."
            )),
            Err(e) => self.say(format!("Cannot update the staged list: {e}")),
        }
        self.reload_staged();
    }

    fn unstage(&mut self, path: &Path) {
        match self.store.update(|items| items.retain(|i| i.path != path)) {
            Ok(()) => self.say(format!(
                "Unstaged {}",
                ui::tilde(path, self.home.as_deref())
            )),
            Err(e) => self.say(format!("Cannot update the staged list: {e}")),
        }
        self.reload_staged();
    }

    fn rescan(&mut self) {
        if self.loading.is_some() {
            return;
        }
        let Some(tree) = &self.tree else { return };
        let target = tree.path(self.dir);
        let select = self.selected_node().map(|id| tree.name(id).to_vec());
        self.loading = Some(start_scan(tree.root_path().to_path_buf(), target, select));
    }

    fn to_apply(&self) -> Vec<StagedItem> {
        self.staged
            .iter()
            .filter(|i| !self.skip.contains(&i.path))
            .cloned()
            .collect()
    }

    fn ask_apply(&mut self) {
        if self.applying.is_some() {
            return;
        }
        if self.to_apply().is_empty() {
            self.say("Nothing to apply: stage items with s, or include them with Space in the staged view.");
            return;
        }
        // Freeze what the dialog shows: the staged list keeps reloading (an agent may
        // stage more meanwhile), and only what the user saw may be applied.
        self.pending = self.to_apply();
        self.popup = Some(Popup::ConfirmApply);
    }

    /// Items and total size that the next apply would include.
    pub fn apply_summary(&self) -> (usize, u64) {
        summary(&self.to_apply())
    }

    /// Items and total size the open confirmation dialog is about.
    pub fn pending_summary(&self) -> (usize, u64) {
        summary(&self.pending)
    }

    fn start_apply(&mut self) {
        // What the user confirmed, minus anything unstaged since.
        let items: Vec<StagedItem> = std::mem::take(&mut self.pending)
            .into_iter()
            .filter(|p| self.staged.iter().any(|i| i.path == p.path))
            .collect();
        if items.is_empty() {
            self.say("Nothing was deleted: the confirmed items are no longer staged.");
            return;
        }
        let store_path = self.store.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let store = StagingStore::at(store_path);
            let result = apply::apply_items(&store, &items, false)
                .map(|outcomes| items.into_iter().zip(outcomes).collect())
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.applying = Some(rx);
        self.busy = Some("Checking and moving items to the trash…");
    }

    fn finish_apply(&mut self, result: ApplyResult) {
        let results = match result {
            Ok(r) => r,
            Err(e) => {
                self.say(format!("Apply failed: {e}"));
                return;
            }
        };
        let (mut deleted, mut freed, mut kept) = (0, 0, 0);
        for (item, outcome) in results {
            match outcome {
                Outcome::Deleted => {
                    deleted += 1;
                    freed += item.alloc;
                }
                Outcome::Gone => {}
                Outcome::Kept(why) => {
                    kept += 1;
                    self.notes.insert(item.path, why);
                }
            }
        }
        let mut msg = format!(
            "Moved {} ({}) to the trash",
            count(deleted, "item", "items"),
            human_size(freed)
        );
        if kept > 0 {
            msg.push_str(&format!(
                "; kept {} (see the staged view)",
                count(kept, "item", "items")
            ));
        }
        msg.push_str(". Space is freed once the trash is emptied.");
        self.say(msg);
        self.reload_staged();
        self.rescan();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 26)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    #[test]
    fn browse_stage_review_and_cancel_apply() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("web/node_modules/react")).unwrap();
        fs::create_dir_all(root.join("media")).unwrap();
        fs::write(
            root.join("web/node_modules/react/index.js"),
            vec![1u8; 300_000],
        )
        .unwrap();
        fs::write(root.join("web/package.json"), b"{}").unwrap();
        fs::write(root.join("media/movie.mkv"), vec![2u8; 900_000]).unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = StagingStore::at(data.path().join("staged.json"));
        let agent_item = StagedItem {
            path: root.join("web/node_modules"),
            alloc: 300_000,
            newest_mtime: staging::now(),
            reason: "JS deps; npm install restores them".into(),
            staged_by: "claude-code".into(),
            staged_at: staging::now(),
        };
        store
            .update(|items| staging::stage(items, agent_item))
            .unwrap();

        let tree = hogs_scan::scan(&root, &ScanOptions::default(), &Progress::default()).unwrap();
        let mut app = App::blank(store);
        app.home = None;
        app.set_tree(tree, &root, None);

        // Largest first: media, then web (which contains the agent's staged item).
        let s = screen(&mut app);
        println!("{s}\n");
        assert!(s.contains("media/"), "{s}");
        let web_line = s.lines().find(|l| l.contains("web/")).unwrap();
        assert!(web_line.contains('•'), "{web_line}");

        // Open web/: node_modules is marked staged and the details show the agent's reason.
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        let s = screen(&mut app);
        println!("{s}\n");
        assert!(
            s.lines()
                .any(|l| l.contains('✗') && l.contains("node_modules/")),
            "{s}"
        );
        assert!(s.contains("staged by claude-code"), "{s}");
        assert!(s.contains("npm install restores"), "{s}");

        // Back up and stage media/ ourselves.
        press(&mut app, KeyCode::Left);
        assert_eq!(
            app.tree
                .as_ref()
                .unwrap()
                .name(app.selected_node().unwrap()),
            b"web"
        );
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.staged.len(), 2);
        assert!(
            app.staged
                .iter()
                .any(|i| i.staged_by == "user" && i.path.ends_with("media"))
        );

        // Staged view lists both with their authors; Space leaves one out of the apply.
        press(&mut app, KeyCode::Tab);
        let s = screen(&mut app);
        println!("{s}\n");
        assert!(s.contains("claude-code") && s.contains("user"), "{s}");
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.apply_summary().0, 1);

        // Apply asks first; declining deletes nothing.
        press(&mut app, KeyCode::Char('a'));
        let s = screen(&mut app);
        println!("{s}\n");
        assert!(s.contains("Move 1 item ("), "{s}");
        // An agent stages more while the dialog is open: the dialog keeps showing
        // what the user is confirming, and only that would be applied.
        let late = StagedItem {
            path: root.join("web/package.json"),
            alloc: 4096,
            newest_mtime: staging::now(),
            reason: "staged while the dialog was open".into(),
            staged_by: "claude-code".into(),
            staged_at: staging::now(),
        };
        app.store
            .update(|items| staging::stage(items, late))
            .unwrap();
        app.reload_staged();
        assert_eq!(app.staged.len(), 3);
        assert_eq!(app.pending_summary().0, 1);
        assert!(app.pending.iter().all(|i| i.path.ends_with("media")));
        let s = screen(&mut app);
        assert!(s.contains("Move 1 item ("), "{s}");
        press(&mut app, KeyCode::Char('n'));
        assert!(app.popup.is_none());
        assert!(app.pending.is_empty());
        assert!(root.join("media/movie.mkv").exists() && root.join("web/node_modules").exists());

        // Unstage from the staged view.
        press(&mut app, KeyCode::Char('u'));
        assert_eq!(app.staged.len(), 2);
    }

    #[test]
    fn bar_is_not_coloured_on_the_selected_row() {
        use ratatui::style::Color;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big"), vec![0u8; 800_000]).unwrap();
        fs::write(dir.path().join("small"), vec![0u8; 400_000]).unwrap();
        let data = tempfile::tempdir().unwrap();
        let tree =
            hogs_scan::scan(dir.path(), &ScanOptions::default(), &Progress::default()).unwrap();
        let mut app = App::blank(StagingStore::at(data.path().join("staged.json")));
        app.set_tree(tree, dir.path(), None);
        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Rows start at y=2 (header line, list border); the bar column starts after the mark and size.
        let bar_fg = |y: u16| (13..23).map(|x| buf[(x, y)].fg).collect::<Vec<_>>();
        assert!(
            !bar_fg(2).contains(&Color::Cyan),
            "selected row: {:?}",
            bar_fg(2)
        );
        assert!(
            bar_fg(3).contains(&Color::Cyan),
            "other row: {:?}",
            bar_fg(3)
        );
    }

    /// Prints the browser on a folder, for README screenshots. The largest
    /// `node_modules` inside is shown as staged by an agent.
    /// `HOGS_SCREENSHOT=<dir> cargo test -p hogs readme_screenshot -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn readme_screenshot() {
        let Some(dir) = std::env::var_os("HOGS_SCREENSHOT") else {
            return;
        };
        let root = dunce::canonicalize(dir).unwrap();
        let tree = hogs_scan::scan(&root, &ScanOptions::default(), &Progress::default()).unwrap();
        let filter = hogs_core::query::Filter {
            name: Some("node_modules".into()),
            ..Default::default()
        };
        let modules = hogs_core::query::find(&tree, Tree::ROOT, &filter, 1).matches[0];
        let data = tempfile::tempdir().unwrap();
        let store = StagingStore::at(data.path().join("staged.json"));
        let item = StagedItem {
            path: tree.path(modules),
            alloc: tree.alloc(modules),
            newest_mtime: tree.mtime(modules),
            reason: "JS dependencies of an app untouched for 2 years; npm install restores them"
                .into(),
            staged_by: "claude-code".into(),
            staged_at: staging::now(),
        };
        store.update(|items| staging::stage(items, item)).unwrap();
        let mut app = App::blank(store);
        app.home = root.parent().map(Path::to_path_buf);
        app.set_tree(tree, &root, None);
        let parent = app.tree.as_ref().unwrap().parent(modules).unwrap();
        app.enter_dir(parent);
        app.select_where(|_, id| id == modules);
        let mut terminal = Terminal::new(TestBackend::new(124, 20)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            println!(
                "{}",
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn protected_paths_cannot_be_staged() {
        let data = tempfile::tempdir().unwrap();
        let tree = hogs_scan::scan(
            Path::new("/etc"),
            &ScanOptions::default(),
            &Progress::default(),
        )
        .unwrap();
        let mut app = App::blank(StagingStore::at(data.path().join("staged.json")));
        app.set_tree(tree, Path::new("/etc"), None);
        press(&mut app, KeyCode::Char('s'));
        assert!(app.staged.is_empty());
        assert!(
            app.message().unwrap().contains("system directory"),
            "{:?}",
            app.message()
        );
    }
}
