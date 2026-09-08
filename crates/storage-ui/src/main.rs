#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Tauri shell. The `Tree` never crosses the IPC boundary: it lives here
//! behind a `Mutex` and the frontend only ever receives one image, a flat rect
//! list, and small JSON structs.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use storage_core::layout::{self, Opts, Scope, SizeMode, View};
use storage_core::clean;
use storage_core::render::{kind_of, render, ColorMode, Kind, RenderOpts};
use storage_core::{aggregate, quick_wins, reconcile, scan, volume_of, Flags, NodeId, Tree};
use tauri::ipc::Response;
use tauri::{Emitter, Manager};

struct Loaded {
    tree: Tree,
    root_path: String,
    elapsed_ms: u64,
}

#[derive(Default)]
struct App {
    loaded: Mutex<Option<Loaded>>,
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
    let (pw, ph) = storage_core::render::px_size(&o);
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
    let parent_total = if parent == storage_core::NO_PARENT {
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
        if p == storage_core::NO_PARENT {
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
            cleanup_commit
        ])
        .run(tauri::generate_context!())
        .expect("failed to start window");
}
