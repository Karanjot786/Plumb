//! Path safety checks. Every one of these answers "may this path be touched at
//! all", and every one is allowed to say no for a reason it cannot fully
//! verify. Refusing costs the user a click; guessing costs them a file.
//!
//! Nothing here removes, moves, or opens a file for writing.

use serde::Deserialize;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

const RULES: &str = include_str!("../blocklist.toml");

#[derive(Deserialize)]
struct Rules {
    deny: Vec<Deny>,
}

#[derive(Deserialize)]
struct Deny {
    path: String,
    reason: String,
    /// Deny the directory itself but not its contents. Mount roots need this:
    /// `/Volumes` must be undeletable without making every external drive
    /// untouchable. Volume roots stay protected by `is_mount_point`.
    #[serde(default)]
    exact: bool,
}

/// Filesystem identity, captured without following symlinks. `dev`/`ino` is
/// what makes the deny list and the TOCTOU check work on case-insensitive
/// filesystems, where two different strings name one file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ident {
    pub dev: u64,
    pub ino: u64,
    pub mtime: u32,
    pub bytes: u64,
}

struct Entry {
    id: Ident,
    reason: String,
    exact: bool,
}

pub struct Blocklist {
    /// Resolved entries. Rules absent from this machine are dropped.
    entries: Vec<Entry>,
}

impl Blocklist {
    /// The reason this path is denied, if it or any ancestor is on the list.
    /// `exact` entries only match the path itself.
    fn reason_for(&self, path: &Path) -> Option<&str> {
        let mut cur = Some(path);
        let mut self_level = true;
        while let Some(p) = cur {
            if let Ok(id) = identity(p) {
                for e in &self.entries {
                    if (self_level || !e.exact) && e.id.dev == id.dev && e.id.ino == id.ino {
                        return Some(&e.reason);
                    }
                }
            }
            self_level = false;
            cur = p.parent();
        }
        None
    }
}

pub fn blocklist() -> &'static Blocklist {
    static BL: OnceLock<Blocklist> = OnceLock::new();
    BL.get_or_init(|| {
        let parsed: Rules = toml::from_str(RULES).expect("embedded blocklist.toml is malformed");
        let mut entries = Vec::new();
        for d in parsed.deny {
            // Follow here on purpose: we want the identity of the real
            // directory, so a symlinked alias to /System is caught too.
            if let Ok(m) = std::fs::metadata(&d.path) {
                entries.push(Entry { id: ident_of(&m), reason: d.reason, exact: d.exact });
            }
        }
        Blocklist { entries }
    })
}

/// `Some(reason)` when the path is on the deny list, compared by inode.
pub fn denied(path: &Path) -> Option<&'static str> {
    // The Blocklist lives in a OnceLock, so its strings outlive any caller.
    let bl: &'static Blocklist = blocklist();
    bl.reason_for(path)
}

/// Structural rules that hold before any filesystem access.
///
/// This is the empty-variable-collapse guard: a blank component must never let
/// `/Users/{user}/{leaf}` become `/Users`.
pub fn shape_problem(path: &Path) -> Option<&'static str> {
    if path.as_os_str().is_empty() {
        return Some("empty path");
    }
    if !path.is_absolute() {
        return Some("not an absolute path");
    }
    let mut named = 0usize;
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                if s.is_empty() {
                    return Some("path has an empty component");
                }
                named += 1;
            }
            Component::CurDir | Component::ParentDir => {
                return Some("path has a . or .. component");
            }
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    // "/", "/Users" and "C:\Users" are never candidates.
    if named < 2 {
        return Some("path is too close to the volume root");
    }
    if let Some(h) = home() {
        if path == h {
            return Some("path is the home directory");
        }
        if h.starts_with(path) {
            return Some("path contains the home directory");
        }
        // Under $HOME, demand at least one level below it, so a collapsed
        // variable cannot address the home directory's children wholesale.
        if path.starts_with(&h) && path.components().count() <= h.components().count() {
            return Some("path is too close to the home directory");
        }
    }
    None
}

/// Absolute, symlink-free path — except for a final component that is itself a
/// symlink, which is preserved.
///
/// Plain `canonicalize` resolves that last link too, which would silently
/// retarget every operation from the link onto whatever it points at.
pub fn canon_keep_link(p: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false);
    if !is_link {
        return p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) => match dir.canonicalize() {
            Ok(d) => d.join(name),
            Err(_) => p.to_path_buf(),
        },
        _ => p.to_path_buf(),
    }
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// A directory whose device differs from its parent's is a mount point.
pub fn is_mount_point(path: &Path) -> bool {
    let Ok(here) = identity(path) else { return false };
    let Some(parent) = path.parent() else { return true };
    match identity(parent) {
        Ok(up) => up.dev != here.dev,
        Err(_) => false,
    }
}

/// True when `path` is a symlink whose target resolves outside `root`. The link
/// itself is what gets staged; the target is never followed.
pub fn symlink_escapes(path: &Path, root: &Path) -> bool {
    let Ok(m) = std::fs::symlink_metadata(path) else { return false };
    if !m.file_type().is_symlink() {
        return false;
    }
    let Ok(target) = std::fs::read_link(path) else { return true };
    let joined = if target.is_absolute() {
        target
    } else {
        match path.parent() {
            Some(p) => p.join(target),
            None => return true,
        }
    };
    let (Ok(real), Ok(real_root)) = (joined.canonicalize(), root.canonicalize()) else {
        // A dangling link points nowhere, so it cannot escape anywhere.
        return false;
    };
    !real.starts_with(real_root)
}

/// `Some(reason)` when a package manager claims this file. Only consulted for
/// paths under a system prefix, so the common case spawns nothing.
pub fn package_owned(path: &Path) -> Option<&'static str> {
    const PREFIXES: [&str; 6] = ["/usr", "/opt", "/bin", "/sbin", "/lib", "/etc"];
    let s = path.to_string_lossy();
    if !PREFIXES.iter().any(|p| s.starts_with(p)) {
        return None;
    }
    for (bin, flag) in [("dpkg", "-S"), ("rpm", "-qf"), ("pacman", "-Qo")] {
        if let Ok(o) = std::process::Command::new(bin).arg(flag).arg(path).output() {
            if o.status.success() {
                return Some("owned by the system package manager");
            }
        }
    }
    None
}

fn ident_of(m: &std::fs::Metadata) -> Ident {
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32);
    Ident { dev: dev_of(m), ino: ino_of(m), mtime, bytes: len_of(m) }
}

#[cfg(unix)]
fn dev_of(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.dev()
}

#[cfg(unix)]
fn ino_of(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.ino()
}

#[cfg(unix)]
fn len_of(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.blocks() * 512
}

#[cfg(windows)]
fn dev_of(_m: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(windows)]
fn ino_of(_m: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(windows)]
fn len_of(m: &std::fs::Metadata) -> u64 {
    m.len()
}

/// Identity of a path, never following a final symlink.
#[cfg(unix)]
pub fn identity(path: &Path) -> io::Result<Ident> {
    let m = std::fs::symlink_metadata(path)?;
    Ok(ident_of(&m))
}

/// Windows has no `ino`, but `(volume serial, file index)` serves the same
/// purpose. `FILE_FLAG_OPEN_REPARSE_POINT` keeps a symlink from being followed,
/// matching the Unix `symlink_metadata` above.
#[cfg(windows)]
pub fn identity(path: &Path) -> io::Result<Ident> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let mut w: Vec<u16> = path.as_os_str().encode_wide().collect();
    w.push(0);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let ft = ((info.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | info.ftLastWriteTime.dwLowDateTime as u64;
    // FILETIME counts 100 ns ticks since 1601-01-01.
    let mtime = ft.saturating_sub(116_444_736_000_000_000) / 10_000_000;
    Ok(Ident {
        dev: info.dwVolumeSerialNumber as u64,
        ino: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        mtime: mtime.min(u32::MAX as u64) as u32,
        bytes: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
    })
}
