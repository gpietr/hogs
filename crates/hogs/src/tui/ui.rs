//! Drawing. Lists render only their visible window, so directories with
//! hundreds of thousands of entries stay fast.

use std::path::Path;

use hogs_core::fmt::human_size;
use hogs_core::{Tree, flags};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Gauge, Paragraph, Row, Table, Wrap};

use super::{App, Popup, View};
use crate::report::{age, count};

const STAGED: Color = Color::Red;
const CONTAINS: Color = Color::Yellow;
const BAR: Color = Color::Cyan;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, header, app);
    if app.tree.is_none() {
        draw_loading(f, body, app);
    } else {
        let [list, details] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
        match app.view {
            View::Browse => {
                draw_list(f, list, app);
                draw_details(f, details, app);
            }
            View::Staged => {
                draw_staged(f, list, app);
                draw_staged_details(f, details, app);
            }
        }
    }
    draw_footer(f, footer, app);
    match app.popup {
        Some(Popup::Help) => draw_help(f),
        Some(Popup::ConfirmApply) => draw_confirm(f, app),
        None => {}
    }
    if let Some(progress) = &app.apply_progress {
        draw_apply_progress(f, &progress.lock().unwrap(), app.home.as_deref());
    }
}

/// Shows paths under the home directory as `~/...`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `YYYY-MM-DD` for a Unix timestamp (UTC).
pub fn date(ts: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Characters the terminal can draw.
pub struct Glyphs {
    /// Eighth blocks for fine-grained bars; otherwise full and half blocks only.
    pub eighths: bool,
    pub staged: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    eighths: true,
    staged: "✗",
};
/// The classic Windows console's fonts (Consolas, Lucida Console) lack the
/// eighth blocks and ✗, which show as `?`; they do have █, ▌, × and •.
const BASIC: Glyphs = Glyphs {
    eighths: false,
    staged: "×",
};

pub fn glyphs() -> &'static Glyphs {
    static GLYPHS: std::sync::OnceLock<&'static Glyphs> = std::sync::OnceLock::new();
    GLYPHS.get_or_init(|| pick_glyphs(cfg!(windows), |v| std::env::var(v).ok()))
}

/// `HOGS_GLYPHS=basic|unicode` overrides; on Windows only terminals known to
/// draw Unicode well (Windows Terminal, VS Code and others setting TERM_PROGRAM)
/// get the full set.
fn pick_glyphs(windows: bool, env: impl Fn(&str) -> Option<String>) -> &'static Glyphs {
    match env("HOGS_GLYPHS").as_deref() {
        Some("basic") => &BASIC,
        Some("unicode") => &UNICODE,
        _ if !windows || env("WT_SESSION").is_some() || env("TERM_PROGRAM").is_some() => &UNICODE,
        _ => &BASIC,
    }
}

/// A size bar: eighth-block resolution where the terminal can draw it, half blocks otherwise.
fn bar(fraction: f64, width: usize, eighths: bool) -> String {
    const PARTS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let steps = if eighths { 8 } else { 2 };
    let units = (fraction.clamp(0.0, 1.0) * (width * steps) as f64).round() as usize;
    let mut s: String = "█".repeat(units / steps);
    if !units.is_multiple_of(steps) {
        s.push(PARTS[units % steps * 8 / steps]);
    }
    format!("{s:<width$}")
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(" hogs ", Style::new().bold().reversed())];
    if let Some(tree) = &app.tree {
        let dir = app.dir;
        spans.push(Span::raw(format!(
            "  {}  {} on disk · {} files · scanned {} ago · sort: {}",
            tilde(&tree.path(dir), app.home.as_deref()),
            human_size(tree.alloc(dir)),
            tree.files(dir),
            short_age(hogs_core::staging::now() - tree.scanned_at()),
            app.sort.label(),
        )));
        if app.loading.is_some() {
            spans.push(Span::styled("  rescanning…", Style::new().fg(CONTAINS)));
        }
    }
    f.render_widget(Line::from(spans), area);
}

fn short_age(secs: i64) -> String {
    match secs {
        ..60 => format!("{}s", secs.max(0)),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

fn draw_loading(f: &mut Frame, area: Rect, app: &App) {
    let text = if let Some(err) = &app.error {
        Text::from(vec![
            Line::from(err.as_str()).fg(STAGED),
            Line::from("Press q to quit."),
        ])
    } else if let Some(l) = &app.loading {
        let p = l.progress.snapshot();
        Text::from(vec![
            Line::from(format!("Scanning {}", tilde(&l.root, app.home.as_deref()))).bold(),
            Line::from(format!(
                "{} files, {} dirs, {} so far ({:.1}s)",
                p.files,
                p.dirs,
                human_size(p.bytes),
                l.started.elapsed().as_secs_f64()
            )),
        ])
    } else {
        Text::from("")
    };
    let area = centered(area, 70, 4);
    f.render_widget(Paragraph::new(text).centered(), area);
}

/// Keeps `selected` visible and returns the window start.
fn scroll(offset: &mut usize, selected: usize, height: usize) -> usize {
    if selected < *offset {
        *offset = selected;
    } else if height > 0 && selected >= *offset + height {
        *offset = selected + 1 - height;
    }
    *offset
}

fn draw_list(f: &mut Frame, area: Rect, app: &mut App) {
    let tree = app.tree.as_ref().unwrap();
    let height = area.height.saturating_sub(2) as usize;
    app.list_height = height;
    let start = scroll(&mut app.offset, app.selected, height);
    let total = tree.alloc(app.dir).max(1);
    let now = hogs_core::staging::now();
    let rows: Vec<Row> = app.entries[start..(start + height).min(app.entries.len())]
        .iter()
        .enumerate()
        .map(|(i, &id)| {
            let selected = start + i == app.selected;
            let path = tree.path(id);
            let (mark, mark_style) = if app.staged_cover(&path).is_some() {
                (glyphs().staged, Style::new().fg(STAGED).bold())
            } else if app.staged_inside(&path).next().is_some() {
                ("•", Style::new().fg(CONTAINS))
            } else {
                (" ", Style::new())
            };
            let fraction = tree.alloc(id) as f64 / total as f64;
            let mut name = tree.name_lossy(id).into_owned();
            let mut name_style = Style::new();
            if tree.is_dir(id) {
                name.push('/');
                name_style = name_style.bold();
            }
            if tree.flags(id) & flags::SYMLINK != 0 {
                name.push_str(" →");
            }
            if tree.flags(id) & (flags::ERROR | flags::OTHER_FS) != 0 {
                name.push_str(" !");
            }
            let row = Row::new(vec![
                Cell::from(mark).style(mark_style),
                Cell::from(format!("{:>9}", human_size(tree.alloc(id)))),
                // Reverse video would turn a coloured bar into a solid block on the selected row.
                Cell::from(bar(fraction, 10, glyphs().eighths)).style(if selected {
                    Style::new()
                } else {
                    Style::new().fg(BAR)
                }),
                Cell::from(format!("{:>5.1}%", fraction * 100.0)),
                Cell::from(format!("{:>6}", age(tree.mtime(id), now))),
                Cell::from(name).style(name_style),
            ]);
            if selected {
                row.style(Style::new().reversed())
            } else {
                row
            }
        })
        .collect();
    let mut title = format!(" {} ", tree.name_lossy(app.dir));
    if app.dir == Tree::ROOT {
        title = format!(" {} ", tilde(tree.root_path(), app.home.as_deref()));
    }
    if !app.filter.is_empty() || app.filtering {
        title.push_str(&format!(
            "[filter: {}{}] ",
            app.filter,
            if app.filtering { "_" } else { "" }
        ));
    }
    let block = Block::bordered().title(title).title_bottom(format!(
        " {}/{} ",
        (app.selected + 1).min(app.entries.len()),
        app.entries.len()
    ));
    let widths = [
        Constraint::Length(1),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Fill(1),
    ];
    let mut table = Table::new(rows, widths).block(block);
    if app.entries.is_empty() {
        table = table.header(Row::new(vec!["", "", "", "", "", "(empty)"]));
    }
    f.render_widget(table, area);
}

fn kv<'a>(key: &'a str, value: String) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{key:<14}"), Style::new().dim()),
        Span::raw(value),
    ])
}

fn draw_details(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(id) = app.selected_node() else {
        f.render_widget(Block::bordered().title(" details "), area);
        return;
    };
    let now = hogs_core::staging::now();
    let home = app.home.clone();
    let mut lines: Vec<Line> = Vec::new();
    {
        let tree = app.tree.as_ref().unwrap();
        let path = tree.path(id);
        lines.push(Line::from(tree.name_lossy(id).into_owned()).bold());
        lines.push(Line::from(tilde(&path, home.as_deref())).dim());
        lines.push(Line::default());
        lines.push(kv(
            "on disk",
            format!(
                "{} ({} apparent)",
                human_size(tree.alloc(id)),
                human_size(tree.size(id))
            ),
        ));
        if tree.is_dir(id) {
            lines.push(kv(
                "files",
                format!(
                    "{} · {} directly inside",
                    tree.files(id),
                    count(tree.children(id).len(), "entry", "entries")
                ),
            ));
        }
        let changed = match age(tree.mtime(id), now).as_str() {
            "today" => "today".to_string(),
            a => format!("{a} ago"),
        };
        let label = if tree.is_dir(id) {
            "newest change"
        } else {
            "modified"
        };
        lines.push(kv(label, format!("{changed} ({})", date(tree.mtime(id)))));
        let fl = tree.flags(id);
        let notes: Vec<&str> = [
            (flags::SYMLINK, "symlink (not followed)"),
            (flags::HARDLINK_DUP, "hardlink, size counted elsewhere"),
            (flags::ERROR, "partially unreadable"),
            (flags::OTHER_FS, "other filesystem, not scanned"),
        ]
        .iter()
        .filter(|(bit, _)| fl & bit != 0)
        .map(|&(_, s)| s)
        .collect();
        if !notes.is_empty() {
            lines.push(kv("note", notes.join(", ")).fg(CONTAINS));
        }
        lines.push(Line::default());
        if let Some(item) = app.staged_cover(&path) {
            let what = if item.path == path {
                "staged".to_string()
            } else {
                format!("staged as part of {}", tilde(&item.path, home.as_deref()))
            };
            lines.push(
                Line::from(format!("{} {what} by {}", glyphs().staged, item.staged_by))
                    .fg(STAGED)
                    .bold(),
            );
            lines.push(Line::from(format!("  “{}”", item.reason)));
            lines.push(Line::default());
        } else {
            let inside: Vec<_> = app.staged_inside(&path).collect();
            if !inside.is_empty() {
                let size: u64 = inside.iter().map(|i| i.alloc).sum();
                lines.push(
                    Line::from(format!(
                        "• {} inside ({})",
                        count(inside.len(), "staged item", "staged items"),
                        human_size(size)
                    ))
                    .fg(CONTAINS),
                );
                lines.push(Line::default());
            }
        }
    }
    if let Some(d) = app.details(id) {
        let (largest, exts) = (d.largest.clone(), d.exts.clone());
        let tree = app.tree.as_ref().unwrap();
        if !largest.is_empty() {
            lines.push(Line::from("Largest files inside").bold());
            let base = tree.path(id);
            for f in largest {
                let p = tree.path(f);
                let rel = p.strip_prefix(&base).unwrap_or(&p);
                lines.push(Line::from(format!(
                    "{:>9}  {:>5}  {}",
                    human_size(tree.alloc(f)),
                    age(tree.mtime(f), now),
                    rel.display()
                )));
            }
            lines.push(Line::default());
        }
        if !exts.is_empty() {
            lines.push(Line::from("By file type").bold());
            let total = tree.alloc(id).max(1);
            for e in exts {
                let ext = if e.ext.is_empty() {
                    "(none)".to_string()
                } else {
                    format!(".{}", e.ext)
                };
                lines.push(Line::from(format!(
                    "{:>9}  {:>5.1}%  {ext} ({} files)",
                    human_size(e.alloc),
                    e.alloc as f64 * 100.0 / total as f64,
                    e.files
                )));
            }
        }
    }
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title(" details "));
    f.render_widget(p, area);
}

fn draw_staged(f: &mut Frame, area: Rect, app: &mut App) {
    let height = area.height.saturating_sub(2) as usize;
    app.list_height = height;
    let start = scroll(&mut app.staged_offset, app.staged_selected, height);
    let home = app.home.as_deref();
    let rows: Vec<Row> = app.staged[start..(start + height).min(app.staged.len())]
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let included = !app.skip.contains(&item.path);
            let mut path = tilde(&item.path, home);
            if app.notes.contains_key(&item.path) {
                path.push_str(" !");
            }
            let row = Row::new(vec![
                Cell::from(if included { "[x]" } else { "[ ]" }),
                Cell::from(format!("{:>9}", human_size(item.alloc))),
                Cell::from(item.staged_by.clone()).style(Style::new().dim()),
                Cell::from(path),
            ])
            .style(if included {
                Style::new()
            } else {
                Style::new().dim()
            });
            if start + i == app.staged_selected {
                row.style(Style::new().reversed())
            } else {
                row
            }
        })
        .collect();
    let (n, total) = app.apply_summary();
    let block = Block::bordered()
        .title(format!(
            " staged for deletion: {} ",
            count(app.staged.len(), "item", "items")
        ))
        .title_bottom(format!(" {n} selected, {} ", human_size(total)));
    let widths = [
        Constraint::Length(3),
        Constraint::Length(9),
        Constraint::Length(11),
        Constraint::Fill(1),
    ];
    let mut table = Table::new(rows, widths).block(block);
    if app.staged.is_empty() {
        table = table.header(Row::new(vec![
            "",
            "",
            "",
            "Nothing staged. Press s on an item, or ask your agent.",
        ]));
    }
    f.render_widget(table, area);
}

fn draw_staged_details(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" details ");
    let Some(item) = app.staged.get(app.staged_selected) else {
        f.render_widget(block, area);
        return;
    };
    let home = app.home.as_deref();
    let mut lines = vec![
        Line::from(tilde(&item.path, home)).bold(),
        Line::default(),
        kv("size", format!("{} when staged", human_size(item.alloc))),
        kv("staged by", item.staged_by.clone()),
        kv(
            "staged",
            format!(
                "{} ago ({})",
                short_age(hogs_core::staging::now() - item.staged_at),
                date(item.staged_at)
            ),
        ),
        kv("newest change", date(item.newest_mtime)),
        Line::default(),
        Line::from("Reason").bold(),
        Line::from(item.reason.clone()),
    ];
    if let Some(why) = app.notes.get(&item.path) {
        lines.push(Line::default());
        lines.push(Line::from("Kept by the last apply").fg(CONTAINS).bold());
        lines.push(Line::from(why.clone()));
    }
    if let Some(tree) = &app.tree
        && tree.lookup(&item.path).is_none()
        && item.path.starts_with(tree.root_path())
    {
        lines.push(Line::default());
        lines.push(Line::from("Not in the current scan (gone, or created after it).").dim());
    }
    lines.push(Line::default());
    lines.push(Line::from("Enter: show it in the browser").dim());
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let line = if let Some(msg) = app.message() {
        Line::from(format!(" {msg}")).fg(CONTAINS)
    } else if app.filtering {
        Line::from(" type to filter · Enter keep · Esc clear").dim()
    } else {
        let (n, total) = app.apply_summary();
        let staged = format!("Tab staged ({n}, {})", human_size(total));
        let keys = match app.view {
            View::Browse => {
                format!(
                    " ↑↓ move · → open · ← back · s stage · {staged} · a apply · / filter · o sort · ? help · q quit"
                )
            }
            View::Staged => " ↑↓ move · Space include · u unstage · Enter show · a apply · Tab browse · ? help · q quit".to_string(),
        };
        Line::from(keys).dim()
    };
    f.render_widget(line, area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn draw_help(f: &mut Frame) {
    let keys = [
        ("↑ ↓ PgUp PgDn g G", "move"),
        ("→ Enter l", "open folder"),
        ("← Backspace h", "go up"),
        ("s Space", "stage / unstage the selected item"),
        ("/", "filter by name (Esc clears)"),
        ("o", "change sort: size, oldest change, file count, name"),
        ("Tab", "switch between browser and staged list"),
        ("a", "apply: move the selected staged items to the trash"),
        ("r", "rescan"),
        ("q Ctrl-C", "quit"),
    ];
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k:<20}"), Style::new().bold()),
                Span::raw(*v),
            ])
        })
        .collect();
    lines.push(Line::default());
    lines.push(
        Line::from(format!(
            "Marks: {} staged (or inside a staged folder), • contains staged items.",
            glyphs().staged
        ))
        .dim(),
    );
    lines.push(Line::from("Ages show the newest change anywhere inside a folder.").dim());
    lines.push(
        Line::from("Agents stage with `hogs stage`; their items and reasons appear live.").dim(),
    );
    let area = centered(f.area(), 76, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" help ")),
        area,
    );
}

fn draw_apply_progress(f: &mut Frame, p: &super::ApplyProgress, home: Option<&Path>) {
    let area = centered(f.area(), 72, 8);
    f.render_widget(Clear, area);
    let block = Block::bordered().title(" applying ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [heading, gauge, _, current, stats] =
        Layout::vertical([Constraint::Length(1); 5]).areas(inner);
    let n = (p.done + 1).min(p.total);
    f.render_widget(
        Line::from(format!("Moving to the trash: item {n} of {}", p.total)).bold(),
        heading,
    );
    let ratio = if p.total_bytes == 0 {
        p.done as f64 / p.total.max(1) as f64
    } else {
        p.done_bytes as f64 / p.total_bytes as f64
    };
    f.render_widget(
        Gauge::default()
            .ratio(ratio.clamp(0.0, 1.0))
            .label(format!(
                "{} of {}",
                human_size(p.done_bytes),
                human_size(p.total_bytes)
            ))
            .gauge_style(Style::new().fg(BAR))
            .use_unicode(glyphs().eighths),
        gauge,
    );
    if let Some((path, size, doing)) = &p.current {
        let tail = format!(" ({})", human_size(*size));
        let room = (current.width as usize).saturating_sub(doing.len() + 1 + tail.len());
        let mut shown = tilde(path, home);
        let len = shown.chars().count();
        if len > room {
            shown = format!(
                "…{}",
                shown
                    .chars()
                    .skip(len + 1 - room.max(1))
                    .collect::<String>()
            );
        }
        f.render_widget(Line::from(format!("{doing} {shown}{tail}")), current);
    }
    let secs = p.started.map_or(0.0, |s| s.elapsed().as_secs_f64());
    f.render_widget(
        Line::from(format!(
            "{} deleted · {} kept · {secs:.0}s",
            p.deleted, p.kept
        ))
        .dim(),
        stats,
    );
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let (n, total) = app.pending_summary();
    let mut lines = vec![
        Line::from(format!(
            "Move {} ({}) to the trash?",
            count(n, "item", "items"),
            human_size(total)
        ))
        .bold(),
        Line::default(),
        Line::from("Each item is checked again first. Items changed since they were"),
        Line::from("staged, or containing other filesystems, are kept."),
        Line::default(),
    ];
    if hogs_core::user::invoker().is_some() {
        lines.push(
            Line::from("Running as root via sudo: items go to root's trash, not yours,")
                .fg(CONTAINS),
        );
        lines.push(
            Line::from("and the space is freed only once that trash is emptied.").fg(CONTAINS),
        );
        lines.push(Line::default());
    }
    lines.extend([Line::from(vec![
        Span::styled(" y ", Style::new().reversed()),
        Span::raw(" move to trash    "),
        Span::styled(" n ", Style::new().reversed()),
        Span::raw(" cancel"),
    ])]);
    let area = centered(f.area(), 70, lines.len() as u16 + 2);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" apply ")
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_793_261_106), "2026-10-29");
        assert_eq!(date(-86_400), "1969-12-31");
    }

    #[test]
    fn bars() {
        assert_eq!(bar(0.0, 4, true), "    ");
        assert_eq!(bar(1.0, 4, true), "████");
        assert_eq!(bar(0.5, 4, true), "██  ");
        assert_eq!(bar(0.125, 4, true), "▌   ");
        assert_eq!(bar(0.03, 4, true), "▏   ");
        // Without eighth blocks: halves only.
        assert_eq!(bar(1.0, 4, false), "████");
        assert_eq!(bar(0.625, 4, false), "██▌ ");
        assert_eq!(bar(0.03, 4, false), "    ");
        assert!(
            bar(0.99, 4, false)
                .chars()
                .all(|c| matches!(c, '█' | '▌' | ' '))
        );
    }

    #[test]
    fn glyphs_per_terminal() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(
            pick_glyphs(false, env(&[])).eighths,
            "Linux and macOS terminals"
        );
        assert!(
            !pick_glyphs(true, env(&[])).eighths,
            "classic Windows console"
        );
        assert!(
            pick_glyphs(true, env(&[("WT_SESSION", "x")])).eighths,
            "Windows Terminal"
        );
        assert!(pick_glyphs(true, env(&[("TERM_PROGRAM", "vscode")])).eighths);
        assert!(!pick_glyphs(false, env(&[("HOGS_GLYPHS", "basic")])).eighths);
        assert!(pick_glyphs(true, env(&[("HOGS_GLYPHS", "unicode")])).eighths);
        assert_eq!(pick_glyphs(true, env(&[])).staged, "×");
    }

    #[test]
    fn tildes() {
        let home = Some(Path::new("/home/u"));
        assert_eq!(tilde(Path::new("/home/u"), home), "~");
        assert_eq!(tilde(Path::new("/home/u/a/b"), home), "~/a/b");
        assert_eq!(tilde(Path::new("/etc"), home), "/etc");
    }
}
