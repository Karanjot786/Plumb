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
    Autosave,
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
            Category::Autosave => "Autosave Information",
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
    /// Another installed app claims the same bundle id, so this data is not
    /// ours alone. Displayed, never stageable — the sibling guard.
    pub shared: bool,
}

pub trait AppProvider {
    fn list(&self) -> io::Result<Vec<App>>;
    fn associated(&self, app: &App) -> io::Result<Vec<Associated>>;
    /// What an uninstall would do. Builds nothing on disk.
    fn uninstall_plan(&self, app: &App) -> io::Result<CleanupPlan>;
}

// ------------------------------------------------------------- validation

/// Reverse-DNS, nothing else. Rejects separators, `..`, glob metacharacters,
/// and anything that is not three or more dot-separated alphanumeric segments.
///
/// This runs before the id is interpolated into any path, which is the whole
/// point: a hostile or corrupt `Info.plist` cannot traverse or widen a match.
///
/// **Three segments, not two.** A two-segment id is a vendor namespace rather
/// than an application - `com.google`, `com.adobe`, `com.apple` - and because
/// matching is descendant-inclusive by design, accepting one would sweep an
/// entire vendor's data out of `~/Library`. `$HOME/Library` is not on the
/// blocklist, so nothing downstream would catch it. The cost is that a
/// genuinely two-segment id loses leftover detection; that is the safe
/// direction, since the app bundle still stages and only the guessing stops.
pub fn valid_bundle_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 255 {
        return false;
    }
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 3 {
        return false;
    }
    segments.iter().all(|s| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

/// Whether an id may be used to key a filesystem match.
///
/// Stricter than `valid_bundle_id`: Apple's namespace is refused outright, the
/// way `usable_as_name_key` already refuses it for names. Any app can declare
/// `CFBundleIdentifier = com.apple.Safari`, and descendant matching would then
/// hand it Safari's containers, caches and preferences.
pub fn usable_as_id_key(id: &str) -> bool {
    if !valid_bundle_id(id) {
        return false;
    }
    let lower = id.to_ascii_lowercase();
    lower != "com.apple" && !lower.starts_with("com.apple.")
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

/// A leftover that is *provably* ours to remove.
///
/// This is the type-level guarantee, not a check a later refactor can forget.
/// The inner field is private and `new` is the only constructor, so there is no
/// way to obtain a `Removable` for a system path or for data an installed
/// sibling still uses. Anything that builds a cleanup plan takes `Removable`,
/// which means the plan simply cannot contain such a path — no runtime filter
/// stands between the two.
#[derive(Clone, Debug)]
pub struct Removable(Associated);

impl Removable {
    /// `None` for anything that must never be staged. The system test is
    /// re-derived from the path itself rather than trusting the `system_level`
    /// flag, so a wrongly-built `Associated` cannot smuggle one through.
    pub fn new(a: &Associated) -> Option<Removable> {
        if a.system_level || a.shared || is_system_path(&a.path) {
            return None;
        }
        Some(Removable(a.clone()))
    }

    pub fn path(&self) -> &Path {
        &self.0.path
    }

    pub fn item(&self) -> &Associated {
        &self.0
    }
}

/// The only way an `Associated` becomes eligible for staging.
pub fn stageable(items: &[Associated]) -> Vec<Removable> {
    let out: Vec<Removable> = items.iter().filter_map(Removable::new).collect();
    debug_assert!(
        out.iter().all(|r| !is_system_path(r.path())),
        "a system path reached the stageable set"
    );
    out
}

/// Why a discovered item was kept out of the removable set. Shown so the user
/// sees the whole footprint and understands what was left alone.
pub fn exclusion_reason(a: &Associated) -> Option<&'static str> {
    if a.system_level || is_system_path(&a.path) {
        Some("system level - review only, not removable here")
    } else if a.shared {
        Some("another installed app claims this bundle id")
    } else {
        None
    }
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

/// Every *other* installed app's bundle id, excluding the one at `idx`.
///
/// Excluding by slot rather than by value matters: two apps can declare the
/// same id, and that second app is exactly the sibling the guard exists for.
pub fn other_ids(apps: &[App], idx: usize) -> Vec<String> {
    apps.iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .filter_map(|(_, a)| a.bundle_id.clone())
        .collect()
}

/// Installed ids that boundary-match this app's own id in either direction.
///
/// The sibling guard, and it must be boundary-aware rather than exact.
/// `id_matches` deliberately claims descendants, so `com.google.Chrome` also
/// matches Chrome Canary's `com.google.Chrome.canary` files - but an exact
/// equality test never notices the two apps are related, and Chrome's
/// uninstall would carry Canary's profile away with it while Canary is still
/// installed. Matching in *either* direction catches both orderings.
pub fn contesting_ids(apps: &[App], idx: usize) -> Vec<String> {
    let Some(id) = apps.get(idx).and_then(|a| a.bundle_id.as_deref()) else {
        return Vec::new();
    };
    let mut v: Vec<String> = other_ids(apps, idx)
        .into_iter()
        .filter(|o| id_matches(o, id) || id_matches(id, o))
        .collect();
    v.sort();
    v.dedup();
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

// ------------------------------------------------------- leftover discovery

/// What a leftover scan is allowed to guess at.
#[derive(Clone, Copy, Default, Debug)]
pub struct LeftoverOpts {
    /// Include name-keyed matches. These are guesses, never proven, and are
    /// labelled as such wherever they surface.
    pub include_name_matches: bool,
}

#[cfg(target_os = "macos")]
mod leftovers {
    use super::*;
    use std::collections::HashSet;

    /// User-level directories keyed by bundle id.
    ///
    /// Every entry was derived from Apple's File System Programming Guide and
    /// from inspecting a real `~/Library` on this machine. GPL uninstallers
    /// ship lists of exactly this shape and those lists are the copyrightable
    /// asset rather than an idea; none was consulted.
    const ID_DIRS: [(&str, Category); 14] = [
        ("Application Support", Category::ApplicationSupport),
        ("Caches", Category::Caches),
        ("Logs", Category::Logs),
        ("Preferences", Category::Preferences),
        ("Preferences/ByHost", Category::Preferences),
        ("Containers", Category::Containers),
        ("Group Containers", Category::GroupContainers),
        ("Saved Application State", Category::SavedState),
        ("WebKit", Category::WebKit),
        ("HTTPStorages", Category::HttpStorages),
        ("Application Scripts", Category::ApplicationScripts),
        ("Cookies", Category::Cookies),
        ("Autosave Information", Category::Autosave),
        ("LaunchAgents", Category::LaunchAgent),
    ];

    /// Name-keyed directories. Only these two: an app's own vendor folder
    /// lands here and nowhere else, and every other Library subtree is keyed
    /// by id, so a name rule there would be pure guesswork.
    const NAME_DIRS: [(&str, Category); 2] = [
        ("Application Support", Category::ApplicationSupport),
        ("Caches", Category::Caches),
    ];

    /// Discovered and displayed. Never removable — see `Removable`.
    const SYSTEM_DIRS: [&str; 8] = [
        "/Library/Application Support",
        "/Library/Caches",
        "/Library/Logs",
        "/Library/Preferences",
        "/Library/LaunchAgents",
        "/Library/LaunchDaemons",
        "/Library/PrivilegedHelperTools",
        "/private/var/db/receipts",
    ];

    const CONTAINER_META: &str = ".com.apple.containermanagerd.metadata.plist";

    /// Boundary-anchored entry match.
    ///
    /// `group` relaxes only the *left* side, for group container directories
    /// which carry a leading team id or literal `group` segment
    /// (`ABCDE12345.com.foo`, `group.com.foo`). At most two leading segments
    /// are stripped and the full id is still required to start at a segment
    /// boundary, so this never becomes a substring test.
    fn entry_matches(name: &str, id: &str, group: bool) -> bool {
        if id_matches(name, id) {
            return true;
        }
        if !group {
            return false;
        }
        let mut rest = name;
        for _ in 0..2 {
            match rest.split_once('.') {
                Some((_, r)) => {
                    rest = r;
                    if id_matches(rest, id) {
                        return true;
                    }
                }
                None => break,
            }
        }
        false
    }

    /// The bundle id a container declares for itself. Catches containers whose
    /// directory name is not the id.
    fn container_declares(dir: &Path) -> Option<String> {
        let v = plist::Value::from_file(dir.join(CONTAINER_META)).ok()?;
        let d = v.into_dictionary()?;
        d.get("MCMMetadataIdentifier").and_then(|x| x.as_string()).map(|s| s.to_string())
    }

    fn push(
        out: &mut Vec<Associated>,
        seen: &mut HashSet<PathBuf>,
        path: PathBuf,
        category: Category,
        evidence: Evidence,
        system_level: bool,
        shared: bool,
    ) {
        if !seen.insert(path.clone()) {
            return;
        }
        let bytes = dir_bytes(&path);
        out.push(Associated { path, bytes, category, evidence, system_level, shared });
    }

    /// Whether some *other* installed app also claims this entry.
    ///
    /// Sharing is a property of the entry, not of the app: `com.google.Chrome`
    /// matches both `~/Library/Caches/com.google.Chrome` and
    /// `~/Library/Caches/com.google.Chrome.canary`, and only the second one
    /// belongs to somebody else.
    ///
    /// An id less specific than ours is not a competing claim. Uninstalling
    /// Canary must still be able to take Canary's own files even though
    /// Chrome's `com.google.Chrome` boundary-matches their names; Chrome is an
    /// ancestor claim, and the most specific installed id owns the entry.
    /// Anything as specific as ours, or more, does make it shared.
    fn claimed_by_other(name: &str, id: &str, others: &[String], group: bool) -> bool {
        others.iter().any(|o| {
            entry_matches(name, o, group) && !(o != id && id_matches(id, o))
        })
    }

    /// Every path a scan of `dir` attributes to `id`.
    fn sweep(
        dir: &Path,
        id: &str,
        others: &[String],
        category: Category,
        group: bool,
        orphan: bool,
        system_level: bool,
        out: &mut Vec<Associated>,
        seen: &mut HashSet<PathBuf>,
    ) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let by_name = entry_matches(&name, id, group);
            // A container may be named something else and still declare the id.
            let declared = if by_name || category != Category::Containers {
                None
            } else {
                container_declares(&e.path())
            };
            let by_meta = declared.as_deref() == Some(id);
            if !by_name && !by_meta {
                continue;
            }
            let evidence = if by_meta {
                Evidence::ContainerMetadataVerified
            } else if orphan {
                Evidence::FormerBundleMissing
            } else {
                Evidence::ExactBundleIdMatch
            };
            // A container that declares an id is claimed through that id, not
            // through its directory name.
            let key = if by_meta { id } else { name.as_str() };
            let shared = claimed_by_other(key, id, others, group && !by_meta);
            push(out, seen, e.path(), category, evidence, system_level, shared);
        }
    }

    pub fn find(app: &App, others: &[String], opts: LeftoverOpts) -> Vec<Associated> {
        let mut out = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        // The bundle itself is always ours, even when the id is contested:
        // two apps sharing an id still have two distinct `.app` directories.
        if app.path.exists() {
            push(
                &mut out,
                &mut seen,
                app.path.clone(),
                Category::Bundle,
                Evidence::ExactBundleIdMatch,
                is_system_path(&app.path),
                false,
            );
        }

        let Some(home) = crate::blocklist::home() else { return out };
        let lib = home.join("Library");
        let orphan = !app.path.exists();

        if let Some(id) = &app.bundle_id {
            // Revalidate at the point of use. `list_apps` already filters, but
            // an `App` can be built by a caller, and this is the last line
            // before the id is interpolated into a path. `usable_as_id_key`
            // also refuses Apple's namespace and any two-segment vendor id.
            if !usable_as_id_key(id) {
                return out;
            }
            debug_assert!(!id.contains('/') && !id.contains('*') && !id.contains(".."));
            debug_assert!(id.split('.').count() >= 3, "vendor namespace reached a sweep");

            for (sub, cat) in ID_DIRS {
                let group = cat == Category::GroupContainers;
                sweep(
                    &lib.join(sub),
                    id,
                    others,
                    cat,
                    group,
                    orphan,
                    false,
                    &mut out,
                    &mut seen,
                );
            }
            for sys in SYSTEM_DIRS {
                sweep(
                    Path::new(sys),
                    id,
                    others,
                    Category::SystemLevel,
                    false,
                    orphan,
                    true,
                    &mut out,
                    &mut seen,
                );
            }
        }

        // Name-keyed. The weakest rule by far, so it is off unless asked for,
        // guarded by `usable_as_name_key`, and never marked proven.
        if opts.include_name_matches && usable_as_name_key(&app.name) {
            // A name-keyed hit is not keyed on an id at all, so the best we
            // can say is whether anybody else claims this app's id.
            let shared = app
                .bundle_id
                .as_ref()
                .map(|id| others.iter().any(|o| id_matches(o, id) || id_matches(id, o)))
                .unwrap_or(false);
            let want = app.name.trim().to_ascii_lowercase();
            for (sub, cat) in NAME_DIRS {
                let dir = lib.join(sub);
                let Ok(rd) = std::fs::read_dir(&dir) else { continue };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.to_ascii_lowercase() != want {
                        continue;
                    }
                    push(
                        &mut out,
                        &mut seen,
                        e.path(),
                        cat,
                        Evidence::NameMatch,
                        false,
                        shared,
                        );
                }
            }
        }

        out.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        out
    }
}

/// Every file and directory we can attribute to `app`.
///
/// `others` is `other_ids` over the installed set - every other app's bundle
/// id. Each matched entry is marked `shared` when one of those also claims it,
/// which is what keeps an uninstall of `com.google.Chrome` from carrying away
/// `com.google.Chrome.canary`'s profile. An empty slice disables the guard,
/// which is only ever correct when there is genuinely one app.
#[cfg(target_os = "macos")]
pub fn associated_for(app: &App, others: &[String], opts: LeftoverOpts) -> Vec<Associated> {
    leftovers::find(app, others, opts)
}

#[cfg(not(target_os = "macos"))]
pub fn associated_for(_app: &App, _others: &[String], _opts: LeftoverOpts) -> Vec<Associated> {
    Vec::new()
}

/// An `AppProvider` over the machine this is running on. Holds the contested
/// id set so every `associated` call has the sibling guard available without
/// re-enumerating every application.
pub struct Local {
    apps: Vec<App>,
    pub opts: LeftoverOpts,
}

impl Local {
    pub fn new() -> io::Result<Local> {
        Ok(Local { apps: list_apps()?, opts: LeftoverOpts::default() })
    }

    /// The slot an app occupies, so the sibling guard can exclude it by
    /// position rather than by id value.
    pub fn index_of(&self, app: &App) -> Option<usize> {
        self.apps.iter().position(|a| a.path == app.path)
    }

    pub fn others_for(&self, app: &App) -> Vec<String> {
        match self.index_of(app) {
            Some(i) => other_ids(&self.apps, i),
            // Unknown app: every installed id is somebody else's.
            None => self.apps.iter().filter_map(|a| a.bundle_id.clone()).collect(),
        }
    }

    pub fn contesting_for(&self, app: &App) -> Vec<String> {
        match self.index_of(app) {
            Some(i) => contesting_ids(&self.apps, i),
            None => Vec::new(),
        }
    }
}

impl AppProvider for Local {
    fn list(&self) -> io::Result<Vec<App>> {
        Ok(self.apps.clone())
    }

    fn associated(&self, app: &App) -> io::Result<Vec<Associated>> {
        Ok(associated_for(app, &self.others_for(app), self.opts))
    }

    fn uninstall_plan(&self, app: &App) -> io::Result<CleanupPlan> {
        Ok(uninstall_plan(app, &self.others_for(app), self.opts))
    }
}

// ------------------------------------------------------------- uninstall

/// A background job that must stop before its files move.
///
/// A login item or launch agent that is still running will happily recreate
/// the directories we just staged, so the user ends up with a half-uninstalled
/// app and no error to explain it.
#[derive(Clone, Debug)]
pub enum Unload {
    /// A login item helper inside the bundle, addressed by its own bundle id.
    LoginItem(String),
    /// A launch agent plist in the user's `LaunchAgents`.
    Agent(PathBuf),
}

impl Unload {
    pub fn label(&self) -> String {
        match self {
            Unload::LoginItem(id) => format!("login item {id}"),
            Unload::Agent(p) => format!("launch agent {}", p.display()),
        }
    }
}

/// Everything an uninstall would do, before any of it is done.
///
/// `items` is `Vec<Removable>` and there is no other way in, so a system path
/// or a sibling's shared data cannot appear here. `excluded` carries the rest
/// so the review sheet can show the whole footprint and say what was spared.
pub struct CleanupPlan {
    pub app: String,
    pub bundle_id: Option<String>,
    pub items: Vec<Removable>,
    pub excluded: Vec<(PathBuf, &'static str)>,
    pub unload: Vec<Unload>,
    pub bytes: u64,
}

impl CleanupPlan {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Login item helpers shipped inside a bundle.
#[cfg(target_os = "macos")]
fn login_items(bundle: &Path) -> Vec<String> {
    let dir = bundle.join("Contents/Library/LoginItems");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "app") {
            continue;
        }
        let info = p.join("Contents/Info.plist");
        let Ok(v) = plist::Value::from_file(&info) else { continue };
        let Some(d) = v.into_dictionary() else { continue };
        let Some(id) = d.get("CFBundleIdentifier").and_then(|x| x.as_string()) else { continue };
        // Same gate as everywhere else: this id becomes a launchctl argument.
        if valid_bundle_id(id) {
            out.push(id.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(not(target_os = "macos"))]
fn login_items(_bundle: &Path) -> Vec<String> {
    Vec::new()
}

/// Build the plan. Touches nothing.
///
/// `others` is `other_ids` over the installed set; see `associated_for`.
pub fn uninstall_plan(app: &App, others: &[String], opts: LeftoverOpts) -> CleanupPlan {
    let found = associated_for(app, others, opts);
    let items = stageable(&found);
    let excluded: Vec<(PathBuf, &'static str)> = found
        .iter()
        .filter_map(|a| exclusion_reason(a).map(|why| (a.path.clone(), why)))
        .collect();

    let mut unload: Vec<Unload> =
        login_items(&app.path).into_iter().map(Unload::LoginItem).collect();
    for r in &items {
        if r.item().category == Category::LaunchAgent {
            unload.push(Unload::Agent(r.path().to_path_buf()));
        }
    }

    let bytes = items.iter().map(|r| r.item().bytes).sum();
    debug_assert!(
        items.iter().all(|r| !is_system_path(r.path())),
        "a system path reached a CleanupPlan"
    );
    CleanupPlan {
        app: app.name.clone(),
        bundle_id: app.bundle_id.clone(),
        items,
        excluded,
        unload,
        bytes,
    }
}

/// Stop the app's background jobs so nothing recreates its files mid-move.
///
/// Call only after the user has confirmed the uninstall. Every failure is
/// returned rather than raised: a helper that was not running is the common
/// case and is not an error, and a helper that will not stop is worth telling
/// the user about without abandoning the uninstall.
#[cfg(target_os = "macos")]
pub fn perform_unload(plan: &CleanupPlan) -> Vec<(String, String)> {
    let uid = unsafe { libc::getuid() };
    let mut problems = Vec::new();
    for u in &plan.unload {
        let out = match u {
            Unload::LoginItem(id) => {
                debug_assert!(valid_bundle_id(id), "unvalidated id reached launchctl");
                std::process::Command::new("launchctl")
                    .arg("bootout")
                    .arg(format!("gui/{uid}/{id}"))
                    .output()
            }
            Unload::Agent(path) => {
                std::process::Command::new("launchctl").arg("unload").arg(path).output()
            }
        };
        match out {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
                problems.push((u.label(), if msg.is_empty() { "not loaded".into() } else { msg }));
            }
            Err(e) => problems.push((u.label(), e.to_string())),
        }
    }
    problems
}

#[cfg(not(target_os = "macos"))]
pub fn perform_unload(_plan: &CleanupPlan) -> Vec<(String, String)> {
    Vec::new()
}

/// Turn the plan into one staging manifest, so one `restore` undoes the whole
/// uninstall.
///
/// Every path goes through `clean::plan` and `clean::stage`, which is the only
/// code allowed to move a user's files and the only path with an undo manifest
/// behind it. Nothing here removes anything.
pub fn stage_uninstall(
    plan: &CleanupPlan,
    threads: usize,
    label: &str,
) -> io::Result<crate::Manifest> {
    let mut all: Option<crate::Plan> = None;
    for r in &plan.items {
        let root = crate::blocklist::canon_keep_link(r.path());
        // Scanning each item separately keeps the aggregate honest for a
        // directory and costs nothing for a plist.
        let mut tree = crate::scan(&root, threads.max(1), |_| {})?;
        let private = crate::scan::private_sizes(&tree, &root);
        crate::aggregate::aggregate_with_private(&mut tree, &private);
        if tree.is_empty() {
            continue;
        }
        let one = crate::clean::plan(&tree, &root, &[0]);
        match &mut all {
            Some(a) => a.absorb(one),
            None => all = Some(one),
        }
    }
    let Some(all) = all else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "nothing to stage"));
    };
    crate::clean::stage(&all, label)
}
