//! Who hogs works for. Under `sudo` (or `pkexec`) the process runs as root
//! but acts for the invoking user: it uses their staged list and saved scans,
//! protects their home directory, and leaves files it creates owned by them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The user who ran hogs through sudo or pkexec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invoker {
    pub home: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

/// The invoking user when running as root via sudo/pkexec, otherwise `None`.
pub fn invoker() -> Option<&'static Invoker> {
    static INVOKER: OnceLock<Option<Invoker>> = OnceLock::new();
    INVOKER.get_or_init(lookup).as_ref()
}

/// Home directory of the person hogs works for.
pub fn home() -> Option<PathBuf> {
    match invoker() {
        Some(i) => Some(i.home.clone()),
        None => std::env::home_dir(),
    }
}

/// Home directories that must never be deleted: the invoking user's and the
/// current process user's (root's, under sudo).
pub fn protected_homes() -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = invoker().map(|i| i.home.clone()).into_iter().collect();
    homes.extend(std::env::home_dir().filter(|h| !homes.contains(h)));
    homes
}

/// Gives a file or directory created as root back to the invoking user. Does
/// nothing when not running via sudo.
pub fn give_back(path: &Path) {
    #[cfg(unix)]
    if let Some(i) = invoker() {
        let _ = std::os::unix::fs::lchown(path, Some(i.uid), Some(i.gid));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// `create_dir_all` that gives every directory it creates to the invoking user.
pub fn create_dir_all(dir: &Path) -> io::Result<()> {
    let missing: Vec<&Path> = dir.ancestors().take_while(|p| !p.exists()).collect();
    fs::create_dir_all(dir)?;
    for p in missing {
        give_back(p);
    }
    Ok(())
}

#[cfg(unix)]
fn lookup() -> Option<Invoker> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let uid: u32 = ["SUDO_UID", "PKEXEC_UID"]
        .iter()
        .find_map(|v| std::env::var(v).ok()?.parse().ok())?;
    if uid == 0 {
        return None;
    }
    let (home, gid) = passwd_entry(uid)?;
    Some(Invoker { home, uid, gid })
}

#[cfg(not(unix))]
fn lookup() -> Option<Invoker> {
    None
}

/// Home directory and primary group of `uid` from the user database (NSS).
#[cfg(unix)]
fn passwd_entry(uid: u32) -> Option<(PathBuf, u32)> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    let mut buf = vec![0u8; 16 * 1024];
    loop {
        // SAFETY: an all-zero passwd is a valid out-parameter for getpwuid_r.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and buf.len() is its true size.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success pw_dir points to a NUL-terminated string inside buf.
        let dir = unsafe { CStr::from_ptr(pwd.pw_dir) };
        let home = PathBuf::from(OsStr::from_bytes(dir.to_bytes()));
        return home.is_absolute().then_some((home, pwd.pw_gid));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn looks_up_own_home() {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let (home, _) = passwd_entry(uid).expect("current user is in the user database");
        assert!(home.is_absolute());
        assert!(passwd_entry(u32::MAX - 1).is_none());
    }

    #[test]
    fn homes_include_current_user() {
        if invoker().is_none() {
            assert_eq!(home(), std::env::home_dir());
            assert_eq!(
                protected_homes(),
                std::env::home_dir().into_iter().collect::<Vec<_>>()
            );
        }
    }
}
