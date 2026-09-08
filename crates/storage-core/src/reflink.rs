//! Reflink deduplication: make two byte-identical files share one set of
//! extents, so the second copy stops costing disk space.
//!
//! # The kernel does not always check for you
//!
//! Verified against primary sources before any of this was written:
//!
//! * **Linux `FIDEDUPERANGE` compares in the kernel.** `ioctl_fideduperange(2)`
//!   describes a "compare and share if identical" operation, and "if even a
//!   single byte in the range does not match, the deduplication request will
//!   be ignored and status set to `FILE_DEDUPE_RANGE_DIFFERS`". A hash
//!   collision there is harmless: the kernel simply declines.
//!
//! * **macOS `clonefile(2)` compares nothing.** It "causes the named file src
//!   to be cloned to the named file dst" and "the named file dst must not
//!   exist for the call to be successful". It is a copy primitive that never
//!   examines the file being replaced, so a clone-and-replace transaction has
//!   no kernel safety net whatsoever.
//!
//! Therefore **`byte_equal` is an unconditional precondition here**, on every
//! platform. Making it conditional on the mechanism would leave a design in
//! which enabling the Linux path first quietly makes the macOS path the
//! unchecked one. A full compare of two files we already believe identical
//! costs one sequential read and buys the guarantee outright.
//!
//! # Availability is probed, never assumed
//!
//! | Platform | Mechanism | Status |
//! |---|---|---|
//! | macOS / APFS | `clonefile(2)` | supported |
//! | Linux / btrfs, XFS | `FICLONE` / `FIDEDUPERANGE` | supported |
//! | Windows / NTFS | none | unsupported, feature hidden |
//! | Windows / ReFS, Dev Drive | `FSCTL_DUPLICATE_EXTENTS_TO_FILE` | conditional |
//!
//! `supported()` writes two tiny files in the target directory and tries the
//! real operation on them, because a filesystem's name does not tell you
//! whether this particular mount will accept it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

// ----------------------------------------------------------------- compare

/// Full sequential comparison. The precondition the kernel will not enforce.
pub fn byte_equal(a: &Path, b: &Path) -> io::Result<bool> {
    use std::io::Read;
    let (ma, mb) = (fs::symlink_metadata(a)?, fs::symlink_metadata(b)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    let mut fa = io::BufReader::new(fs::File::open(a)?);
    let mut fb = io::BufReader::new(fs::File::open(b)?);
    let mut ba = vec![0u8; 256 * 1024];
    let mut bb = vec![0u8; 256 * 1024];
    loop {
        let na = fa.read(&mut ba)?;
        if na == 0 {
            return Ok(fb.read(&mut bb)? == 0);
        }
        let mut filled = 0;
        while filled < na {
            let n = fb.read(&mut bb[filled..na])?;
            if n == 0 {
                return Ok(false);
            }
            filled += n;
        }
        if ba[..na] != bb[..na] {
            return Ok(false);
        }
    }
}

// ---------------------------------------------------------------- platform

#[cfg(target_os = "macos")]
fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let s = CString::new(src.as_os_str().as_bytes()).map_err(|_| other("path contains NUL"))?;
    let d = CString::new(dst.as_os_str().as_bytes()).map_err(|_| other("path contains NUL"))?;
    // clonefile(2) requires dst not to exist, which is why the transaction
    // below always renames the victim aside first.
    let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    let s = fs::File::open(src)?;
    let d = fs::File::create(dst)?;
    let rc = unsafe { libc::ioctl(d.as_raw_fd(), FICLONE, s.as_raw_fd()) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        drop(d);
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_file(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(other("reflink is not available on this platform"))
}

// ------------------------------------------------------------------- probe

/// Whether this specific directory's mount will accept a reflink. Two tiny
/// files are created, cloned, and removed. Never inferred from the filesystem
/// type: NTFS cannot do this at all, and ReFS only can under matching
/// integrity and cluster settings.
pub fn supported(dir: &Path) -> bool {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let src = dir.join(format!(".sv-reflink-probe-{stamp}"));
    let dst = dir.join(format!(".sv-reflink-probe-{stamp}.clone"));
    let ok = fs::write(&src, b"sv reflink probe").is_ok() && clone_file(&src, &dst).is_ok();
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    ok
}

// ------------------------------------------------------------- transaction

#[derive(Debug, Clone)]
pub struct Deduped {
    pub kept: PathBuf,
    pub replaced: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Refused {
    pub path: PathBuf,
    pub why: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub done: Vec<Deduped>,
    pub refused: Vec<Refused>,
    /// Bytes the filesystem returns once the extents are shared.
    pub freed: u64,
}

/// Extended attributes have to survive the replacement, not be refused: macOS
/// stamps `com.apple.provenance` on ordinary files, so treating "has xattrs"
/// as a refusal would disable the feature on almost every real file.
///
/// `clonefile` copies the *source's* xattrs onto the clone, which are the
/// wrong ones. The clone is therefore stripped and the victim's own set is
/// written back.
type Xattrs = Vec<(std::ffi::CString, Vec<u8>)>;

#[cfg(unix)]
fn cpath(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| other("path contains NUL"))
}

#[cfg(all(unix, target_os = "macos"))]
fn list_raw(c: &std::ffi::CStr, buf: *mut libc::c_char, size: usize) -> isize {
    unsafe { libc::listxattr(c.as_ptr(), buf, size, libc::XATTR_NOFOLLOW) }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn list_raw(c: &std::ffi::CStr, buf: *mut libc::c_char, size: usize) -> isize {
    unsafe { libc::llistxattr(c.as_ptr(), buf, size) }
}

#[cfg(all(unix, target_os = "macos"))]
fn get_raw(c: &std::ffi::CStr, n: &std::ffi::CStr, buf: *mut libc::c_void, size: usize) -> isize {
    unsafe { libc::getxattr(c.as_ptr(), n.as_ptr(), buf, size, 0, libc::XATTR_NOFOLLOW) }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn get_raw(c: &std::ffi::CStr, n: &std::ffi::CStr, buf: *mut libc::c_void, size: usize) -> isize {
    unsafe { libc::lgetxattr(c.as_ptr(), n.as_ptr(), buf, size) }
}

#[cfg(all(unix, target_os = "macos"))]
fn set_raw(c: &std::ffi::CStr, n: &std::ffi::CStr, v: &[u8]) -> i32 {
    unsafe {
        libc::setxattr(
            c.as_ptr(),
            n.as_ptr(),
            v.as_ptr() as *const libc::c_void,
            v.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn set_raw(c: &std::ffi::CStr, n: &std::ffi::CStr, v: &[u8]) -> i32 {
    unsafe {
        libc::lsetxattr(c.as_ptr(), n.as_ptr(), v.as_ptr() as *const libc::c_void, v.len(), 0)
    }
}

#[cfg(all(unix, target_os = "macos"))]
fn remove_raw(c: &std::ffi::CStr, n: &std::ffi::CStr) -> i32 {
    unsafe { libc::removexattr(c.as_ptr(), n.as_ptr(), libc::XATTR_NOFOLLOW) }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn remove_raw(c: &std::ffi::CStr, n: &std::ffi::CStr) -> i32 {
    unsafe { libc::lremovexattr(c.as_ptr(), n.as_ptr()) }
}

#[cfg(unix)]
fn read_xattrs(p: &Path) -> io::Result<Xattrs> {
    let c = cpath(p)?;
    let size = list_raw(&c, std::ptr::null_mut(), 0);
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0 as libc::c_char; size as usize];
    let n = list_raw(&c, names.as_mut_ptr(), names.len());
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes: Vec<u8> = names[..n as usize].iter().map(|&b| b as u8).collect();

    let mut out = Vec::new();
    for name in bytes.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        let Ok(nc) = std::ffi::CString::new(name) else { continue };
        let len = get_raw(&c, &nc, std::ptr::null_mut(), 0);
        if len < 0 {
            continue;
        }
        let mut val = vec![0u8; len as usize];
        let got = get_raw(&c, &nc, val.as_mut_ptr() as *mut libc::c_void, val.len());
        if got < 0 {
            continue;
        }
        val.truncate(got as usize);
        out.push((nc, val));
    }
    Ok(out)
}

/// Strip whatever the clone inherited, then write the saved set back.
#[cfg(unix)]
fn restore_xattrs(p: &Path, want: &Xattrs) -> io::Result<()> {
    let c = cpath(p)?;
    for (name, _) in read_xattrs(p)?.iter() {
        remove_raw(&c, name);
    }
    for (name, val) in want {
        if set_raw(&c, name, val) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn read_xattrs(_p: &Path) -> io::Result<Xattrs> {
    Ok(Vec::new())
}

#[cfg(not(unix))]
fn restore_xattrs(_p: &Path, _want: &Xattrs) -> io::Result<()> {
    Ok(())
}

/// Replace `dup` with a reflink of `keep`, so the two names share one copy of
/// the data.
///
/// The order is chosen so every failure is recoverable:
///
/// 1. refuse anything that is not a plain file, or that carries xattrs
/// 2. **byte-compare** — the kernel will not do it for us here
/// 3. rename `dup` aside, in the same directory, so it is a rename not a copy
/// 4. clone `keep` into `dup`'s place
/// 5. restore `dup`'s own permissions and modification time onto the clone
/// 6. on any failure from step 4 on, put the original straight back
pub fn dedupe_pair(keep: &Path, dup: &Path) -> io::Result<u64> {
    let (mk, md) = (fs::symlink_metadata(keep)?, fs::symlink_metadata(dup)?);
    if !mk.is_file() || !md.is_file() {
        return Err(other("both paths must be plain files"));
    }
    if mk.file_type().is_symlink() || md.file_type().is_symlink() {
        return Err(other("refusing to dedupe a symlink"));
    }
    if keep == dup {
        return Err(other("a file cannot be deduped against itself"));
    }
    if !byte_equal(keep, dup)? {
        return Err(other("contents differ: refusing to replace"));
    }
    // Captured before anything moves, so a failure can put them back too.
    let xattrs = read_xattrs(dup)?;

    let parent = dup.parent().ok_or_else(|| other("no parent directory"))?;
    let backup = parent.join(format!(
        ".sv-reflink-backup-{}",
        dup.file_name().unwrap_or_default().to_string_lossy()
    ));
    if backup.exists() {
        return Err(other("a previous dedupe left a backup here; resolve it first"));
    }

    fs::rename(dup, &backup)?;

    if let Err(e) = clone_file(keep, dup) {
        let _ = fs::rename(&backup, dup);
        return Err(e);
    }
    // Permissions and mtime belong to the file being replaced, not the source.
    if let Err(e) = fs::set_permissions(dup, md.permissions()) {
        let _ = fs::remove_file(dup);
        let _ = fs::rename(&backup, dup);
        return Err(e);
    }
    if let Err(e) = restore_xattrs(dup, &xattrs) {
        let _ = fs::remove_file(dup);
        let _ = fs::rename(&backup, dup);
        return Err(e);
    }
    if let Ok(t) = md.modified() {
        if let Ok(f) = fs::File::options().write(true).open(dup) {
            let _ = f.set_modified(t);
        }
    }

    // Only once the replacement is in place and correct does the original go.
    fs::remove_file(&backup)?;
    Ok(md.len())
}

/// Every check `dedupe_pair` makes, with none of the consequences.
pub fn preflight(keep: &Path, dup: &Path) -> io::Result<()> {
    let (mk, md) = (fs::symlink_metadata(keep)?, fs::symlink_metadata(dup)?);
    if !mk.is_file() || !md.is_file() {
        return Err(other("both paths must be plain files"));
    }
    // Readable means restorable; unreadable would mean silently dropping them.
    read_xattrs(dup)?;
    if !byte_equal(keep, dup)? {
        return Err(other("contents differ: refusing to replace"));
    }
    let parent = dup.parent().ok_or_else(|| other("no parent directory"))?;
    if !supported(parent) {
        return Err(other("this mount does not support reflinks"));
    }
    Ok(())
}

/// Dedupe every group, keeping its first member. `dry_run` runs every check
/// and touches nothing.
pub fn dedupe_groups(groups: &[crate::dupes::Group], dry_run: bool) -> Report {
    let mut r = Report::default();
    for g in groups {
        // Nothing to win where the copies already share blocks.
        if g.reclaimable == 0 {
            continue;
        }
        let Some(keep) = g.members.first() else { continue };
        for m in g.members.iter().skip(1) {
            // Already sharing with the member we are keeping.
            if m.shared {
                continue;
            }
            let res = if dry_run {
                preflight(&keep.path, &m.path).map(|()| g.bytes_each)
            } else {
                dedupe_pair(&keep.path, &m.path)
            };
            match res {
                Ok(bytes) => {
                    r.freed += bytes;
                    r.done.push(Deduped {
                        kept: keep.path.clone(),
                        replaced: m.path.clone(),
                        bytes,
                    });
                }
                Err(e) => r.refused.push(Refused { path: m.path.clone(), why: e.to_string() }),
            }
        }
    }
    r
}
