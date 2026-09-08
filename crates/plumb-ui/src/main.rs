#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Tauri shell. The `Tree` never crosses the IPC boundary: it lives here
//! behind a `Mutex` and the frontend only ever receives one image, a flat rect
//! list, and small JSON structs.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use plumb_core::layout::{self, Opts, Scope, SizeMode, View};
use plumb_core::clean;
use plumb_core::render::{kind_of, render, ColorMode, Kind, RenderOpts};
use plumb_core::{aggregate, quick_wins, reconcile, scan, volume_of, Flags, NodeId, Tree};
use tauri::ipc::Response;
use tauri::{Emitter, Manager};

struct Loaded {
    tree: Tree,
    root_path: String,
    elapsed_ms: u64,
}

/// Applications and their leftovers, computed once per `apps_list` call.
///
/// The frontend addresses an app by its index here rather than by path, so a
/// detail or uninstall request can never name something the list did not
/// produce.
#[derive(Default)]
struct AppsCache {
    apps: Vec<plumb_core::apps::App>,
    found: Vec<Vec<plumb_core::apps::Associated>>,
}

#[derive(Default)]
struct App {
    loaded: Mutex<Option<Loaded>>,
    apps: Mutex<AppsCache>,
    /// Uninstall plans awaiting confirmation, keyed by the token the review
    /// sheet was handed. The sheet shows one of these and `app_uninstall`
    /// stages that same one, so the two can no longer disagree. Cleared
    /// whenever the apps list is rebuilt, which is what makes a stale sheet
    /// fail loudly instead of acting on a re-sorted index.
    plans: Mutex<std::collections::HashMap<u64, plumb_core::apps::CleanupPlan>>,
    watch: Mutex<Option<Box<dyn plumb_core::watch::Watcher + Send>>>,
}

/// Handles for pending plans. Never reused, so a token from a cleared
/// generation cannot collide with a live one.
fn next_token() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn now_secs() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

fn size_mode(v: u8) -> SizeMode {
    match v {
        1 => SizeMode::Allocated,
        2 => SizeMode::Logical,
        _ => SizeMode::Freeable,
    }
}

fn view_of(v: u8) -> View {
    match v {
        1 => View::Folders,
        2 => View::Sunburst,
        3 => View::Flame,
        4 => View::Bubbles,
        5 => View::MindMap,
        6 => View::TopSizes,
        7 => View::AgeMap,
        _ => View::Treemap,
    }
}

fn color_mode(v: u8) -> ColorMode {
    match v {
        1 => ColorMode::ByFolder,
        2 => ColorMode::ByAge,
        _ => ColorMode::ByType,
    }
}

fn scope_of(v: u8) -> Scope {
    match v {
        1 => Scope::FilesAnywhere,
        2 => Scope::FoldersAnywhere,
        _ => Scope::Here,
    }
}

// ----------------------------------------------------------------- targets

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[derive(Serialize)]
struct Target {
    label: String,
    path: String,
}

#[tauri::command]
fn targets() -> Vec<Target> {
    let h = home();
    let mut out = Vec::new();
    let mut add = |label: &str, p: PathBuf| {
        if p.is_dir() {
            out.push(Target { label: label.to_string(), path: p.display().to_string() });
        }
    };
    add("Home", h.clone());
    for name in ["Downloads", "Documents", "Desktop", "Movies", "Music", "Pictures"] {
        add(name, h.join(name));
    }
    add("Caches", h.join("Library").join("Caches"));
    add("Applications", PathBuf::from("/Applications"));
    out
}

// ---------------------------------------------------------------- overview

#[derive(Serialize)]
struct WinOut {
    label: String,
    id: NodeId,
    bytes: u64,
    items: u32,
    path: String,
}

#[derive(Serialize)]
struct TypeSlice {
    label: String,
    css: String,
    bytes: u64,
}

#[derive(Serialize)]
struct Overview {
    root: NodeId,
    path: String,
    files: u32,
    dirs: u32,
    logical: u64,
    allocated: u64,
    freeable: u64,
    volume_total: u64,
    volume_free: u64,
    volume_used: u64,
    unaccounted: u64,
    denied: u32,
    shared: u32,
    elapsed_ms: u64,
    wins: Vec<WinOut>,
    types: Vec<TypeSlice>,
}

fn build_overview(l: &Loaded, size: SizeMode) -> Overview {
    let t = &l.tree;
    let mut types = vec![0u64; Kind::ALL.len()];
    let mut denied = 0u32;
    let mut shared = 0u32;
    for i in 0..t.len() {
        let id = i as NodeId;
        if t.flags[i].has(Flags::DENIED) {
            denied += 1;
        }
        if t.flags[i].has(Flags::SHARED) {
            shared += 1;
        }
        if t.flags[i].is_dir() {
            continue;
        }
        let bytes = layout::size_of(t, id, size);
        let k = kind_of(t.name(id));
        types[Kind::ALL.iter().position(|&x| x == k).unwrap_or(6)] += bytes;
    }

    let vol = volume_of(Path::new(&l.root_path)).ok();
    let (vt, vf, vu, un) = match &vol {
        Some(v) => {
            let r = reconcile(t, v);
            (v.total, v.free, v.used, r.unaccounted)
        }
        None => (0, 0, 0, 0),
    };

    Overview {
        root: 0,
        path: l.root_path.clone(),
        files: t.sub_files[0],
        dirs: t.sub_dirs[0],
        logical: t.sub_logical[0],
        allocated: t.sub_blocks[0],
        freeable: t.sub_excl[0],
        volume_total: vt,
        volume_free: vf,
        volume_used: vu,
        unaccounted: un,
        denied,
        shared,
        elapsed_ms: l.elapsed_ms,
        wins: quick_wins(t)
            .into_iter()
            .take(8)
            .map(|w| WinOut {
                label: w.label.to_string(),
                id: w.id,
                bytes: w.bytes,
                items: w.items,
                path: t.path(w.id),
            })
            .collect(),
        types: Kind::ALL
            .iter()
            .enumerate()
            .map(|(i, k)| TypeSlice { label: k.label().to_string(), css: k.css(), bytes: types[i] })
            .collect(),
    }
}

#[tauri::command]
async fn scan_dir(path: String, size: u8, app: tauri::AppHandle) -> Result<Overview, String> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("{path} is not a folder"));
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let started = Instant::now();
    let p2 = p.clone();
    let emitter = app.clone();
    let tree = tauri::async_runtime::spawn_blocking(move || {
        // Progress is integers only; payloads never travel by event.
        let mut t = scan(&p2, threads, |seen| {
            let _ = emitter.emit("scan_progress", seen);
        })?;
        aggregate(&mut t);
        Ok::<Tree, std::io::Error>(t)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    if tree.is_empty() {
        return Err(format!("nothing readable under {path}"));
    }
    let loaded = Loaded {
        tree,
        root_path: p.display().to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    let ov = build_overview(&loaded, size_mode(size));
    *app.state::<App>().loaded.lock().unwrap() = Some(loaded);
    Ok(ov)
}

#[tauri::command]
fn overview(size: u8, state: tauri::State<'_, App>) -> Result<Overview, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    Ok(build_overview(l, size_mode(size)))
}

// ------------------------------------------------------------------ render

#[derive(Deserialize)]
struct ViewReq {
    node: NodeId,
    view: u8,
    levels: u16,
    w: f32,
    h: f32,
    dpr: f32,
    color: u8,
    size: u8,
    scope: u8,
    filter: String,
}

/// One binary frame: header, rect list, then RGBA pixels. Never the event
/// system, which is documented as JSON `eval`.
#[tauri::command]
fn render_view(req: ViewReq, state: tauri::State<'_, App>) -> Response {
    let g = state.loaded.lock().unwrap();
    let Some(l) = g.as_ref() else { return Response::new(Vec::new()) };
    let t = &l.tree;
    let node = if (req.node as usize) < t.len() { req.node } else { 0 };

    let view = view_of(req.view);
    let o = RenderOpts {
        view,
        color: color_mode(req.color),
        dpr: req.dpr.clamp(1.0, 4.0),
        filter: req.filter,
        lay: Opts {
            w: req.w.max(4.0),
            h: req.h.max(4.0),
            levels: req.levels.clamp(1, 12),
            size: size_mode(req.size),
            scope: scope_of(req.scope),
            now: now_secs(),
        },
    };

    let started = Instant::now();
    let (pixels, rects) = render(t, node, &o);
    let (pw, ph) = plumb_core::render::px_size(&o);
    eprintln!(
        "render {view:?} node={node} rects={} in {} ms",
        rects.len(),
        started.elapsed().as_millis()
    );
    debug_assert!(rects.len() <= layout::MAX_RECTS);

    let geom: u32 = match view.geom() {
        layout::Geom::Cartesian => 0,
        layout::Geom::Polar => 1,
        layout::Geom::Circle => 2,
    };

    let mut buf = Vec::with_capacity(16 + rects.len() * 24 + pixels.len());
    buf.extend_from_slice(&pw.to_le_bytes());
    buf.extend_from_slice(&ph.to_le_bytes());
    buf.extend_from_slice(&(rects.len() as u32).to_le_bytes());
    buf.extend_from_slice(&geom.to_le_bytes());
    for r in &rects {
        buf.extend_from_slice(&r.x.to_le_bytes());
        buf.extend_from_slice(&r.y.to_le_bytes());
        buf.extend_from_slice(&r.w.to_le_bytes());
        buf.extend_from_slice(&r.h.to_le_bytes());
        buf.extend_from_slice(&r.id.to_le_bytes());
        buf.extend_from_slice(&(r.depth as u32).to_le_bytes());
    }
    buf.extend_from_slice(&pixels);
    Response::new(buf)
}

// --------------------------------------------------------------- inspector

#[derive(Serialize)]
struct Child {
    id: NodeId,
    name: String,
    bytes: u64,
    is_dir: bool,
}

#[derive(Serialize)]
struct Info {
    id: NodeId,
    name: String,
    path: String,
    is_dir: bool,
    kind: String,
    bytes: u64,
    logical: u64,
    allocated: u64,
    freeable: u64,
    files: u32,
    dirs: u32,
    mtime: u32,
    pct_scan: f64,
    pct_parent: f64,
    shared: bool,
    denied: bool,
    children: Vec<Child>,
}

#[tauri::command]
fn node_info(id: NodeId, size: u8, state: tauri::State<'_, App>) -> Result<Info, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    let t = &l.tree;
    let i = id as usize;
    if i >= t.len() {
        return Err("no such node".into());
    }
    let m = size_mode(size);
    let bytes = layout::size_of(t, id, m);
    let scan_total = layout::size_of(t, 0, m).max(1);
    let parent = t.parent[i];
    let parent_total = if parent == plumb_core::NO_PARENT {
        scan_total
    } else {
        layout::size_of(t, parent, m).max(1)
    };

    let mut children: Vec<Child> = t
        .children(id)
        .map(|c| Child {
            id: c,
            name: t.name(c).to_string(),
            bytes: layout::size_of(t, c, m),
            is_dir: t.flags[c as usize].is_dir(),
        })
        .collect();
    children.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));
    children.truncate(8);

    Ok(Info {
        id,
        name: t.name(id).to_string(),
        path: t.path(id),
        is_dir: t.flags[i].is_dir(),
        kind: if t.flags[i].is_dir() {
            "folder".to_string()
        } else {
            kind_of(t.name(id)).label().to_string()
        },
        bytes,
        logical: t.sub_logical[i],
        allocated: t.sub_blocks[i],
        freeable: t.sub_excl[i],
        files: t.sub_files[i],
        dirs: t.sub_dirs[i],
        mtime: t.mtime[i],
        pct_scan: bytes as f64 * 100.0 / scan_total as f64,
        pct_parent: bytes as f64 * 100.0 / parent_total as f64,
        shared: t.flags[i].has(Flags::SHARED),
        denied: t.flags[i].has(Flags::DENIED),
        children,
    })
}

#[derive(Serialize)]
struct Crumb {
    id: NodeId,
    name: String,
}

#[tauri::command]
fn breadcrumb(id: NodeId, state: tauri::State<'_, App>) -> Result<Vec<Crumb>, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    let t = &l.tree;
    if id as usize >= t.len() {
        return Err("no such node".into());
    }
    let mut out = Vec::new();
    let mut cur = id;
    loop {
        out.push(Crumb { id: cur, name: t.name(cur).to_string() });
        let p = t.parent[cur as usize];
        if p == plumb_core::NO_PARENT {
            break;
        }
        cur = p;
    }
    out.reverse();
    Ok(out)
}

// ------------------------------------------------------------------ cleanup

#[derive(Serialize)]
struct PlanItem {
    path: String,
    bytes: u64,
}

#[derive(Serialize)]
struct PlanOut {
    items: Vec<PlanItem>,
    refused: Vec<(String, String)>,
    total_bytes: u64,
}

fn plan_for(l: &Loaded, ids: &[NodeId]) -> clean::Plan {
    clean::plan(&l.tree, Path::new(&l.root_path), ids)
}

/// Dry run. The UI always calls this before it offers to stage anything.
#[tauri::command]
fn cleanup_plan(ids: Vec<NodeId>, state: tauri::State<'_, App>) -> Result<PlanOut, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    let p = plan_for(l, &ids);
    Ok(PlanOut {
        // Not zipped with `ids`: plan() drops covered and refused entries, so
        // the two lists are different lengths and pairing them would mislabel.
        items: p
            .staged
            .iter()
            .map(|i| PlanItem { path: i.original.display().to_string(), bytes: i.bytes })
            .collect(),
        refused: p
            .refused
            .iter()
            .map(|(path, why)| (path.display().to_string(), why.to_string()))
            .collect(),
        total_bytes: p.total_bytes,
    })
}

#[derive(Serialize)]
struct StageOut {
    manifest: u64,
    moved: usize,
    skipped: usize,
    total_bytes: u64,
    refused: Vec<(String, String)>,
}

#[tauri::command]
fn cleanup_stage(
    ids: Vec<NodeId>,
    label: String,
    state: tauri::State<'_, App>,
) -> Result<StageOut, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    let p = plan_for(l, &ids);
    let planned = p.staged.len();
    let refused = p
        .refused
        .iter()
        .map(|(path, why)| (path.display().to_string(), why.to_string()))
        .collect();
    let m = clean::stage(&p, &label).map_err(|e| e.to_string())?;
    Ok(StageOut {
        manifest: m.id,
        moved: m.items.len(),
        skipped: planned - m.items.len(),
        total_bytes: m.total_bytes,
        refused,
    })
}

#[derive(Serialize)]
struct ManifestOut {
    id: u64,
    label: String,
    items: usize,
    total_bytes: u64,
    expires_in_days: u64,
    expired: bool,
}

#[tauri::command]
fn cleanup_list() -> Result<Vec<ManifestOut>, String> {
    let now = clean::now_secs();
    let list = clean::list_staged().map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .map(|m| ManifestOut {
            expires_in_days: m.expires_at.saturating_sub(now) / 86_400,
            expired: m.is_expired(now),
            id: m.id,
            label: m.label,
            items: m.items.len(),
            total_bytes: m.total_bytes,
        })
        .collect())
}

#[tauri::command]
fn cleanup_restore(id: u64) -> Result<usize, String> {
    clean::restore(id).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct CommitOut {
    freed: u64,
    skipped: Vec<(String, String)>,
}

#[tauri::command]
fn cleanup_commit(id: u64) -> Result<CommitOut, String> {
    let r = clean::commit(id).map_err(|e| e.to_string())?;
    Ok(CommitOut {
        freed: r.freed,
        skipped: r.skipped.iter().map(|(p, w)| (p.display().to_string(), w.clone())).collect(),
    })
}

// ---------------------------------------------------------------- stage 5

#[derive(Serialize)]
struct SnapOut {
    path: String,
    name: String,
    bytes: u64,
    modified: u64,
}

#[tauri::command]
fn snapshot_save(state: tauri::State<'_, App>) -> Result<SnapOut, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    let stem = Path::new(&l.root_path)
        .file_name()
        .map(|s| s.to_string_lossy().replace(['/', ' '], "_"))
        .unwrap_or_else(|| "root".into());
    let out = plumb_core::snapshot::snapshots_dir()
        .map_err(|e| e.to_string())?
        .join(format!("{stem}-{}.plumbsnap", clean::now_secs()));
    plumb_core::snapshot::save(&l.tree, &out).map_err(|e| e.to_string())?;
    let m = std::fs::metadata(&out).map_err(|e| e.to_string())?;
    Ok(SnapOut {
        name: out.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        path: out.display().to_string(),
        bytes: m.len(),
        modified: clean::now_secs(),
    })
}

#[tauri::command]
fn snapshot_list() -> Result<Vec<SnapOut>, String> {
    Ok(plumb_core::snapshot::list()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|e| SnapOut {
            name: e.path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            path: e.path.display().to_string(),
            bytes: e.bytes,
            modified: e.modified,
        })
        .collect())
}

#[derive(Serialize)]
struct DiffOut {
    kind: &'static str,
    path: String,
    is_dir: bool,
    old: u64,
    new: u64,
    delta: i64,
}

#[tauri::command]
fn snapshot_diff(old: String, new: String) -> Result<Vec<DiffOut>, String> {
    let a = plumb_core::snapshot::Snapshot::open(Path::new(&old)).map_err(|e| e.to_string())?;
    let b = plumb_core::snapshot::Snapshot::open(Path::new(&new)).map_err(|e| e.to_string())?;
    Ok(plumb_core::diff::diff(a.tree(), b.tree())
        .into_iter()
        .take(500)
        .map(|c| {
            let delta = c.delta().clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            DiffOut { kind: c.kind.label(), path: c.path, is_dir: c.is_dir, old: c.old, new: c.new, delta }
        })
        .collect())
}

#[derive(Serialize)]
struct DupeMemberOut {
    path: String,
    shared: bool,
}

#[derive(Serialize)]
struct DupeGroupOut {
    bytes_each: u64,
    reclaimable: u64,
    copies: usize,
    members: Vec<DupeMemberOut>,
}

#[derive(Serialize)]
struct DupesOut {
    groups: Vec<DupeGroupOut>,
    total_reclaimable: u64,
    /// False when this mount cannot share extents, so the UI hides the action
    /// rather than offering something that will fail.
    reflink_supported: bool,
}

fn scan_dupes(l: &Loaded, min_size: u64) -> Vec<plumb_core::dupes::Group> {
    plumb_core::dupes::find(&l.tree, Path::new(&l.root_path), min_size)
}

#[tauri::command]
fn dupes_find(min_size: u64, state: tauri::State<'_, App>) -> Result<DupesOut, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    let groups = scan_dupes(l, min_size);
    Ok(DupesOut {
        total_reclaimable: plumb_core::dupes::total_reclaimable(&groups),
        reflink_supported: plumb_core::reflink::supported(Path::new(&l.root_path)),
        groups: groups
            .into_iter()
            .take(200)
            .map(|g| DupeGroupOut {
                bytes_each: g.bytes_each,
                reclaimable: g.reclaimable,
                copies: g.copies,
                members: g
                    .members
                    .into_iter()
                    .map(|m| DupeMemberOut {
                        path: m.path.display().to_string(),
                        shared: m.shared,
                    })
                    .collect(),
            })
            .collect(),
    })
}

#[derive(Serialize)]
struct DedupeOut {
    freed: u64,
    done: usize,
    refused: Vec<(String, String)>,
    dry_run: bool,
}

/// Always reachable as a dry run; the UI calls it that way first.
#[tauri::command]
fn dupes_dedupe(
    min_size: u64,
    dry_run: bool,
    state: tauri::State<'_, App>,
) -> Result<DedupeOut, String> {
    let g = state.loaded.lock().unwrap();
    let l = g.as_ref().ok_or("nothing scanned yet")?;
    if !plumb_core::reflink::supported(Path::new(&l.root_path)) {
        return Err("this mount does not support reflinks".into());
    }
    let groups = scan_dupes(l, min_size);
    let r = plumb_core::reflink::dedupe_groups(&groups, dry_run);
    Ok(DedupeOut {
        freed: r.freed,
        done: r.done.len(),
        refused: r.refused.iter().map(|f| (f.path.display().to_string(), f.why.clone())).collect(),
        dry_run,
    })
}

// ------------------------------------------------------------- stage 6

#[derive(Serialize)]
struct AppOut {
    idx: usize,
    name: String,
    bundle_id: Option<String>,
    version: Option<String>,
    path: String,
    /// The `.app` itself.
    bundle_bytes: u64,
    /// Everything else we can attribute to it, removable or not.
    support_bytes: u64,
    leftovers: usize,
    /// Another installed app claims the same bundle id.
    contested: bool,
}

#[derive(Serialize)]
struct AssocOut {
    path: String,
    bytes: u64,
    category: String,
    evidence: String,
    /// False for system-level paths and for a sibling's shared data. The UI
    /// greys these and says why; they cannot reach a plan.
    removable: bool,
    why: Option<String>,
}

#[derive(Serialize)]
struct AppDetail {
    app: AppOut,
    items: Vec<AssocOut>,
    /// Background jobs that would be stopped before anything moved.
    unload: Vec<String>,
    stage_bytes: u64,
    /// Handle for the plan behind this panel. The review sheet renders the
    /// two lists below and `app_uninstall` stages this exact plan.
    token: u64,
    /// Exactly what will be staged.
    staged: Vec<AssocOut>,
    /// Everything discovered and deliberately left, with the reason.
    excluded: Vec<AssocOut>,
}

fn app_out(
    idx: usize,
    a: &plumb_core::apps::App,
    found: &[plumb_core::apps::Associated],
    contested: bool,
) -> AppOut {
    let bundle_bytes = found
        .iter()
        .filter(|i| i.category == plumb_core::apps::Category::Bundle)
        .map(|i| i.bytes)
        .sum::<u64>()
        .max(a.bundle_bytes);
    let support_bytes = found
        .iter()
        .filter(|i| i.category != plumb_core::apps::Category::Bundle)
        .map(|i| i.bytes)
        .sum();
    AppOut {
        idx,
        name: a.name.clone(),
        bundle_id: a.bundle_id.clone(),
        version: a.version.clone(),
        path: a.path.display().to_string(),
        bundle_bytes,
        support_bytes,
        leftovers: found.iter().filter(|i| i.category != plumb_core::apps::Category::Bundle).count(),
        contested,
    }
}

#[tauri::command]
fn apps_list(guesses: bool, state: tauri::State<'_, App>) -> Result<Vec<AppOut>, String> {
    use plumb_core::apps::{self, LeftoverOpts};
    let apps = apps::list_apps().map_err(|e| e.to_string())?;
    let opts = LeftoverOpts { include_name_matches: guesses };
    // The sibling guard excludes an app by slot, so every app gets its own
    // "everybody else" list rather than one shared contested set.
    let found: Vec<Vec<apps::Associated>> = apps
        .iter()
        .enumerate()
        .map(|(i, a)| apps::associated_for(a, &apps::other_ids(&apps, i), opts))
        .collect();

    let out = apps
        .iter()
        .enumerate()
        .map(|(i, a)| app_out(i, a, &found[i], !apps::contesting_ids(&apps, i).is_empty()))
        .collect();
    *state.apps.lock().unwrap() = AppsCache { apps, found };
    // Every outstanding review described the old list. Drop them all.
    state.plans.lock().unwrap().clear();
    Ok(out)
}

fn assoc_out(i: &plumb_core::apps::Associated) -> AssocOut {
    let why = plumb_core::apps::exclusion_reason(i);
    AssocOut {
        path: i.path.display().to_string(),
        bytes: i.bytes,
        category: i.category.label().to_string(),
        evidence: i.evidence.label().to_string(),
        removable: why.is_none(),
        why: why.map(|w| w.to_string()),
    }
}

/// Build the plan once, keep it, and hand back a token for it.
///
/// Nothing here rescans. The plan comes from the same associations the list
/// was built from, so the panel, the sheet and the staging run are all one
/// scan - previously these were three, with three different option sets, and
/// the sheet could promise items that would not stage while staging items it
/// had never shown.
#[tauri::command]
fn app_detail(idx: usize, state: tauri::State<'_, App>) -> Result<AppDetail, String> {
    use plumb_core::apps;
    let cache = state.apps.lock().unwrap();
    let a = cache.apps.get(idx).ok_or("no such application")?;
    let found = &cache.found[idx];

    let plan = apps::plan_from(a, found);
    let staged: Vec<AssocOut> = plan.items.iter().map(|r| assoc_out(r.item())).collect();
    let excluded: Vec<AssocOut> =
        found.iter().filter(|i| apps::exclusion_reason(i).is_some()).map(assoc_out).collect();

    // The plan recomputed each stageable item's size honestly (hardlink- and
    // clone-aware). Carry those numbers into the inventory list too, so every
    // figure on this screen comes from one accounting rather than two.
    let honest: std::collections::HashMap<&std::path::Path, u64> =
        plan.items.iter().map(|r| (r.path(), r.item().bytes)).collect();

    let detail = AppDetail {
        app: app_out(idx, a, found, !apps::contesting_ids(&cache.apps, idx).is_empty()),
        items: found
            .iter()
            .map(|i| {
                let mut o = assoc_out(i);
                if let Some(&b) = honest.get(i.path.as_path()) {
                    o.bytes = b;
                }
                o
            })
            .collect(),
        unload: plan.unload.iter().map(|u| u.label()).collect(),
        stage_bytes: plan.bytes,
        token: next_token(),
        staged,
        excluded,
    };
    state.plans.lock().unwrap().insert(detail.token, plan);
    Ok(detail)
}

#[derive(Serialize)]
struct UninstallOut {
    manifest: u64,
    count: usize,
    bytes: u64,
    /// Helpers that would not stop. Reported, never fatal: one that was not
    /// running is the ordinary case.
    unload_problems: Vec<(String, String)>,
    /// Paths the planner declined, with the reason it gave.
    refused: Vec<(String, String)>,
}

/// Stop the app's background jobs, then route every path through staging.
/// Nothing is deleted; `cleanup_commit` remains the only thing that removes.
///
/// Addressed by token, never by list index. An index is a moving target: the
/// list is name-sorted, and refreshing it while a detail panel is open could
/// shift the slot and uninstall a different application.
#[tauri::command]
fn app_uninstall(token: u64, state: tauri::State<'_, App>) -> Result<UninstallOut, String> {
    use plumb_core::apps;
    // Take the plan the review sheet was built from. Gone means the list was
    // rebuilt underneath it, and this request is about a world that no longer
    // exists.
    let plan = state
        .plans
        .lock()
        .unwrap()
        .remove(&token)
        .ok_or("this review is out of date - reopen the application and try again")?;
    if plan.is_empty() {
        return Err("nothing to stage".into());
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    // Plan before unloading: a refused bundle must abort while the app is
    // still running and still installed, not after its helpers are stopped.
    let prepared = apps::prepare_uninstall(&plan, threads).map_err(|e| e.to_string())?;
    let refused: Vec<(String, String)> =
        prepared.refused.iter().map(|(p, w)| (p.display().to_string(), w.to_string())).collect();

    let unload_problems = apps::perform_unload(&plan);
    let label = format!("uninstall {}", plan.app);
    let m = apps::stage_prepared(&prepared, &label).map_err(|e| e.to_string())?;
    // Anything that changed between planning and moving is named, not counted.
    let mut refused = refused;
    refused.extend(
        apps::changed_since_plan(&prepared, &m)
            .into_iter()
            .map(|(p, w)| (p.display().to_string(), w.to_string())),
    );
    Ok(UninstallOut {
        manifest: m.id,
        count: m.items.len(),
        bytes: m.total_bytes,
        unload_problems,
        refused,
    })
}

#[derive(Serialize)]
struct EventOut {
    path: String,
    kind: &'static str,
    bytes: i64,
    at: u64,
}

#[tauri::command]
fn watch_start(path: String, state: tauri::State<'_, App>) -> Result<(), String> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let w = plumb_core::watch::watcher(Path::new(&path), threads).map_err(|e| e.to_string())?;
    *state.watch.lock().unwrap() = Some(w);
    Ok(())
}

#[tauri::command]
fn watch_stop(state: tauri::State<'_, App>) {
    *state.watch.lock().unwrap() = None;
}

/// One drain, already coalesced to at most one entry per path. The frontend
/// renders per drain, never per event.
#[tauri::command]
fn watch_poll(state: tauri::State<'_, App>) -> Result<Vec<EventOut>, String> {
    let mut guard = state.watch.lock().unwrap();
    let Some(w) = guard.as_mut() else { return Ok(Vec::new()) };
    let evs = w.events().map_err(|e| e.to_string())?;
    Ok(evs
        .into_iter()
        .map(|e| EventOut {
            path: e.path.display().to_string(),
            kind: e.kind.label(),
            bytes: e.bytes,
            at: e.at,
        })
        .collect())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![
            targets,
            scan_dir,
            overview,
            render_view,
            node_info,
            breadcrumb,
            cleanup_plan,
            cleanup_stage,
            cleanup_list,
            cleanup_restore,
            cleanup_commit,
            snapshot_save,
            snapshot_list,
            snapshot_diff,
            dupes_find,
            dupes_dedupe,
            apps_list,
            app_detail,
            app_uninstall,
            watch_start,
            watch_stop,
            watch_poll
        ])
        .run(tauri::generate_context!())
        .expect("failed to start window");
}
