use crate::Tree;
use std::io;
use std::path::Path;

pub struct Volume { pub total: u64, pub free: u64, pub used: u64 }

pub struct Reconciliation {
    pub used: u64,
    pub scanned: u64,
    /// Volume bytes not under the scan root, plus snapshots and purgeable
    /// caches. Reported as ONE bucket: per-snapshot bytes exist only on ZFS,
    /// so splitting this would be invented data on macOS and Windows.
    pub unaccounted: u64,
}

#[cfg(unix)]
pub fn volume_of(path: &Path) -> io::Result<Volume> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let bs = s.f_frsize as u64;
    let total = s.f_blocks as u64 * bs;
    let free = s.f_bavail as u64 * bs;
    Ok(Volume { total, free, used: total.saturating_sub(free) })
}

#[cfg(windows)]
pub fn volume_of(path: &Path) -> io::Result<Volume> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut w: Vec<u16> = path.as_os_str().encode_wide().collect();
    w.push(0);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Volume { total, free: avail, used: total.saturating_sub(avail) })
}

pub fn reconcile(t: &Tree, v: &Volume) -> Reconciliation {
    let scanned = if t.is_empty() { 0 } else { t.sub_blocks[0] };
    Reconciliation { used: v.used, scanned, unaccounted: v.used.saturating_sub(scanned) }
}
