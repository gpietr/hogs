use std::fs;
use std::path::Path;

use hogs_core::{Progress, Tree};
use hogs_scan::{Backend, ScanOptions, scan};

fn backends() -> [Backend; 2] {
    [Backend::Auto, Backend::Generic]
}

fn scan_with(root: &Path, backend: Backend) -> Tree {
    let opts = ScanOptions {
        backend,
        ..Default::default()
    };
    scan(root, &opts, &Progress::default()).unwrap()
}

fn node(tree: &Tree, rel: &str) -> u32 {
    tree.lookup(&tree.root_path().join(rel))
        .unwrap_or_else(|| panic!("{rel} not in tree"))
}

/// A small tree with known file sizes.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    fs::create_dir_all(r.join("a/b/c")).unwrap();
    fs::create_dir_all(r.join("empty")).unwrap();
    fs::write(r.join("top.bin"), vec![0u8; 10_000]).unwrap();
    fs::write(r.join("a/one.txt"), b"hello").unwrap();
    fs::write(r.join("a/b/c/deep.dat"), vec![1u8; 300_000]).unwrap();
    dir
}

#[test]
fn sizes_and_structure() {
    let dir = fixture();
    for backend in backends() {
        let t = scan_with(dir.path(), backend);
        // Apparent size counts file contents only, like `du --apparent-size`.
        assert_eq!(t.size(Tree::ROOT), 10_000 + 5 + 300_000, "{backend:?}");
        assert_eq!(t.files(Tree::ROOT), 3);
        assert_eq!(t.len(), 8);
        // Largest child first.
        assert_eq!(t.name(t.children_by_size(Tree::ROOT)[0]), b"a");
        assert!(t.alloc(node(&t, "a")) >= 300_000);
        assert_eq!(t.files(node(&t, "a/b")), 1);
        assert!(t.is_dir(node(&t, "empty")));
        assert_eq!(t.children(node(&t, "empty")).len(), 0);
    }
}

#[cfg(unix)]
#[test]
fn hardlinks_counted_once_and_symlinks_not_followed() {
    let dir = fixture();
    let r = dir.path();
    fs::hard_link(r.join("a/b/c/deep.dat"), r.join("link.dat")).unwrap();
    std::os::unix::fs::symlink(r.join("a"), r.join("sym")).unwrap();
    for backend in backends() {
        let t = scan_with(r, backend);
        assert_eq!(t.files(Tree::ROOT), 5, "{backend:?}");
        let a = node(&t, "a/b/c/deep.dat");
        let b = node(&t, "link.dat");
        // Exactly one of the two links carries the size.
        assert_eq!(t.size(a) + t.size(b), 300_000);
        assert_eq!(
            (t.flags(a) | t.flags(b)) & hogs_core::flags::HARDLINK_DUP,
            hogs_core::flags::HARDLINK_DUP
        );
        let sym = node(&t, "sym");
        assert!(t.flags(sym) & hogs_core::flags::SYMLINK != 0);
        assert!(!t.is_dir(sym));
        assert_eq!(t.children(sym).len(), 0);
    }
}

#[cfg(unix)]
#[test]
fn unreadable_dir_is_flagged() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("secret"), b"x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&locked).is_ok() {
        return; // running as root; permissions are not enforced
    }
    for backend in backends() {
        let t = scan_with(dir.path(), backend);
        let id = node(&t, "locked");
        assert!(t.flags(id) & hogs_core::flags::ERROR != 0, "{backend:?}");
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn non_utf8_names_round_trip() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let name = OsStr::from_bytes(b"bad\xffname");
    // Some filesystems (APFS) refuse names that are not valid UTF-8.
    let bad_allowed = fs::write(dir.path().join(name), b"data").is_ok();
    fs::write(dir.path().join("zażółć"), b"data").unwrap();
    for backend in backends() {
        let t = scan_with(dir.path(), backend);
        let id = node(&t, "zażółć");
        assert_eq!(t.size(id), 4);
        if !bad_allowed {
            continue;
        }
        let bad = t
            .lookup(&dir.path().canonicalize().unwrap().join(name))
            .unwrap();
        assert_eq!(
            t.path(bad),
            dir.path().canonicalize().unwrap().join(name),
            "{backend:?}"
        );
    }
}
