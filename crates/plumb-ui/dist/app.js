const invoke = window.__TAURI__.core.invoke;

const VIEWS = [
  "Treemap", "Folders", "Sunburst", "Flame",
  "Bubbles", "Mind Map", "Top Sizes", "Age Map",
];

const S = {
  node: 0,
  view: 0,
  levels: 4,
  color: 0,
  size: 0,
  scope: 0,
  filter: "",
  scanned: false,
  rects: [],
  geom: 0,
  hover: -1,
  // Selected node id, NOT a rect index: indices are invalidated by every
  // relayout, but the selection must survive resize, depth and view changes.
  sel: -1,
  css: { w: 0, h: 0 },
};

// Storage panels, one visible at a time inside #center.
const PANELS = ["map", "wins", "types", "dupes", "snaps", "watch", "cleanup"];
let panel = "map";

const $ = (id) => document.getElementById(id);
const image = $("image");
const overlay = $("overlay");
const ictx = image.getContext("2d");
const octx = overlay.getContext("2d");

function mid(s, n) {
  return s.length <= n ? s : s.slice(0, n / 2 - 1) + "\u2026" + s.slice(-(n / 2));
}

function human(b) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let v = Number(b), i = 0;
  while (v >= 1024 && i < 4) { v /= 1024; i++; }
  if (i === 0) return `${v} B`;
  return `${v >= 100 ? v.toFixed(0) : v.toFixed(1)} ${u[i]}`;
}

function when(secs) {
  if (!secs) return "unknown";
  return new Date(secs * 1000).toISOString().slice(0, 10);
}

// ------------------------------------------------------------------- chrome

// Four views on the row, the rest behind "More". Indices into VIEWS.
const MAIN = [0, 2, 1, 7];
const MORE = [3, 4, 5, 6];

function viewBtn(i) {
  const b = document.createElement("button");
  b.textContent = VIEWS[i];
  b.dataset.view = i;
  b.onclick = () => setView(i);
  return b;
}
MAIN.forEach((i) => $("tabs").append(viewBtn(i)));
MORE.forEach((i) => $("more-pop").append(viewBtn(i)));

function setView(i) {
  S.view = i;
  $("scope-wrap").hidden = i !== 6;
  closePops();
  paintTabs();
  draw();
}

function paintTabs() {
  document.querySelectorAll("[data-view]").forEach((b) => b.classList.toggle("on", +b.dataset.view === S.view));
  const more = MORE.includes(S.view);
  $("more-btn").textContent = `${more ? VIEWS[S.view] : "More"} ▾`;
  $("more-btn").classList.toggle("on", more);
}

function setOpen(el, on) { el.toggleAttribute("data-open", on); }
// Set by ask() (Task 4) so Cancel, Escape and a backdrop click resolve it false.
let sheetCancel = null;
function closeSheet() {
  setOpen($("sheet"), false);
  const c = sheetCancel;
  sheetCancel = null;
  if (c) c();
}
function closePops() { document.querySelectorAll(".pop[data-open]").forEach((p) => setOpen(p, false)); }
function popover(btn, pop, onOpen) {
  btn.onclick = (e) => {
    e.stopPropagation();
    const open = !pop.hasAttribute("data-open");
    closePops();
    setOpen(pop, open);
    if (open && onOpen) onOpen();
  };
  pop.onclick = (e) => e.stopPropagation();
}
popover($("more-btn"), $("more-pop"));
popover($("display-btn"), $("display-pop"), () => tipOnce("size"));
document.addEventListener("click", closePops);

const onMap = () => M.mode === "storage" && panel === "map" && S.scanned && !scanning;

function showPanel(name) {
  panel = name;
  closePops();
  for (const p of PANELS) $("panel-" + p).hidden = p !== name;
  $("inspector").hidden = name !== "map";
  document.querySelectorAll("#rail [data-panel]").forEach((b) => b.classList.toggle("on", b.dataset.panel === name));
  document.querySelectorAll("#targets button").forEach((b) =>
    b.classList.toggle("on", name === "map" && S.scanned && b.title === $("path").value));
  if (name === "cleanup") paintPending();
}
document.querySelectorAll("#rail [data-panel]").forEach((b) => { b.onclick = () => showPanel(b.dataset.panel); });

// Inside #panel-map: welcome before any scan, progress while scanning, map after.
function paintHome() {
  $("welcome").hidden = S.scanned || scanning;
  $("scanning").hidden = !scanning;
  $("mapview").hidden = !S.scanned || scanning;
  document.querySelectorAll("[data-needs-scan]").forEach((b) => {
    b.disabled = !S.scanned || scanning;
    b.title = b.disabled ? "Scan a folder first" : "";
  });
}

$("color").onchange = (e) => { S.color = +e.target.value; draw(); };
$("size").onchange = (e) => { S.size = +e.target.value; refreshOverview(); draw(); };
$("scope").onchange = (e) => { S.scope = +e.target.value; draw(); };
$("depth").oninput = (e) => {
  S.levels = +e.target.value;
  $("depth-val").textContent = S.levels;
  draw();
};
$("filter").oninput = (e) => { S.filter = e.target.value; draw(); };
$("go").onclick = () => startScan($("path").value.trim());
$("scan-cancel").onclick = () => invoke("scan_cancel");
$("pick").onclick = async () => {
  // Invoked as a plugin command so the page needs no bundled JS binding.
  const dir = await invoke("plugin:dialog|open", {
    options: { directory: true, multiple: false, title: "Choose a folder to scan" },
  });
  const path = Array.isArray(dir) ? dir[0] : dir;
  if (path) startScan(typeof path === "string" ? path : path.path);
};
$("path").onkeydown = (e) => { if (e.key === "Enter") $("go").click(); };

let homePath = "";
invoke("targets").then((list) => {
  for (const t of list) {
    if (t.label === "Home") homePath = t.path;
    const b = document.createElement("button");
    b.textContent = t.label;
    b.title = t.path;
    b.onclick = () => startScan(t.path);
    $("targets").append(b);
  }
});

// ------------------------------------------------------------------ welcome

invoke("volume_info", { path: null }).then((v) => {
  const total = Math.max(1, Number(v.total));
  $("w-used").style.width = `${(Number(v.used) / total) * 100}%`;
  $("w-meta").textContent = `${human(v.used)} used · ${human(v.free)} free of ${human(v.total)}`;
}).catch(() => { $("w-bar").hidden = true; });

$("w-home").onclick = () => startScan(homePath);
$("w-disk").onclick = () => startScan(navigator.platform.startsWith("Win") ? "C:\\" : "/");

// Tauri delivers OS drops as window events with real paths; the DOM drop
// event never sees a filesystem path in a webview.
const tev = window.__TAURI__.event;
tev.listen("tauri://drag-enter", () => { if (M.mode === "storage") document.body.classList.add("dropping"); });
tev.listen("tauri://drag-leave", () => document.body.classList.remove("dropping"));
tev.listen("tauri://drag-drop", (e) => {
  document.body.classList.remove("dropping");
  const p = e.payload?.paths?.[0];
  if (p && M.mode === "storage") startScan(p);
});

// One short callout the first time each idea shows up; replaces the old tour.
function tipOnce(name) {
  const el = document.querySelector(`[data-tip="${name}"]`);
  let seen = false;
  try { seen = localStorage.getItem("tip." + name) === "1"; } catch {}
  if (!el || seen) return;
  el.hidden = false;
  el.querySelector("button").onclick = () => {
    el.hidden = true;
    try { localStorage.setItem("tip." + name, "1"); } catch {}
  };
}

// -------------------------------------------------------------------- scan

$("excludes").value = localStorage.excludes || "";
$("excludes").onchange = (e) => { localStorage.excludes = e.target.value; };
let scanning = false;
$("fda-open").onclick = () => invoke("open_privacy_settings");
$("fda-x").onclick = () => { localStorage.fdaDismissed = "1"; $("fda").hidden = true; };

// Everything derived from a tree: node ids in it mean nothing after a rescan
// or a stage, so all of it goes together.
function resetDerived() {
  $("dupe-total").textContent = "";
  $("dupe-actions").hidden = true;
  $("dupe-note").textContent = "";
  $("dupe-title").textContent = "";
  $("dupe-list").replaceChildren();
  $("diff-title").textContent = "";
  $("diff-list").replaceChildren();
  paintSnaps();
}

async function startScan(path) {
  if (!path || scanning) return;
  let unlisten = null;
  scanning = true;
  $("scan-path").textContent = path;
  $("scan-count").textContent = "Starting…";
  $("w-error").hidden = true;
  showPanel("map");
  paintHome();
  try {
    unlisten = await window.__TAURI__.event.listen("scan_progress", (e) => {
      $("scan-count").textContent = `${Number(e.payload).toLocaleString()} items`;
    });
    const ov = await invoke("scan_dir", { path, size: S.size, excludes: $("excludes").value.split("\n") });
    S.scanned = true;
    S.node = 0;
    S.sel = -1;
    inspectId = -1;
    $("insp").hidden = true;
    $("insp-empty").hidden = false;
    tray.clear();
    paintTray();
    resetDerived();
    $("path").value = path;
    applyOverview(ov);
    await crumbs();
  } catch (e) {
    if (S.scanned) toast(String(e));
    else { $("w-error").textContent = String(e); $("w-error").hidden = false; }
  } finally {
    if (unlisten) unlisten();
    scanning = false;
    showPanel("map");
    paintHome();
    if (S.scanned) await draw();
  }
}

async function refreshOverview() {
  if (!S.scanned) return;
  applyOverview(await invoke("overview", { size: S.size }));
}

const SIZE_WORDS = ["you could free", "on disk", "in file sizes"];

function applyOverview(ov) {
  $("status").textContent = `Scanned in ${(ov.elapsed_ms / 1000).toFixed(1)} s`;

  const shown = [ov.freeable, ov.allocated, ov.logical][S.size];
  $("title-name").textContent = ov.path.split(/[\\/]/).filter(Boolean).pop() || ov.path;
  $("title-sub").textContent =
    `${human(shown)} ${SIZE_WORDS[S.size]} · ${ov.files.toLocaleString()} files · ${ov.dirs.toLocaleString()} folders` +
    (ov.denied ? ` · ${ov.denied} unreadable` : "") +
    (ov.excluded ? ` · ${ov.excluded} skipped` : "");

  const mac = navigator.platform.startsWith("Mac");
  $("fda").hidden = !(ov.denied > 0 && !localStorage.fdaDismissed);
  $("fda-text").textContent = mac
    ? `macOS blocked ${ov.denied} folders. Grant Full Disk Access to see everything.`
    : `${ov.denied} folders were unreadable.`;
  $("fda-open").hidden = !mac;

  const total = Math.max(1, Number(ov.volume_total));
  const scanned = Math.min(Number(ov.allocated), total);
  const other = Math.max(0, Number(ov.volume_used) - scanned);
  $("disk-scan").style.width = `${(scanned / total) * 100}%`;
  $("disk-other").style.width = `${(other / total) * 100}%`;
  $("disk-meta").textContent =
    `This folder ${human(scanned)} · rest of disk ${human(other)} · free ${human(ov.volume_free)}`;

  $("wins").replaceChildren(...ov.wins.map((w) => {
    const li = document.createElement("li");
    li.className = "click";
    const l = document.createElement("span");
    l.innerHTML = `<b>${w.label}</b><br><small>${w.items.toLocaleString()} files</small>`;
    const r = document.createElement("span");
    r.textContent = human(w.bytes);
    li.append(l, r);
    li.title = w.path;
    li.onclick = () => { showPanel("map"); go(w.id); };
    return li;
  }));

  const sum = ov.types.reduce((n, t) => n + Number(t.bytes), 0) || 1;
  $("typebar").replaceChildren(...ov.types.map((t) => {
    const d = document.createElement("i");
    d.style.cssText = `background:${t.css};width:${(Number(t.bytes) / sum) * 100}%`;
    d.title = `${t.label} ${human(t.bytes)}`;
    return d;
  }));
  $("typelist").replaceChildren(...ov.types
    .filter((t) => Number(t.bytes) > 0)
    .sort((x, y) => Number(y.bytes) - Number(x.bytes))
    .map((t) => {
      const li = document.createElement("li");
      li.innerHTML =
        `<span><i class="sw" style="background:${t.css}"></i>${t.label}</span>` +
        `<span>${human(t.bytes)}</span>`;
      return li;
    }));
}

// ------------------------------------------------------------------ render

let pending = false;

async function draw() {
  if (!S.scanned || pending) return;
  pending = true;
  const stage = $("stage");
  const w = stage.clientWidth, h = stage.clientHeight;
  const dpr = window.devicePixelRatio || 1;
  S.css = { w, h };

  const buf = await invoke("render_view", {
    req: {
      node: S.node, view: S.view, levels: S.levels,
      w, h, dpr, color: S.color, size: S.size,
      scope: S.scope, filter: S.filter,
    },
  });
  pending = false;
  const u8 = new Uint8Array(buf);
  if (u8.byteLength < 20) return;
  const dv = new DataView(u8.buffer, u8.byteOffset);
  const pw = dv.getUint32(0, true), ph = dv.getUint32(4, true);
  const n = dv.getUint32(8, true);
  S.geom = dv.getUint32(12, true);
  const nl = dv.getUint32(16, true);

  const rects = new Array(n);
  let off = 20;
  for (let i = 0; i < n; i++, off += 24) {
    rects[i] = {
      x: dv.getFloat32(off, true),
      y: dv.getFloat32(off + 4, true),
      w: dv.getFloat32(off + 8, true),
      h: dv.getFloat32(off + 12, true),
      id: dv.getUint32(off + 16, true),
      depth: dv.getUint32(off + 20, true),
    };
  }
  S.rects = rects;

  const dec = new TextDecoder();
  const labels = new Array(nl);
  for (let i = 0; i < nl; i++) {
    const x = dv.getFloat32(off, true), y = dv.getFloat32(off + 4, true);
    const color = `rgb(${u8[off + 8]},${u8[off + 9]},${u8[off + 10]})`;
    const len = dv.getUint16(off + 11, true);
    labels[i] = { x, y, color, text: dec.decode(u8.subarray(off + 13, off + 13 + len)) };
    off += 13 + len;
  }
  S.labels = labels;

  for (const c of [image, overlay]) {
    c.width = pw; c.height = ph;
    c.style.width = w + "px"; c.style.height = h + "px";
  }
  const px = new Uint8ClampedArray(u8.buffer, u8.byteOffset + off, pw * ph * 4);
  ictx.putImageData(new ImageData(px, pw, ph), 0, 0);
  ictx.save();
  ictx.scale(dpr, dpr);
  ictx.font = "12px system-ui, -apple-system, 'Segoe UI', sans-serif";
  ictx.textBaseline = "top";
  for (const l of labels) { ictx.fillStyle = l.color; ictx.fillText(l.text, l.x, l.y); }
  ictx.restore();
  S.hover = -1;
  // paintOverlay() clears the overlay itself, so it must come last here -
  // a trailing clearRect would wipe the selection outline it just drew.
  paintOverlay();
}

// -------------------------------------------------------------- hit testing

function hit(mx, my) {
  const r = S.rects;
  if (S.geom === 1) {
    const cx = S.css.w / 2, cy = S.css.h / 2;
    const dx = mx - cx, dy = my - cy;
    const rad = Math.hypot(dx, dy);
    let ang = Math.atan2(dy, dx);
    if (ang < 0) ang += Math.PI * 2;
    for (let i = r.length - 1; i >= 0; i--) {
      const q = r[i];
      if (rad < q.y || rad > q.y + q.h) continue;
      let a = ang - q.x;
      while (a < 0) a += Math.PI * 2;
      if (a <= q.w) return i;
    }
    return -1;
  }
  if (S.geom === 2) {
    for (let i = r.length - 1; i >= 0; i--) {
      const q = r[i], rr = q.w / 2;
      if (Math.hypot(mx - (q.x + rr), my - (q.y + rr)) <= rr) return i;
    }
    return -1;
  }
  for (let i = r.length - 1; i >= 0; i--) {
    const q = r[i];
    if (mx >= q.x && my >= q.y && mx <= q.x + q.w && my <= q.y + q.h) return i;
  }
  return -1;
}

function traceRect(i) {
  const q = S.rects[i];
  octx.beginPath();
  if (S.geom === 1) {
    const cx = S.css.w / 2, cy = S.css.h / 2;
    octx.arc(cx, cy, q.y + q.h, q.x, q.x + q.w);
    octx.arc(cx, cy, q.y, q.x + q.w, q.x, true);
    octx.closePath();
  } else if (S.geom === 2) {
    const rr = q.w / 2;
    octx.arc(q.x + rr, q.y + rr, rr, 0, Math.PI * 2);
  } else {
    octx.rect(q.x + 1, q.y + 1, Math.max(1, q.w - 2), Math.max(1, q.h - 2));
  }
}

/// Index of the selected node in the current rect list, or -1 if the selection
/// is not on screen (culled, or we navigated elsewhere).
function selIndex() {
  return S.sel < 0 ? -1 : S.rects.findIndex((q) => q.id === S.sel);
}

/// Paints hover and selection together. Selection is drawn last and heavier so
/// it stays legible under the cursor. Selection persists; hover does not.
function paintOverlay() {
  const dpr = window.devicePixelRatio || 1;
  octx.clearRect(0, 0, overlay.width, overlay.height);
  const si = selIndex();
  if (S.hover < 0 && si < 0) return;
  octx.save();
  octx.scale(dpr, dpr);
  if (S.hover >= 0 && S.hover !== si) {
    octx.lineWidth = 2;
    octx.strokeStyle = "rgba(255,255,255,0.9)";
    octx.fillStyle = "rgba(255,255,255,0.12)";
    traceRect(S.hover);
    octx.fill();
    octx.stroke();
  }
  if (si >= 0) {
    octx.lineWidth = 3;
    octx.strokeStyle = "#30d158";
    octx.fillStyle = "rgba(48,209,88,0.18)";
    traceRect(si);
    octx.fill();
    octx.stroke();
  }
  octx.restore();
}

// Kept so existing call sites still work; hover index in, full repaint out.
function highlight(i) {
  S.hover = i;
  paintOverlay();
}

let inspectId = -1;

async function inspect(id) {
  if (id === inspectId) return;
  inspectId = id;
  if (id < 0) return;
  let info;
  try {
    info = await invoke("node_info", { id, size: S.size });
  } catch { return; }
  if (inspectId !== id) return;

  $("insp-empty").hidden = true;
  $("insp").hidden = false;
  tipOnce("inspect");
  $("i-name").textContent = info.name;
  $("i-path").textContent = info.path;
  $("i-size").textContent = human(info.bytes);
  $("i-pct").textContent =
    `${info.pct_scan.toFixed(1)}% of scan · ${info.pct_parent.toFixed(1)}% of parent · ${info.kind}`;

  const rows = [
    ["allocated", human(info.allocated)],
    ["logical", human(info.logical)],
    ["freeable", human(info.freeable)],
    ["files", info.files.toLocaleString()],
    ["folders", info.dirs.toLocaleString()],
    ["modified", when(info.mtime)],
  ];
  if (info.shared) rows.push(["shared", "shares blocks"]);
  if (info.denied) rows.push(["access", "unreadable"]);
  const dl = $("i-details");
  dl.replaceChildren();
  for (const [k, v] of rows) {
    const dt = document.createElement("dt"); dt.textContent = k;
    const dd = document.createElement("dd"); dd.textContent = v;
    dl.append(dt, dd);
  }

  const max = Math.max(1, ...info.children.map((c) => Number(c.bytes)));
  $("i-children").replaceChildren(...info.children.map((c) => {
    const li = document.createElement("li");
    li.innerHTML =
      `<span class="top"><span class="cname">${c.name}</span><span>${human(c.bytes)}</span></span>` +
      `<span class="bar" style="width:${(Number(c.bytes) / max) * 100}%"></span>`;
    if (c.is_dir) li.onclick = () => go(c.id);
    return li;
  }));
}

// ------------------------------------------------------------------- tray

// Node ids the user has queued. Cleared on every new scan, because ids only
// mean anything against the tree they came from.
const tray = new Map();

$("add-cleanup").onclick = () => {
  const id = S.sel >= 0 ? S.sel : inspectId;
  if (id < 0) return;
  tray.set(id, $("i-name").textContent);
  paintTray();
  tipOnce("cleanup");
};
$("tray-clear").onclick = () => { tray.clear(); paintTray(); };

async function paintTray() {
  $("badge").hidden = tray.size === 0; $("badge").textContent = tray.size; $("queue-empty").hidden = tray.size > 0;
  $("tray").replaceChildren(...[...tray].map(([id, name]) => {
    const li = document.createElement("li");
    const n = document.createElement("span");
    n.className = "name";
    n.textContent = name;
    const x = document.createElement("button");
    x.className = "drop";
    x.textContent = "remove";
    x.onclick = () => { tray.delete(id); paintTray(); };
    li.append(n, x);
    return li;
  }));
  if (tray.size === 0) {
    $("tray-total").textContent = "";
    $("tray-refused").textContent = "";
    return;
  }
  // Always a dry run first: the total shown is the one Rust would act on.
  try {
    const p = await invoke("cleanup_plan", { ids: [...tray.keys()] });
    $("tray-total").textContent = `${p.items.length} items, ${human(p.total_bytes)} freeable`;
    $("tray-refused").textContent = p.refused.length
      ? p.refused.map(([path, why]) => `refused ${path.split("/").pop()}: ${why}`).join("\n")
      : "";
  } catch (e) {
    $("tray-total").textContent = String(e);
  }
}

$("stage-btn").onclick = async () => {
  if (tray.size === 0) return;
  $("tray-total").textContent = "staging...";
  try {
    const r = await invoke("cleanup_stage", { ids: [...tray.keys()], label: "cleanup" });
    lastStage = r.manifest;
    toast(`Staged ${human(r.total_bytes)} \u00b7 ${r.moved} items` + (r.skipped ? ` \u00b7 ${r.skipped} changed, left in place` : ""),
          "Undo", () => undoManifest(r.manifest));
    tray.clear();
    paintTray();
    resetDerived();
    $("status").textContent =
      `staged ${r.moved} items, ${human(r.total_bytes)}` +
      (r.skipped ? ` (${r.skipped} changed, skipped)` : "");
    await paintPending();
    inspectId = -1;
    S.sel = -1;
    await refreshOverview();
    await draw();
  } catch (e) {
    $("tray-total").textContent = String(e);
  }
};

let lastStage = 0;      // manifest id \u2318Z undoes, until the next stage or any commit
let toastTimer = 0;
function toast(text, action, fn) {
  clearTimeout(toastTimer);
  $("toast-text").textContent = text;
  const b = $("toast-act");
  b.hidden = !action;
  if (action) { b.textContent = action; b.onclick = () => { setOpen($("toast"), false); fn(); }; }
  setOpen($("toast"), true);
  toastTimer = setTimeout(() => setOpen($("toast"), false), 10000);
}
async function undoManifest(id) {
  if (!id) return;
  lastStage = 0;
  const n = await invoke("cleanup_restore", { id });
  inspectId = -1; S.sel = -1;
  await paintPending(); await refreshOverview(); await draw();
  toast(`Restored ${n} items`);
}
function center(q) {
  if (S.geom === 1) { const a = q.x + q.w / 2, rr = q.y + q.h / 2; return [S.css.w / 2 + Math.cos(a) * rr, S.css.h / 2 + Math.sin(a) * rr]; }
  if (S.geom === 2) return [q.x + q.w / 2, q.y + q.w / 2];
  return [q.x + q.w / 2, q.y + q.h / 2];
}
function nearest(from, dx, dy) {
  const [fx, fy] = center(S.rects[from]);
  let best = -1, bd = Infinity;
  S.rects.forEach((q, i) => {
    if (i === from) return;
    const [cx, cy] = center(q);
    const ax = cx - fx, ay = cy - fy;
    const along = ax * dx + ay * dy, perp = Math.abs(ax * dy - ay * dx);
    if (along <= 0 || perp > along) return;
    const d = along + perp * 2;
    if (d < bd) { bd = d; best = i; }
  });
  return best;
}

let qlOpen = false;
async function quickLook(id) {
  if (id < 0) return;
  try { await invoke("quick_look", { id }); qlOpen = true; return; } catch {}
  const info = await invoke("node_info", { id, size: S.size });
  const buf = info.is_dir ? new ArrayBuffer(0) : await invoke("thumbnail", { id, px: 800 });
  const img = $("ql-img");
  img.hidden = !buf.byteLength;
  if (buf.byteLength) img.src = URL.createObjectURL(new Blob([buf], { type: "image/png" }));
  $("ql-text").textContent = `${info.name} \u00b7 ${mid(info.path, 80)} \u00b7 ${human(info.bytes)}`;
  $("ql").showModal();
}
function quickLookHide() {
  qlOpen = false;
  invoke("quick_look", { id: null }).catch(() => {});
  if ($("ql").open) $("ql").close();
}
$("ql-btn").onclick = () => quickLook(S.sel >= 0 ? S.sel : inspectId);
$("ql-close").onclick = quickLookHide;
if (!/Mac/i.test(navigator.platform)) $("reveal-btn").textContent = "Reveal";
$("reveal-btn").onclick = () => { const id = S.sel >= 0 ? S.sel : inspectId; if (id >= 0) invoke("reveal", { id }); };

document.addEventListener("keydown", (e) => {
  if (e.target.matches("input, textarea, select")) return;
  if ((e.metaKey || e.ctrlKey) && e.key === "z") { e.preventDefault(); undoManifest(lastStage); }
  if (e.key === "Escape") {
    if ($("sheet").hasAttribute("data-open")) { closeSheet(); return; }
    if (document.querySelector(".pop[data-open]")) { closePops(); return; }
    setOpen($("toast"), false); hideTip(); quickLookHide();
    if (M.mode === "storage" && panel !== "map") showPanel("map");
    return;
  }

  if (!onMap()) return;
  if (e.key === " ") { e.preventDefault(); qlOpen ? quickLookHide() : quickLook(S.sel >= 0 ? S.sel : inspectId); }
  const dirs = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] };
  if (dirs[e.key] && !(e.metaKey || e.ctrlKey)) {
    e.preventDefault();
    const si = selIndex();
    const i = si < 0 ? 0 : nearest(si, ...dirs[e.key]);
    if (i >= 0) { S.sel = S.rects[i].id; inspect(S.sel); paintOverlay(); if (qlOpen) quickLook(S.sel); }
  }
  if (e.key === "Enter" && S.sel >= 0) { invoke("node_info", { id: S.sel, size: S.size }).then((n) => { if (n.is_dir) go(S.sel); }); }
  if (e.key === "Backspace" || ((e.metaKey || e.ctrlKey) && e.key === "ArrowUp")) { e.preventDefault(); if (S.crumbs?.length > 1) go(S.crumbs[S.crumbs.length - 2].id); }
  if ((e.metaKey || e.ctrlKey) && (e.key === "=" || e.key === "+")) { $("depth").value = Math.min(8, S.levels + 1); $("depth").dispatchEvent(new Event("input")); }
  if ((e.metaKey || e.ctrlKey) && e.key === "-") { $("depth").value = Math.max(1, S.levels - 1); $("depth").dispatchEvent(new Event("input")); }
  if (e.key === "Delete" && S.sel >= 0) $("add-cleanup").click();
});

async function paintPending() {
  let list = [];
  try { list = await invoke("cleanup_list"); } catch { return; }
  $("pending-empty").hidden = list.length > 0;
  $("pending").replaceChildren(...list.map((m) => {
    const li = document.createElement("li");
    const n = document.createElement("span");
    n.className = "name";
    n.textContent = `${human(m.total_bytes)} · ${m.items} items · ${m.expired ? "expired" : m.expires_in_days + "d left"}`
      + (m.partial ? " · partially deleted" : "");
    const acts = document.createElement("span");
    acts.className = "acts";
    const undo = document.createElement("button");
    undo.textContent = "Undo";
    undo.onclick = () => undoManifest(m.id);
    const del = document.createElement("button");
    del.className = "danger";
    del.textContent = "Commit";
    del.onclick = async () => {
      if (!confirm(`Permanently delete ${m.items} items (${human(m.total_bytes)})? This cannot be undone.`)) return;
      lastStage = 0;
      const ch = new window.__TAURI__.core.Channel();
      ch.onmessage = (p) => {
        n.textContent = `Deleting · ${p.files.toLocaleString()} files · ${human(p.bytes)} of ${human(m.total_bytes)} · ${mid(p.current, 48)}`;
        $("status").textContent = n.textContent;
      };
      undo.hidden = true;
      del.textContent = "Cancel";
      del.onclick = () => invoke("cleanup_cancel");
      try {
        const r = await invoke("cleanup_commit", { id: m.id, onProgress: ch });
        $("status").textContent = `freed ${human(r.freed)}`;
        toast(`Freed ${human(r.freed)}`);
        if (r.skipped.length) {
          const ul = document.createElement("ul");
          ul.className = "muted tiny";
          for (const [p, why] of r.skipped) { const s = document.createElement("li"); s.textContent = `${mid(p, 60)}: ${why}`; ul.append(s); }
          li.append(ul);
        }
      } catch (e) { $("status").textContent = String(e); }
      await paintPending();
      await refreshOverview();
    };
    acts.append(undo, del);
    li.append(n, acts);
    return li;
  }));
}

// ---------------------------------------------------------------- stage 5

function showResults(which, title, items) { $(which + "-title").textContent = title; $(which + "-list").replaceChildren(...items); }

// --- snapshots ------------------------------------------------------------

$("snap-save").onclick = async () => {
  $("snap-save").textContent = "saving...";
  try {
    await invoke("snapshot_save");
    await paintSnaps();
    toast("Snapshot saved");
  } catch (e) {
    $("diff-title").textContent = String(e);
  }
  $("snap-save").textContent = "Save snapshot";
};

async function paintSnaps() {
  let list = [];
  try { list = await invoke("snapshot_list"); } catch { return; }
  $("snaps").replaceChildren(...list.map((s) => {
    const li = document.createElement("li");
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.value = s.path;
    cb.onchange = onSnapPick;
    const nm = document.createElement("span");
    nm.className = "nm";
    nm.textContent = `${s.name} (${human(s.bytes)})`;
    nm.title = s.path;
    li.append(cb, nm);
    return li;
  }));
}

// Two ticked boxes means "compare these", oldest first.
async function onSnapPick() {
  const picked = [...$("snaps").querySelectorAll("input:checked")];
  if (picked.length < 2) return;
  const [a, b] = picked.slice(-2).map((c) => c.value);
  picked.forEach((c) => { c.checked = false; });
  let rows = [];
  try {
    rows = await invoke("snapshot_diff", { old: a, new: b });
  } catch (e) {
    $("diff-title").textContent = String(e);
    return;
  }
  if (!rows.length) {
    showResults("diff", "No changes between those snapshots", []);
    return;
  }
  let net = 0;
  const items = rows.map((r) => {
    net += r.delta;
    const li = document.createElement("li");
    const cls = r.delta >= 0 ? "plus" : "minus";
    const sign = r.delta >= 0 ? "+" : "-";
    li.innerHTML =
      `<span class="row2"><span class="p"><span class="tag">${r.kind}</span>${r.path}${r.is_dir ? "/" : ""}</span>` +
      `<span class="${cls}">${sign}${human(Math.abs(r.delta))}</span></span>`;
    return li;
  });
  const sign = net >= 0 ? "+" : "-";
  showResults("diff", `${rows.length} change(s), net ${sign}${human(Math.abs(net))}`, items);
}

// --- duplicates -----------------------------------------------------------

let dupeReady = false;

$("dupe-find").onclick = async () => {
  $("dupe-find").textContent = "scanning...";
  try {
    const d = await invoke("dupes_find", { minSize: 0 });
    dupeReady = d.groups.length > 0;
    $("dupe-total").textContent = d.groups.length
      ? `${d.groups.length} group(s), ${human(d.total_reclaimable)} reclaimable`
      : "no duplicates found";
    // Hidden rather than broken where the mount cannot share extents.
    $("dupe-actions").hidden = !(dupeReady && d.reflink_supported);
    $("dupe-note").textContent = !dupeReady
      ? ""
      : d.reflink_supported
        ? "Dedupe shares extents; both copies stay readable."
        : "This volume cannot share extents, so dedupe is unavailable here.";

    const items = [];
    for (const g of d.groups) {
      const head = document.createElement("li");
      head.innerHTML =
        `<span class="row2"><span class="p">${human(g.bytes_each)} each, ${g.members.length} names, ` +
        `${g.copies} physical cop${g.copies === 1 ? "y" : "ies"}</span>` +
        `<span class="${g.reclaimable ? "plus" : ""}">${human(g.reclaimable)} reclaimable</span></span>`;
      items.push(head);
      for (const m of g.members) {
        const li = document.createElement("li");
        li.className = "member";
        li.innerHTML = `<span class="p">${m.shared ? '<span class="shared">[shares blocks]</span> ' : ""}${m.path}</span>`;
        items.push(li);
      }
    }
    showResults("dupe", `Duplicates — ${human(d.total_reclaimable)} reclaimable`, items);
  } catch (e) {
    $("dupe-total").textContent = String(e);
  }
  $("dupe-find").textContent = "Find duplicates";
};

async function runDedupe(dry) {
  try {
    const r = await invoke("dupes_dedupe", { minSize: 0, dryRun: dry });
    const items = r.refused.map(([p, why]) => {
      const li = document.createElement("li");
      li.innerHTML = `<span class="row2"><span class="p"><span class="tag">refused</span>${p}</span><span>${why}</span></span>`;
      return li;
    });
    showResults("dupe",
      `${dry ? "Dry run" : "Deduped"}: ${r.done} file(s), ${human(r.freed)} ${dry ? "would be freed" : "freed"}`,
      items,
    );
    if (!dry) await startScan($("path").value.trim());
  } catch (e) {
    $("dupe-total").textContent = String(e);
  }
}

$("dupe-dry").onclick = () => runDedupe(true);
$("dupe-go").onclick = () => {
  if (!confirm("Replace duplicate copies with shared extents? Both files stay readable and byte-identical.")) return;
  runDedupe(false);
};

// -------------------------------------------------------------- navigation

async function crumbs() {
  const list = await invoke("breadcrumb", { id: S.node });
  S.crumbs = list;
  const el = $("crumbs");
  el.replaceChildren();
  list.forEach((c, i) => {
    if (i) {
      const sep = document.createElement("span");
      sep.textContent = "/";
      el.append(sep);
    }
    const b = document.createElement("button");
    b.textContent = c.name;
    b.onclick = () => go(c.id);
    el.append(b);
  });
}

async function go(id) {
  const i = S.rects.findIndex((q) => q.id === id);
  if (i >= 0 && S.geom === 0 && !matchMedia("(prefers-reduced-motion: reduce)").matches) {
    const q = S.rects[i];
    image.style.transformOrigin = `${q.x}px ${q.y}px`;
    image.style.transform = `scale(${S.css.w / Math.max(1, q.w)}, ${S.css.h / Math.max(1, q.h)})`;
    image.style.opacity = "0.6";
    await new Promise((r) => setTimeout(r, 180));
  }
  S.node = id;
  await crumbs();
  await draw();
  image.style.transition = "none";
  image.style.transform = "none";
  void image.offsetWidth;
  image.style.transition = "";
  image.style.opacity = "1";
}

let tipTimer = 0, tipUrl = "";
function ago(secs) {
  if (!secs) return "unknown";
  const d = (Date.now() / 1000 - secs) / 86400;
  return d < 1 ? "today" : d < 30 ? `${d | 0} days ago` : d < 365 ? `${(d / 30) | 0} months ago` : `${(d / 365) | 0} years ago`;
}
function hideTip() { clearTimeout(tipTimer); $("tip").hidden = true; if (tipUrl) { URL.revokeObjectURL(tipUrl); tipUrl = ""; } }
function line(cls, text) { const d = document.createElement("div"); if (cls) d.className = cls; d.textContent = text; return d; }
async function showTip(id, mx, my) {
  let info; try { info = await invoke("node_info", { id, size: S.size }); } catch { return; }
  if (S.hover < 0 || S.rects[S.hover]?.id !== id) return;
  const badges = [info.shared && "shared", info.denied && "denied", info.staged && "staged"].filter(Boolean).join(" \u00b7 ");
  const name = document.createElement("b"); name.textContent = info.name;
  $("tip-body").replaceChildren(
    name,
    line("path muted", mid(info.path, 60)),
    line("", human(info.bytes) + (info.is_dir ? ` \u00b7 ${info.files.toLocaleString()} files \u00b7 ${info.dirs.toLocaleString()} folders` : "")),
    line("muted", `modified ${ago(info.mtime)}${badges ? " \u00b7 " + badges : ""}`),
  );
  const img = $("tip-img"); img.hidden = true;
  const tip = $("tip"); tip.hidden = false;
  const st = $("stage").getBoundingClientRect();
  tip.style.left = Math.min(mx + 12, st.width - tip.offsetWidth - 8) + "px";
  tip.style.top = Math.min(my + 12, st.height - tip.offsetHeight - 8) + "px";
  if (!info.is_dir) {
    const buf = await invoke("thumbnail", { id, px: 96 });
    if (buf.byteLength && !$("tip").hidden && S.rects[S.hover]?.id === id) {
      if (tipUrl) URL.revokeObjectURL(tipUrl);
      tipUrl = URL.createObjectURL(new Blob([buf], { type: "image/png" }));
      img.src = tipUrl; img.hidden = false;
    }
  }
}

overlay.parentElement.addEventListener("mousemove", (e) => {
  if (!S.rects.length) return;
  const r = image.getBoundingClientRect();
  const i = hit(e.clientX - r.left, e.clientY - r.top);
  if (i === S.hover) return;
  S.hover = i;
  paintOverlay();
  // With nothing selected, hover previews. Once the user has selected, the
  // inspector belongs to the selection - otherwise moving the mouse toward
  // "Add to Cleanup" would silently retarget it.
  if (i >= 0 && S.sel < 0) inspect(S.rects[i].id);
  hideTip();
  if (i >= 0) {
    const mx = e.clientX - r.left, my = e.clientY - r.top;
    tipTimer = setTimeout(() => showTip(S.rects[i].id, mx, my), 250);
  }
});

overlay.parentElement.addEventListener("mouseleave", () => {
  S.hover = -1;
  hideTip();
  paintOverlay();
});

overlay.parentElement.addEventListener("click", (e) => {
  if (!S.rects.length) return;
  const r = image.getBoundingClientRect();
  const i = hit(e.clientX - r.left, e.clientY - r.top);
  hideTip();
  if (i < 0) { S.sel = -1; paintOverlay(); return; }
  S.sel = S.rects[i].id;
  inspect(S.sel);
  paintOverlay();
});

overlay.parentElement.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const r = image.getBoundingClientRect();
  const i = hit(e.clientX - r.left, e.clientY - r.top);
  if (i >= 0) invoke("reveal", { id: S.rects[i].id });
});

// Navigation moved to double-click so a single click can mean "select".
overlay.parentElement.addEventListener("dblclick", async (e) => {
  if (!S.rects.length) return;
  const r = image.getBoundingClientRect();
  const i = hit(e.clientX - r.left, e.clientY - r.top);
  if (i < 0) return;
  const id = S.rects[i].id;
  const info = await invoke("node_info", { id, size: S.size });
  if (info.is_dir && id !== S.node) go(id);
});

let rzTimer = 0;
new ResizeObserver(() => {
  clearTimeout(rzTimer);
  rzTimer = setTimeout(draw, 120);
}).observe($("stage"));

paintTabs();
paintPending();
paintSnaps();

// ---------------------------------------------------------------- stage 6

// --- mode switching -------------------------------------------------------

const M = {
  mode: "storage",
  apps: [],
  sel: -1,
  detail: null,
  events: [],
  cap: 500,
  paused: false,
  timer: 0,
  seen: 0,
};

function setMode(mode) {
  M.mode = mode;
  [...$("modes").children].forEach((b) => b.classList.toggle("on", b.dataset.mode === mode));
  const storage = mode === "storage";
  $("rail").hidden = !storage;
  $("filter").hidden = !storage;
  $("crumbs").hidden = !storage;
  $("apps-panel").hidden = storage;
  closePops();
  if (storage) showPanel(panel);
  else {
    for (const p of PANELS) $("panel-" + p).hidden = true;
    $("inspector").hidden = true;
  }
  if (mode === "apps" && !M.apps.length) loadApps();
}

[...$("modes").children].forEach((b) => {
  b.onclick = () => setMode(b.dataset.mode);
});

// --- applications ---------------------------------------------------------

async function loadApps() {
  $("apps-count").textContent = "scanning applications...";
  try {
    M.apps = await invoke("apps_list", { guesses: $("apps-guesses").checked });
  } catch (e) {
    $("apps-count").textContent = String(e);
    return;
  }
  const total = M.apps.reduce((a, x) => a + Number(x.bundle_bytes) + Number(x.support_bytes), 0);
  $("apps-count").textContent = `${M.apps.length} applications - ${human(total)}`;
  paintAppList();
}

// Rebuilding the list drops every stored plan on the backend, so any open
// review is now stale. Close it rather than leave a button that will fail.
async function reloadApps() {
  M.detail = null;
  M.sel = -1;
  closeSheet();
  $("app-detail").replaceChildren();
  await loadApps();
}
$("apps-refresh").onclick = reloadApps;
$("apps-guesses").onchange = reloadApps;

function paintAppList() {
  const rows = [...M.apps].sort(
    (a, b) => Number(b.bundle_bytes) + Number(b.support_bytes)
            - Number(a.bundle_bytes) - Number(a.support_bytes),
  );
  $("apps-list").replaceChildren(...rows.map((a) => {
    const li = document.createElement("li");
    li.classList.toggle("on", a.idx === M.sel);

    const name = document.createElement("span");
    name.className = "an";
    name.textContent = a.name;
    const sub = document.createElement("span");
    sub.className = "al";
    const bits = [];
    if (a.leftovers) bits.push(`+${a.leftovers} leftover${a.leftovers === 1 ? "" : "s"}`);
    if (a.contested) bits.push("bundle id shared");
    sub.textContent = bits.join(" - ") || (a.bundle_id ?? "no bundle id");
    name.append(sub);

    const size = document.createElement("span");
    size.className = "as";
    size.textContent = human(Number(a.bundle_bytes) + Number(a.support_bytes));

    li.append(name, size);
    li.onclick = () => openApp(a.idx);
    return li;
  }));
}

function assocList(items) {
  // Grouped by category, biggest group first, so the panel reads as a
  // footprint rather than a flat dump.
  const groups = new Map();
  for (const it of items) {
    if (!groups.has(it.category)) groups.set(it.category, []);
    groups.get(it.category).push(it);
  }
  const out = [];
  const ordered = [...groups.entries()].sort(
    (a, b) => b[1].reduce((s, x) => s + Number(x.bytes), 0)
            - a[1].reduce((s, x) => s + Number(x.bytes), 0),
  );
  for (const [cat, list] of ordered) {
    const h = document.createElement("p");
    h.className = "grp muted";
    const sum = list.reduce((s, x) => s + Number(x.bytes), 0);
    h.textContent = `${cat} - ${human(sum)}`;
    out.push(h);

    const ul = document.createElement("ul");
    for (const it of list) {
      const li = document.createElement("li");
      li.classList.toggle("locked", !it.removable);
      const p = document.createElement("span");
      p.className = "p";
      p.textContent = it.path;
      const ev = document.createElement("span");
      ev.className = "ev";
      ev.textContent = it.removable
        ? it.evidence
        : `${it.evidence} - review only, not removable here`;
      p.append(ev);
      const b = document.createElement("span");
      b.className = "b";
      b.textContent = human(Number(it.bytes));
      li.append(p, b);
      ul.append(li);
    }
    out.push(ul);
  }
  return out;
}

async function openApp(idx) {
  M.sel = idx;
  paintAppList();
  const d = await invoke("app_detail", { idx });
  M.detail = d;

  const box = $("app-detail");
  box.replaceChildren();

  const h = document.createElement("h1");
  h.textContent = d.app.name;
  const sub = document.createElement("p");
  sub.className = "muted path";
  sub.textContent = [d.app.bundle_id ?? "no bundle id", d.app.version ? `v${d.app.version}` : null, d.app.path]
    .filter(Boolean).join("  -  ");
  box.append(h, sub);

  const bundle = Number(d.app.bundle_bytes);
  const support = Number(d.app.support_bytes);
  const total = bundle + support;

  const cap = document.createElement("h3");
  cap.textContent = "Total footprint";
  box.append(cap);

  const foot = document.createElement("div");
  foot.className = "foot";
  const big = document.createElement("div");
  big.className = "big";
  big.textContent = human(total);
  const split = document.createElement("div");
  split.className = "split";
  const bar = document.createElement("div");
  bar.className = "bar";
  const pct = total ? (bundle / total) * 100 : 100;
  const i1 = document.createElement("i");
  i1.className = "b1";
  i1.style.width = `${pct}%`;
  const i2 = document.createElement("i");
  i2.className = "b2";
  i2.style.width = `${100 - pct}%`;
  bar.append(i1, i2);
  const keys = document.createElement("div");
  keys.className = "keys";
  keys.innerHTML =
    `<span><i style="background:var(--accent)"></i>bundle ${human(bundle)}</span>` +
    `<span><i style="background:#c9a888"></i>support files ${human(support)}</span>`;
  split.append(bar, keys);
  foot.append(big, split);
  box.append(foot);

  if (d.app.contested) {
    const w = document.createElement("p");
    w.className = "muted";
    w.textContent =
      "Another installed app claims this bundle id, so its shared data is kept. Only the app bundle can be removed.";
    box.append(w);
  }

  const ah = document.createElement("h3");
  ah.textContent = "Associated files";
  box.append(ah, ...assocList(d.items));

  const btn = document.createElement("button");
  btn.className = "wide danger";
  btn.textContent = "Uninstall Completely";
  btn.disabled = d.staged.length === 0;
  btn.onclick = () => reviewUninstall(d);
  box.append(btn);

  const note = document.createElement("p");
  note.className = "muted tiny";
  note.textContent = `${d.staged.length} item(s), ${human(Number(d.stage_bytes))} would be staged. Nothing is deleted until you commit.`;
  box.append(note);
}

// --- review sheet ---------------------------------------------------------

// Nothing is staged until this sheet is confirmed.
function reviewUninstall(d) {
  $("sheet-title").textContent = `Uninstall ${d.app.name}`;
  $("sheet-sub").textContent =
    `${d.staged.length} item(s), ${human(Number(d.stage_bytes))} will be moved to staging.`;

  const body = $("sheet-body");
  body.replaceChildren();

  if (d.unload.length) {
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = `Will be stopped first so nothing recreates its files: ${d.unload.join(", ")}`;
    body.append(p);
  }

  // The plan's own two lists, not a re-derivation from the panel. What the
  // sheet shows is what `app_uninstall` will stage, because both are the same
  // stored plan.
  const go = d.staged;
  const keep = d.excluded;

  const h1 = document.createElement("p");
  h1.className = "grp muted";
  h1.textContent = "Will be staged";
  body.append(h1);
  const ul = document.createElement("ul");
  for (const it of go) {
    const li = document.createElement("li");
    const p = document.createElement("span");
    p.className = "p";
    p.textContent = it.path;
    // Say what each match rests on. With "possible leftovers" ticked these
    // include guesses, and a guess about to be moved should say so.
    const ev = document.createElement("span");
    ev.className = "ev";
    ev.textContent = it.evidence;
    p.append(ev);
    const b = document.createElement("span");
    b.className = "b";
    b.textContent = human(Number(it.bytes));
    li.append(p, b);
    ul.append(li);
  }
  body.append(ul);

  if (keep.length) {
    const h2 = document.createElement("p");
    h2.className = "grp muted";
    h2.textContent = "Left alone";
    body.append(h2);
    const ul2 = document.createElement("ul");
    for (const it of keep) {
      const li = document.createElement("li");
      li.className = "locked";
      const p = document.createElement("span");
      p.className = "p";
      p.textContent = it.path;
      const ev = document.createElement("span");
      ev.className = "ev";
      ev.textContent = it.why ?? "";
      p.append(ev);
      const b = document.createElement("span");
      b.textContent = human(Number(it.bytes));
      li.append(p, b);
      ul2.append(li);
    }
    body.append(ul2);
  }

  $("sheet-go").onclick = async () => {
    $("sheet-go").disabled = true;
    $("sheet-go").textContent = "staging...";
    try {
      const r = await invoke("app_uninstall", { token: d.token });
      closeSheet();
      const missed = r.refused.length
        ? ` - ${r.refused.length} not moved: ${r.refused.map(([p, w]) => `${p} (${w})`).join("; ")}`
        : "";
      $("status").textContent =
        `staged ${r.count} item(s), ${human(Number(r.bytes))} - undo from Staged, manifest ${r.manifest}${missed}`;
      for (const [what, why] of r.unload_problems) {
        console.warn(`${what}: ${why}`);
      }
      await paintPending();
      await loadApps();
      $("app-detail").replaceChildren();
    } catch (e) {
      $("sheet-sub").textContent = String(e);
    }
    $("sheet-go").disabled = false;
    $("sheet-go").textContent = "Stage for removal";
  };
  setOpen($("sheet"), true);
}

$("sheet-cancel").onclick = closeSheet;
$("sheet").onclick = (e) => { if (e.target === $("sheet")) closeSheet(); };

// --- monitor --------------------------------------------------------------

function paintEvents() {
  $("mon-list").replaceChildren(...M.events.map((e) => {
    const li = document.createElement("li");
    const k = document.createElement("span");
    k.className = "k";
    k.textContent = e.kind;
    const d = document.createElement("span");
    d.className = `d${Number(e.bytes) > 0 ? " up" : ""}`;
    const n = Number(e.bytes);
    d.textContent = `${n < 0 ? "-" : "+"}${human(Math.abs(n))}`;
    const p = document.createElement("span");
    p.className = "p";
    p.textContent = e.path;
    li.append(k, d, p);
    return li;
  }));
  $("mon-empty").hidden = M.events.length > 0;
}

async function tick() {
  let batch = [];
  try {
    // The drain runs even while paused, so a burst cannot back up in the
    // channel; pausing keeps nothing rather than deferring everything.
    batch = await invoke("watch_poll");
  } catch (e) {
    $("mon-status").textContent = String(e);
    return stopMonitor();
  }
  if (!batch.length) return;
  M.seen += batch.length;
  if (!M.paused) {
    M.events.unshift(...batch.reverse());
    if (M.events.length > M.cap) M.events.length = M.cap;
    paintEvents();
  }
  $("mon-status").textContent = `${M.seen} event(s)${M.paused ? " - paused" : ""}`;
}

function stopMonitor() {
  clearInterval(M.timer);
  M.timer = 0;
  invoke("watch_stop");
  $("mon-start").textContent = "Start";
  $("mon-pause").disabled = true;
}

// The same plugin command the Storage tab's picker uses, so the page needs no
// bundled JS binding. It also makes this panel reachable without typing, which
// is what kept the monitor unverified.
$("mon-pick").onclick = async () => {
  const dir = await invoke("plugin:dialog|open", {
    options: { directory: true, multiple: false, title: "Choose a folder to watch" },
  });
  const path = Array.isArray(dir) ? dir[0] : dir;
  if (!path) return;
  $("mon-path").value = typeof path === "string" ? path : path.path;
  $("mon-start").click();
};

$("mon-start").onclick = async () => {
  if (M.timer) return stopMonitor();
  const path = $("mon-path").value.trim() || $("path").value.trim();
  if (!path) { $("mon-status").textContent = "give a path to watch"; return; }
  try {
    await invoke("watch_start", { path });
  } catch (e) {
    $("mon-status").textContent = String(e);
    return;
  }
  M.seen = 0;
  M.paused = false;
  $("mon-pause").textContent = "Pause";
  $("mon-pause").disabled = false;
  $("mon-start").textContent = "Stop";
  $("mon-status").textContent = `watching ${path}`;
  // One render per drain, never one per event.
  M.timer = setInterval(tick, 400);
};

$("mon-pause").onclick = () => {
  M.paused = !M.paused;
  $("mon-pause").textContent = M.paused ? "Resume" : "Pause";
  $("mon-status").textContent = `${M.seen} event(s)${M.paused ? " - paused" : ""}`;
};

$("mon-clear").onclick = () => { M.events = []; paintEvents(); };

setMode("storage");
paintHome();
