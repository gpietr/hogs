//! Paths that are never deleted, whatever gets staged. This is a safety
//! rail, not advice: deciding what is worth deleting is up to the user and
//! their agent.

use std::path::{Component, Path, PathBuf};

/// System locations, protected with everything inside them. Compared
/// case-insensitively; `*:` matches any Windows drive.
const SYSTEM: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/proc",
    "/sbin",
    "/sys",
    "/usr",
    "/var/lib",
    // macOS, where /etc and /var resolve into /private. Not all of /private/var:
    // per-user temp and cache folders live in /private/var/folders.
    "/private/etc",
    "/private/var/db",
    "/private/var/root",
    "/private/var/vm",
    "/System",
    "/Library",
    "*:/windows",
    "*:/program files",
    "*:/program files (x86)",
    "*:/programdata",
];

/// Why deleting `path` must be refused, if it must. `homes` are the home
/// directories to protect (see [`crate::user::protected_homes`]).
pub fn is_protected(path: &Path, homes: &[PathBuf]) -> Option<String> {
    // Everything below compares paths lexically: `..` could point anywhere, and
    // `.` would hide a match, so rebuild the path from its components first.
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Some("is not a normalized absolute path".into());
    }
    let clean: PathBuf = path
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    let path = clean.as_path();
    if path.parent().is_none() {
        return Some("is a filesystem root".into());
    }
    let p = comparable(path);
    if homes.iter().any(|h| {
        let h = comparable(h);
        h == p || h.starts_with(&format!("{p}/"))
    }) {
        return Some("is a home directory or one of its parents".into());
    }
    if let Some(dir) = path
        .ancestors()
        .find_map(|p| SYSTEM.iter().find(|s| matches(s, p)))
    {
        return Some(format!("is inside the system directory {dir}"));
    }
    // Deleting a parent of a system directory deletes that too. (Drive-wildcard
    // entries only have drive roots as parents, which are refused above.)
    let contains = |s: &&&str| {
        !s.starts_with("*:")
            && Path::new(s)
                .ancestors()
                .skip(1)
                .any(|a| matches(&a.to_string_lossy(), path))
    };
    SYSTEM
        .iter()
        .find(contains)
        .map(|dir| format!("contains the system directory {dir}"))
}

/// A path as a string that compares equal for every spelling of the same
/// location: Windows verbatim prefixes (`\\?\C:\`, `\\?\UNC\`) removed, `/`
/// separators, no trailing separator, and case folded. Folding is only needed on
/// Windows, but elsewhere it can only make the guard stricter.
fn comparable(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = match s.strip_prefix("//?/UNC/") {
        Some(rest) => format!("//{rest}"),
        None => s
            .strip_prefix("//?/")
            .or_else(|| s.strip_prefix("//./"))
            .unwrap_or(&s)
            .to_string(),
    };
    s.trim_end_matches('/').to_lowercase()
}

fn matches(pat: &str, path: &Path) -> bool {
    let p = comparable(path);
    match pat.strip_prefix("*:") {
        Some(rest) => {
            let b = p.as_bytes();
            b.len() >= 2
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && p[2..] == rest.to_lowercase()
        }
        None => p == pat.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_paths() {
        let protected =
            |p: &str| is_protected(Path::new(p), &["/home/u".into(), "/root".into()]).is_some();
        assert!(protected("/"));
        assert!(protected("/home"));
        assert!(protected("/home/u"));
        assert!(protected("/root"));
        assert!(!protected("/root/.cache"));
        assert!(protected("/usr"));
        assert!(protected("/usr/share/doc"));
        assert!(protected("/var/lib/docker/overlay2"));
        assert!(!protected("/var/log"));
        assert!(protected("/var"));
        assert!(protected("/private"));
        assert!(protected("/private/etc/hosts"));
        assert!(protected("/private/var/db/receipts"));
        assert!(protected("/private/var"));
        assert!(!protected("/private/var/folders/ab/xyz/T/old-build"));
        assert!(protected("/home/u/x/../../../usr"));
        assert!(protected("/usr/./lib"));
        assert!(!protected("/home/u/./x"));
        assert!(protected("relative/path"));
        assert!(!protected("/home/u/.cache"));
        assert!(!protected("/home/u/code/app/node_modules"));
        assert!(!protected("/usrx"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths() {
        let protected = |p: &str| is_protected(Path::new(p), &[r"C:\Users\Me".into()]).is_some();
        assert!(protected(r"C:\"));
        assert!(protected(r"D:\"));
        assert!(protected(r"C:\Windows"));
        assert!(protected(r"c:\windows\System32\drivers"));
        assert!(protected(r"\\?\C:\Windows\WinSxS"));
        assert!(protected(r"C:\Program Files\App"));
        assert!(protected(r"C:\ProgramData"));
        assert!(protected(r"C:\Users"));
        assert!(protected(r"C:\Users\Me"));
        assert!(protected(r"\\?\C:\Users\me"));
        assert!(protected(r"C:\Users\Me\proj\..\..\Other"));
        assert!(!protected(r"C:\Users\Me\proj\node_modules"));
        assert!(!protected(r"\\?\C:\Users\Me\AppData\Local\Temp\x"));
        assert!(!protected(r"D:\games\old"));
    }

    #[test]
    fn path_spellings() {
        assert!(matches("*:/windows", Path::new("D:\\Windows\\")));
        // std::fs::canonicalize returns verbatim paths on Windows.
        assert!(matches("*:/windows", Path::new(r"\\?\C:\Windows")));
        assert!(matches(
            "*:/program files",
            Path::new(r"\\?\c:\PROGRAM FILES\")
        ));
        assert!(!matches("*:/windows", Path::new("D:\\Windowsx")));
        assert_eq!(comparable(Path::new(r"\\?\C:\Users\Me")), "c:/users/me");
        assert_eq!(
            comparable(Path::new(r"\\?\UNC\srv\share\x")),
            "//srv/share/x"
        );
        assert_eq!(comparable(Path::new(r"C:\Users\me\")), "c:/users/me");
    }
}
