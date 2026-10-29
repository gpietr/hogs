//! Plain-text formatting for the CLI. Plain aligned
//! text is both readable for humans and compact for agents.

use std::fmt::Write;

use hogs_core::fmt::human_size;
use hogs_core::{NodeId, Tree, flags};

/// Coarse age of a Unix timestamp relative to `now`, e.g. `3d`, `5mo`, `2y`.
pub fn age(mtime: i64, now: i64) -> String {
    let days = (now - mtime).max(0) / 86_400;
    match days {
        0 => "today".into(),
        1..=59 => format!("{days}d"),
        60..=729 => format!("{}mo", days / 30),
        _ => format!("{}y", days / 365),
    }
}

/// `1 item`, `2 items`.
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Parses sizes like `500`, `100k`, `1.5G`, `2GiB` (binary units).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size {s:?}"))?;
    let shift = match unit
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches("ib")
        .trim_end_matches('b')
    {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        _ => return Err(format!("invalid size unit in {s:?}")),
    };
    if num < 0.0 {
        return Err(format!("invalid size {s:?}: must not be negative"));
    }
    Ok((num * (1u64 << shift) as f64) as u64)
}

/// Parses durations like `30s`, `10m`, `2h`, `90d`, `6mo`, `1y` into seconds.
pub fn parse_duration(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration {s:?}"))?;
    let unit_secs = match unit.trim() {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        "mo" => 30 * 86_400,
        "y" => 365 * 86_400,
        _ => {
            return Err(format!(
                "invalid duration {s:?}; use a unit, e.g. 90d, 10m, 1y"
            ));
        }
    };
    if num < 0.0 {
        return Err(format!("invalid duration {s:?}: must not be negative"));
    }
    Ok((num * unit_secs as f64) as i64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// Aligned human-readable rows.
    Table,
    /// Bare paths, one per line or NUL-terminated, for piping.
    Paths { nul: bool },
}

/// Formats node rows: size, share of `total`, file count, age and full path.
pub struct Rows<'a> {
    pub tree: &'a Tree,
    pub now: i64,
    pub output: Output,
}

impl Rows<'_> {
    pub fn is_table(&self) -> bool {
        self.output == Output::Table
    }

    pub fn row(&self, out: &mut String, id: NodeId, total: u64, indent: usize) {
        let t = self.tree;
        if let Output::Paths { nul } = self.output {
            let _ = write!(out, "{}", t.path(id).display());
            out.push(if nul { '\0' } else { '\n' });
            return;
        }
        let pct = t.alloc(id) as f64 * 100.0 / total.max(1) as f64;
        let _ = write!(
            out,
            "{:indent$}{:>10} {:>5.1}%",
            "",
            human_size(t.alloc(id)),
            pct
        );
        if t.is_dir(id) {
            let _ = write!(out, " {:>8} files", t.files(id));
        } else {
            let _ = write!(out, " {:>14}", "");
        }
        let _ = write!(
            out,
            " {:>6}  {}",
            age(t.mtime(id), self.now),
            t.path(id).display()
        );
        if t.is_dir(id) {
            out.push('/');
        }
        let f = t.flags(id);
        if f & flags::ERROR != 0 {
            out.push_str("  [partially unreadable]");
        }
        if f & flags::OTHER_FS != 0 {
            out.push_str("  [other filesystem, not scanned]");
        }
        if f & flags::SYMLINK != 0 {
            out.push_str("  [symlink]");
        }
        if f & flags::HARDLINK_DUP != 0 {
            out.push_str("  [hardlink, counted elsewhere]");
        }
        out.push('\n');
    }

    /// Children of `id` (largest first) down to `depth` levels, `limit` per directory.
    pub fn listing(&self, out: &mut String, id: NodeId, depth: usize, limit: usize) {
        self.listing_at(out, id, depth, limit, 0);
    }

    fn listing_at(&self, out: &mut String, id: NodeId, depth: usize, limit: usize, indent: usize) {
        let t = self.tree;
        let kids = t.children_by_size(id);
        for &c in kids.iter().take(limit) {
            self.row(out, c, t.alloc(id), indent);
            if depth > 1 && t.is_dir(c) {
                self.listing_at(out, c, depth - 1, limit, indent + 2);
            }
        }
        if kids.len() > limit && self.is_table() {
            let rest: u64 = kids[limit..].iter().map(|&c| t.alloc(c)).sum();
            let _ = writeln!(
                out,
                "{:indent$}… {} more entries, {} total",
                "",
                kids.len() - limit,
                human_size(rest)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("500"), Ok(500));
        assert_eq!(parse_size("100k"), Ok(100 << 10));
        assert_eq!(parse_size("1.5G"), Ok(3 << 29));
        assert_eq!(parse_size("2GiB"), Ok(2 << 30));
        assert_eq!(parse_size("10 MB"), Ok(10 << 20));
        assert!(parse_size("5x").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90d"), Ok(90 * 86_400));
        assert_eq!(parse_duration("10m"), Ok(600));
        assert_eq!(parse_duration("1.5h"), Ok(5_400));
        assert_eq!(parse_duration("6mo"), Ok(180 * 86_400));
        assert!(parse_duration("90").is_err());
        assert!(parse_duration("-5d").is_err());
        assert!(parse_size("-1G").is_err());
    }

    #[test]
    fn ages() {
        assert_eq!(age(1000, 1000), "today");
        assert_eq!(age(0, 86_400 * 3), "3d");
        assert_eq!(age(0, 86_400 * 400), "13mo");
        assert_eq!(age(0, 86_400 * 1000), "2y");
    }
}
