//! Staging, undo manifest, restore, commit.
//!
//! The whole design is one sentence: **nothing is ever deleted except by
//! `commit`, and everything `commit` can delete is described by a manifest that
//! was written to disk before the first file moved.**
//!
//! Consequences that are load-bearing, not stylistic:
//!
//! * Staging is `rename(2)` on the same device. A copy would double the disk
//!   usage of a cleanup tool, and copy-then-delete has a window where the data
//!   exists nowhere complete. If a same-device staging directory cannot be
//!   made, the item is refused — there is no fallback path that deletes.
//! * The mount point of a volume is found by walking up until `dev` changes,
//!   not by enumerating mounts. `getmntent`/`getmntinfo` are not thread-safe,
//!   which is exactly the hazard the design doc calls out in the `trash` crate.
//! * Identity is `(dev, ino, mtime)` captured at plan time and re-checked
//!   immediately before the move. Comparing path strings would not survive a
//!   rename between the two.

use crate::blocklist::{self, identity};
use crate::{NodeId, Tree};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_TTL_DAYS: u64 = 30;

/// Why an item will not be touched. Every variant means "we did nothing".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Blocked(&'static str),
    Changed,
    CrossDevice,
    Missing,
    IsMountPoint,
    EscapesRoot,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Blocked(why) => write!(f, "blocked: {why}"),
            Refusal::Changed => write!(f, "changed since it was inspected"),
            Refusal::CrossDevice => write!(f, "no staging directory on its volume"),
            Refusal::Missing => write!(f, "no longer exists"),
            Refusal::IsMountPoint => write!(f, "is a mount point"),
            Refusal::EscapesRoot => write!(f, "symlink resolves outside the scan root"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StagedItem {
    pub original: PathBuf,
    pub staged: PathBuf,
    pub bytes: u64,
    /// Identity captured at plan time; re-checked immediately before the move.
    pub dev: u64,
    pub ino: u64,
    pub mtime: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub id: u64,
    pub created_at: u64,
    pub label: String,
    pub expires_at: u64,
    pub items: Vec<StagedItem>,
    pub total_bytes: u64,
}

impl Manifest {
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

/// The result of planning. `id` is fixed here rather than at stage time so
/// `--dry-run` can print the exact manifest a later `stage` would write.
pub struct Plan {
    pub id: u64,
    pub staged: Vec<StagedItem>,
    pub refused: Vec<(PathBuf, Refusal)>,
    pub total_bytes: u64,
}

impl Plan {
    /// Fold another plan into this one, re-homing its staged paths under this
    /// plan's id and re-indexing their names, so one command yields exactly one
    /// manifest and one `restore` undoes all of it.
    pub fn absorb(&mut self, other: Plan) {
        for mut item in other.staged {
            let name = item.original.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if let Some(root) = item.staged.parent().and_then(|p| p.parent()) {
                item.staged = root
                    .join(self.id.to_string())
                    .join(format!("{:05}-{name}", self.staged.len()));
            }
            self.total_bytes += item.bytes;
            self.staged.push(item);
        }
        self.refused.extend(other.refused);
        debug_assert_eq!(
            self.total_bytes,
            self.staged.iter().map(|i| i.bytes).sum::<u64>(),
            "plan total disagrees with its items after absorb"
        );
    }

    /// The manifest `stage` would write, without writing it.
    pub fn manifest(&self, label: &str) -> Manifest {
        Manifest {
            id: self.id,
            created_at: self.id,
            label: label.to_string(),
            expires_at: self.id + DEFAULT_TTL_DAYS * 86_400,
            items: self.staged.clone(),
            total_bytes: self.total_bytes,
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ------------------------------------------------------------- directories

/// Per-user application directory. The manifest index always lives here, even
/// when a payload had to be staged on another volume.
pub fn central() -> io::Result<PathBuf> {
    let missing =
        || io::Error::new(io::ErrorKind::NotFound, "no home directory in the environment");

    #[cfg(target_os = "macos")]
    let base = blocklist::home().ok_or_else(missing)?.join("Library/Application Support");

    #[cfg(all(unix, not(target_os = "macos")))]
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => blocklist::home().ok_or_else(missing)?.join(".local/share"),
    };

    #[cfg(windows)]
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or_else(missing)?);

    Ok(base.join("sv"))
}

pub fn manifests_dir() -> io::Result<PathBuf> {
    let d = central()?.join("manifests");
    fs::create_dir_all(&d)?;
    Ok(d)
}

fn private_dir(p: &Path) -> io::Result<()> {
    fs::create_dir_all(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Mount point of whatever volume holds `path`, found by walking up until the
/// device number changes. No mount table is enumerated.
fn mount_point_of(path: &Path) -> Option<PathBuf> {
    let dev = identity(path).ok()?.dev;
    let mut cur = path.to_path_buf();
    loop {
        let Some(parent) = cur.parent() else { return Some(cur) };
        match identity(parent) {
            Ok(p) if p.dev == dev => cur = parent.to_path_buf(),
            _ => return Some(cur),
        }
    }
}

/// Staging root on the *same device* as `path`, so the move is a rename.
/// Returns `Err` rather than choosing any strategy that would copy or delete.
fn staging_root_for(path: &Path) -> io::Result<PathBuf> {
    let want = identity(path)?.dev;

    let home_staging = central()?.join("staging");
    private_dir(&home_staging)?;
    if identity(&home_staging)?.dev == want {
        return Ok(home_staging);
    }

    let mp = mount_point_of(path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no mount point for path"))?;
    let alt = mp.join(".sv-staging");
    private_dir(&alt)?;
    if identity(&alt)?.dev != want {
        return Err(io::Error::other("staging directory is on another device"));
    }
    Ok(alt)
}

/// `Path::exists` follows symlinks, so a staged symlink whose relative target
/// does not resolve inside the staging directory reads as absent. Everything
/// here asks whether the *entry* exists, never whether its target does.
fn entry_exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// Hard gate on every removal. Not a `debug_assert`: this is the check that
/// stands between a bug in path handling and someone's home directory.
fn assert_in_staging(p: &Path) -> io::Result<()> {
    let deny = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to touch {} : {why}", p.display()),
        ))
    };
    if !p.is_absolute() {
        return deny("not an absolute path");
    }
    // A `..` anywhere makes every prefix test below meaningless:
    // `/Volumes/X/.sv-staging/../../../Users/me/Documents` contains the marker
    // and still walks straight out of the staging area.
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return deny("path contains a .. component");
    }

    let home = central()?.join("staging");
    let home = home.canonicalize().unwrap_or(home);
    if p.starts_with(&home) {
        return Ok(());
    }

    // Per-volume root. Rebuilt from the path itself, then required to be a real
    // directory of that exact name sitting on a mount point, which is the only
    // place `staging_root_for` ever creates one.
    let mut acc = PathBuf::new();
    for c in p.components() {
        acc.push(c);
        if c.as_os_str() == ".sv-staging" {
            let ok = acc.is_dir()
                && acc.parent().map(blocklist::is_mount_point).unwrap_or(false)
                && p.starts_with(&acc);
            return if ok { Ok(()) } else { deny("not a staging directory on a mount point") };
        }
    }
    deny("not inside a staging directory")
}

// -------------------------------------------------------------------- plan

/// Absolute path of a node. `Tree::path` is rooted at the scan root's own
/// basename, so the parent of the scan root is what it must be joined onto.
fn abs_of(root: &Path, tree: &Tree, id: NodeId) -> Option<PathBuf> {
    let base = root.parent()?;
    let p = base.join(tree.path(id));
    debug_assert!(p.starts_with(root), "{} escaped scan root {}", p.display(), root.display());
    p.starts_with(root).then_some(p)
}

/// Drop any id already covered by another selected id. Staging a parent first
/// would make every selected descendant `Missing`, and the byte total would
/// double-count.
fn drop_covered(tree: &Tree, ids: &[NodeId]) -> Vec<NodeId> {
    let mut v: Vec<NodeId> = ids.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut out: Vec<NodeId> = Vec::with_capacity(v.len());
    for id in v {
        let covered = out.iter().any(|&a| {
            let end = a + tree.subtree_len[a as usize];
            id > a && id < end
        });
        if !covered {
            out.push(id);
        }
    }
    out
}

/// Pure inspection. Touches nothing, creates nothing except the staging
/// directory it needs in order to know whether a rename is even possible.
pub fn plan(tree: &Tree, root: &Path, ids: &[NodeId]) -> Plan {
    let root = blocklist::canon_keep_link(root);
    let id = free_manifest_id();
    let mut staged = Vec::new();
    let mut refused = Vec::new();
    let mut total_bytes = 0u64;

    for node in drop_covered(tree, ids) {
        let Some(path) = abs_of(&root, tree, node) else {
            refused.push((PathBuf::from(tree.path(node)), Refusal::EscapesRoot));
            continue;
        };

        if let Some(why) = blocklist::shape_problem(&path) {
            refused.push((path, Refusal::Blocked(why)));
            continue;
        }
        if let Some(why) = blocklist::denied(&path) {
            refused.push((path, Refusal::Blocked(why)));
            continue;
        }
        if blocklist::is_mount_point(&path) {
            refused.push((path, Refusal::IsMountPoint));
            continue;
        }
        if blocklist::symlink_escapes(&path, &root) {
            refused.push((path, Refusal::EscapesRoot));
            continue;
        }
        if let Some(why) = blocklist::package_owned(&path) {
            refused.push((path, Refusal::Blocked(why)));
            continue;
        }

        let Ok(ident) = identity(&path) else {
            refused.push((path, Refusal::Missing));
            continue;
        };
        let Ok(root_dir) = staging_root_for(&path) else {
            refused.push((path, Refusal::CrossDevice));
            continue;
        };

        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let target = root_dir.join(id.to_string()).join(format!("{:05}-{name}", staged.len()));
        debug_assert!(target.starts_with(&root_dir), "staged path escaped the staging root");

        // The tree's aggregate is the honest number for a directory; a bare
        // stat would report the directory entry, not its contents.
        let bytes = tree.sub_excl[node as usize].max(ident.bytes);
        total_bytes += bytes;
        staged.push(StagedItem {
            original: path,
            staged: target,
            bytes,
            dev: ident.dev,
            ino: ident.ino,
            mtime: ident.mtime,
        });
    }

    debug_assert_eq!(
        total_bytes,
        staged.iter().map(|i| i.bytes).sum::<u64>(),
        "plan total disagrees with its items"
    );
    Plan { id, staged, refused, total_bytes }
}

fn free_manifest_id() -> u64 {
    let mut id = now_secs();
    if let Ok(dir) = manifests_dir() {
        while dir.join(format!("{id}.json")).exists() {
            id += 1;
        }
    }
    id
}

// ------------------------------------------------------------------- stage

fn write_manifest(m: &Manifest) -> io::Result<()> {
    debug_assert_eq!(
        m.total_bytes,
        m.items.iter().map(|i| i.bytes).sum::<u64>(),
        "manifest total disagrees with its items"
    );
    let dir = manifests_dir()?;
    let path = dir.join(format!("{}.json", m.id));
    let json =
        serde_json::to_vec_pretty(m).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut f = fs::File::create(&path)?;
    {
        use io::Write;
        f.write_all(&json)?;
        f.flush()?;
    }
    f.sync_all()?;
    // Durability of the directory entry itself, so a crash cannot leave a
    // manifest that exists but is not linked.
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn read_manifest(id: u64) -> io::Result<Manifest> {
    let path = manifests_dir()?.join(format!("{id}.json"));
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Move every planned item into staging. The manifest is written first, so a
/// crash at any point after that line leaves something `restore` can finish.
pub fn stage(plan: &Plan, label: &str) -> io::Result<Manifest> {
    let mut manifest = plan.manifest(label);
    write_manifest(&manifest)?;

    let mut moved: Vec<StagedItem> = Vec::with_capacity(manifest.items.len());
    for item in &manifest.items {
        // TOCTOU defence: it must still be the same file.
        let Ok(now) = identity(&item.original) else { continue };
        if now.dev != item.dev || now.ino != item.ino || now.mtime != item.mtime {
            continue;
        }
        let Some(parent) = item.staged.parent() else { continue };
        if private_dir(parent).is_err() {
            continue;
        }
        if fs::rename(&item.original, &item.staged).is_ok() {
            moved.push(item.clone());
        }
    }

    manifest.total_bytes = moved.iter().map(|i| i.bytes).sum();
    manifest.items = moved;
    write_manifest(&manifest)?;
    Ok(manifest)
}

pub fn list_staged() -> io::Result<Vec<Manifest>> {
    let dir = manifests_dir()?;
    let mut out = Vec::new();
    for e in fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        if e.path().extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(e.path()) else { continue };
        let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else { continue };
        // An emptied manifest has already been restored.
        if !m.items.is_empty() {
            out.push(m);
        }
    }
    out.sort_by_key(|m| m.created_at);
    Ok(out)
}

// ----------------------------------------------------------------- restore

/// Put everything back. Never clobbers: if something already occupies the
/// original path, that item stays staged and stays in the manifest, so the user
/// can retry after clearing the way.
pub fn restore(id: u64) -> io::Result<usize> {
    let mut m = read_manifest(id)?;
    let mut restored = 0usize;
    let mut left: Vec<StagedItem> = Vec::new();

    for item in m.items.iter().rev() {
        if !entry_exists(&item.staged) {
            if entry_exists(&item.original) {
                // Already back where it belongs; nothing to undo.
                continue;
            }
            // Present in neither place. Keep it listed rather than quietly
            // forgetting a path we are supposed to be accountable for.
            left.push(item.clone());
            continue;
        }
        if entry_exists(&item.original) {
            left.push(item.clone());
            continue;
        }
        if let Some(parent) = item.original.parent() {
            if fs::create_dir_all(parent).is_err() {
                left.push(item.clone());
                continue;
            }
        }
        match fs::rename(&item.staged, &item.original) {
            Ok(()) => restored += 1,
            Err(_) => left.push(item.clone()),
        }
    }

    left.reverse();
    m.total_bytes = left.iter().map(|i| i.bytes).sum();
    m.items = left;
    write_manifest(&m)?;
    Ok(restored)
}

// ------------------------------------------------------------------ commit

/// What a commit actually did. `skipped` is never empty silently: an item this
/// function declined to remove is an item the user still has, and they are told.
#[derive(Debug, Default)]
pub struct Removal {
    pub freed: u64,
    pub skipped: Vec<(PathBuf, String)>,
}

/// Permanent removal. The only function in the project that deletes anything,
/// and it refuses any path not inside a staging directory.
pub fn commit(id: u64) -> io::Result<Removal> {
    let mut m = read_manifest(id)?;
    let mut out = Removal::default();
    let mut left: Vec<StagedItem> = Vec::new();

    for item in &m.items {
        if let Err(e) = assert_in_staging(&item.staged) {
            out.skipped.push((item.staged.clone(), e.to_string()));
            left.push(item.clone());
            continue;
        }
        let meta = match fs::symlink_metadata(&item.staged) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Already gone. Nothing to free, nothing to keep listed.
                continue;
            }
            Err(e) => {
                out.skipped.push((item.staged.clone(), e.to_string()));
                left.push(item.clone());
                continue;
            }
        };
        let res = if meta.is_dir() && !meta.file_type().is_symlink() {
            fs::remove_dir_all(&item.staged)
        } else {
            fs::remove_file(&item.staged)
        };
        match res {
            Ok(()) => out.freed += item.bytes,
            Err(e) => {
                out.skipped.push((item.staged.clone(), e.to_string()));
                left.push(item.clone());
            }
        }
    }

    let manifest_path = manifests_dir()?.join(format!("{id}.json"));
    if left.is_empty() {
        // Everything went. Manifest and its now-empty payload directory last,
        // so a failure partway through still leaves a manifest describing what
        // remains.
        if manifest_path.exists() {
            fs::remove_file(&manifest_path)?;
        }
        for item in &m.items {
            if let Some(parent) = item.staged.parent() {
                if assert_in_staging(parent).is_ok() {
                    let _ = fs::remove_dir(parent);
                }
            }
        }
    } else {
        // Keep the survivors addressable. Dropping the manifest here would
        // orphan the very files we just failed to remove.
        m.total_bytes = left.iter().map(|i| i.bytes).sum();
        m.items = left;
        write_manifest(&m)?;
    }
    Ok(out)
}
