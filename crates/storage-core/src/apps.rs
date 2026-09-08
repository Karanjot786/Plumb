//! Installed applications and the files they leave behind.
//!
//! Nothing in this module removes anything. Uninstall builds a plan and hands
//! it to `clean::stage`, which is the only code in the project allowed to move
//! a user's files and the only path with an undo manifest behind it. There is
//! deliberately no `fs::remove_*` call anywhere in this file.
//!
//! # Two rules govern leftover matching
//!
//! **Boundary-anchored, never substring.** `com.foo` matches `com.foo.plist`
//! and `com.foo.helper.plist` and must never match `com.foobar.plist`. Every
//! comparison here anchors on a dot or a path separator.
//!
//! **The bundle id is validated before it touches a path.** A malformed
//! `Info.plist` must not be able to walk out of a Library subtree or widen a
//! match, so the id is checked to be reverse-DNS with no separators and no
//! glob metacharacters *before* it is ever interpolated.
//!
//! # System paths are shown, never staged
//!
//! `/Library/**`, `/private/var/db/receipts` and the privileged helper and
//! launch directories are discovered and displayed so the user can see the
//! whole footprint, and are marked `system_level`. `stageable()` filters them
//! out structurally rather than relying on a caller to remember.
//!
//! # Provenance
//!
//! Every path below was derived from Apple's File System Programming Guide and
//! from inspecting a real `~/Library`. GPL-licensed uninstallers ship exactly
//! these lists, and those lists are the copyrightable asset rather than merely
//! an idea; none was consulted.

use crate::{NodeId, Tree};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct App {
    pub name: String,
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    pub path: PathBuf,
    pub bundle_bytes: u64,
    pub last_used: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    ApplicationSupport,
    Caches,
    Preferences,
    Containers,
    GroupContainers,
    SavedState,
    Logs,
    WebKit,
    HttpStorages,
    ApplicationScripts,
    Cookies,
    LaunchAgent,
    Bundle,
    SystemLevel,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::ApplicationSupport => "Application Support",
            Category::Caches => "Caches",
            Category::Preferences => "Preferences",
            Category::Containers => "Containers",
            Category::GroupContainers => "Group Containers",
            Category::SavedState => "Saved Application State",
            Category::Logs => "Logs",
            Category::WebKit => "WebKit",
            Category::HttpStorages => "HTTP Storages",
            Category::ApplicationScripts => "Application Scripts",
            Category::Cookies => "Cookies",
            Category::LaunchAgent => "Launch Agents",
            Category::Bundle => "Application bundle",
            Category::SystemLevel => "System (review only)",
        }
    }
}

/// Why we believe a path belongs to an app. Anything weaker than a proven
/// match stays out of the default list entirely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Evidence {
    /// A path component is exactly the bundle id, anchored at a boundary.
    ExactBundleIdMatch,
    /// The container's own metadata plist names this bundle id.
    ContainerMetadataVerified,
    /// The `.app` is already gone, so this is a true orphan.
    FormerBundleMissing,
    /// Name-keyed. The weakest signal, and never shown by default.
    NameMatch,
}

impl Evidence {
    pub fn label(self) -> &'static str {
        match self {
            Evidence::ExactBundleIdMatch => "bundle id match",
            Evidence::ContainerMetadataVerified => "container metadata verified",
            Evidence::FormerBundleMissing => "orphan, app already gone",
            Evidence::NameMatch => "name match (guess)",
        }
    }

    /// Only proven evidence appears without the user asking for guesses.
    pub fn proven(self) -> bool {
        !matches!(self, Evidence::NameMatch)
    }
}

#[derive(Clone, Debug)]
pub struct Associated {
    pub path: PathBuf,
    pub bytes: u64,
    pub category: Category,
    pub evidence: Evidence,
    /// Displayed, never stageable.
    pub system_level: bool,
}

pub trait AppProvider {
    fn list(&self) -> io::Result<Vec<App>>;
    fn associated(&self, app: &App) -> io::Result<Vec<Associated>>;
}

// ------------------------------------------------------------- validation

/// Reverse-DNS, nothing else. Rejects separators, `..`, glob metacharacters,
/// and anything that is not two or more dot-separated alphanumeric segments.
///
/// This runs before the id is interpolated into any path, which is the whole
/// point: a hostile or corrupt `Info.plist` cannot traverse or widen a match.
pub fn valid_bundle_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 255 {
        return false;
    }
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        return false;
    }
    segments.iter().all(|s| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// Boundary-anchored containment: `name` is `id`, or `id` followed by a dot.
///
/// `com.foo` matches `com.foo`, `com.foo.plist` and `com.foo.helper.plist`.
/// It does not match `com.foobar.plist`, because the character after the id is
/// `b` rather than a boundary.
pub fn id_matches(name: &str, id: &str) -> bool {
    if name == id {
        return true;
    }
    match name.strip_prefix(id) {
        Some(rest) => rest.starts_with('.'),
        None => false,
    }
}

/// Words too generic to key a filesystem match on. A name-keyed rule using any
/// of these would sweep in directories belonging to other software.
const NAME_DENYLIST: [&str; 24] = [
    "app", "apps", "application", "applications", "data", "cache", "caches", "log", "logs",
    "temp", "tmp", "user", "users", "system", "library", "support", "config", "settings",
    "preferences", "documents", "downloads", "desktop", "shared", "common",
];

/// Name-keyed matching is the weakest rule and needs guards or it matches half
/// the disk: long enough to be distinctive, not a generic word, never Apple's.
pub fn usable_as_name_key(name: &str) -> bool {
    let n = name.trim();
    n.chars().count() >= 5
        && !n.contains('/')
        && !n.contains("..")
        && !n.to_ascii_lowercase().starts_with("com.apple")
        && !NAME_DENYLIST.contains(&n.to_ascii_lowercase().as_str())
}

pub fn is_system_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s == "/Library"
        || s.starts_with("/Library/")
        || s.starts_with("/private/var/db/receipts")
        || s.starts_with("/System/")
}

/// The only way an `Associated` becomes eligible for staging. System-level
/// items are filtered structurally here rather than at each call site, so a
/// forgotten check cannot make one removable.
pub fn stageable(items: &[Associated]) -> Vec<&Associated> {
    items.iter().filter(|a| !a.system_level).collect()
}

// -------------------------------------------------------------- discovery

#[cfg(target_os = "macos")]
mod mac {
    use super::*;

    /// Where `.app` bundles live. `pkgutil` covers installers that put one
    /// somewhere else.
    fn bundle_roots() -> Vec<PathBuf> {
        let mut v = vec![PathBuf::from("/Applications"), PathBuf::from("/Applications/Utilities")];
        if let Some(h) = crate::blocklist::home() {
            v.push(h.join("Applications"));
        }
        v
    }

    fn plist_string(d: &plist::Dictionary, key: &str) -> Option<String> {
        d.get(key).and_then(|v| v.as_string()).map(|s| s.to_string())
    }

    /// Bundle identity from `Contents/Info.plist`.
    fn read_bundle(path: &Path) -> Option<App> {
        let info = path.join("Contents/Info.plist");
        let dict = plist::Value::from_file(&info).ok()?.into_dictionary()?;
        let file_name =
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // The bundle's own display name beats the file name, and both beat a
        // shell out to mdls.
        let name = plist_string(&dict, "CFBundleDisplayName")
            .or_else(|| plist_string(&dict, "CFBundleName"))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(file_name);
        let bundle_id = plist_string(&dict, "CFBundleIdentifier").filter(|s| valid_bundle_id(s));
        let version = plist_string(&dict, "CFBundleShortVersionString")
            .or_else(|| plist_string(&dict, "CFBundleVersion"));
        let last_used = std::fs::metadata(path)
            .and_then(|m| m.accessed())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().min(u32::MAX as u64) as u32);
        Some(App { name, bundle_id, version, path: path.to_path_buf(), bundle_bytes: 0, last_used })
    }

    /// `pkgutil --pkgs` then `--files`, restricted hard.
    ///
    /// A receipt lists every file the installer wrote, including shared system
    /// libraries. Treating that as an uninstall list would delete files other
    /// software depends on, so only `.app` bundles under `/usr/local` and
    /// `/opt` are accepted: the two places a package may legitimately put an
    /// application that `/Applications` enumeration would miss.
    fn from_pkgutil() -> Vec<PathBuf> {
        let Ok(pkgs) = std::process::Command::new("pkgutil").arg("--pkgs").output() else {
            return Vec::new();
        };
        if !pkgs.status.success() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for pkg in String::from_utf8_lossy(&pkgs.stdout).lines().take(400) {
            let Ok(files) = std::process::Command::new("pkgutil").arg("--files").arg(pkg).output()
            else {
                continue;
            };
            for line in String::from_utf8_lossy(&files.stdout).lines() {
                let abs = PathBuf::from("/").join(line);
                let s = abs.to_string_lossy().into_owned();
                if !(s.starts_with("/usr/local/") || s.starts_with("/opt/")) {
                    continue;
                }
                // Only the bundle directory itself, never its contents.
                if s.ends_with(".app") && abs.is_dir() {
                    out.push(abs);
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn list_apps() -> io::Result<Vec<App>> {
        let mut out: Vec<App> = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        for root in bundle_roots() {
            let Ok(rd) = std::fs::read_dir(&root) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "app") {
                    continue;
                }
                if let Some(app) = read_bundle(&p) {
                    seen.push(p);
                    out.push(app);
                }
            }
        }
        for p in from_pkgutil() {
            if seen.contains(&p) {
                continue;
            }
            if let Some(app) = read_bundle(&p) {
                out.push(app);
            }
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }
}

#[cfg(target_os = "macos")]
pub fn list_apps() -> io::Result<Vec<App>> {
    mac::list_apps()
}

#[cfg(not(target_os = "macos"))]
pub fn list_apps() -> io::Result<Vec<App>> {
    Ok(Vec::new())
}

/// Bundle ids claimed by more than one installed app.
///
/// The sibling guard: `Xcode.app` and `Xcode-beta.app` share an id, and their
/// support directories are shared too. When an id is contested only the `.app`
/// itself may be removed.
pub fn contested_ids(apps: &[App]) -> Vec<String> {
    let mut count: HashMap<&str, usize> = HashMap::new();
    for a in apps {
        if let Some(id) = &a.bundle_id {
            *count.entry(id.as_str()).or_default() += 1;
        }
    }
    let mut v: Vec<String> =
        count.into_iter().filter(|(_, n)| *n > 1).map(|(id, _)| id.to_string()).collect();
    v.sort();
    v
}

/// Directory size by walking it directly. Used for bundles, which are small
/// enough that a scan of the whole volume is not worth it.
pub fn dir_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if !meta.is_dir() {
        return block_bytes(&meta);
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.file_type().is_symlink() {
                continue;
            }
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += block_bytes(&m);
            }
        }
    }
    total
}

fn block_bytes(m: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        m.len()
    }
}

/// Size from an already-scanned tree where it covers the path, else a direct
/// walk. Keeps the UI from rescanning the disk per application.
pub fn size_of(path: &Path, tree: Option<(&Tree, &Path)>) -> u64 {
    if let Some((t, root)) = tree {
        if let Some(id) = node_for(t, root, path) {
            return t.sub_blocks[id as usize];
        }
    }
    dir_bytes(path)
}

fn node_for(t: &Tree, root: &Path, path: &Path) -> Option<NodeId> {
    let rel = path.strip_prefix(root.parent()?).ok()?;
    let want = rel.to_string_lossy();
    (0..t.len() as NodeId).find(|&i| t.path(i) == want)
}
