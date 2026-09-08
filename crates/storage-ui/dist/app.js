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

const $ = (id) => document.getElementById(id);
const image = $("image");
const overlay = $("overlay");
const ictx = image.getContext("2d");
const octx = overlay.getContext("2d");

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

VIEWS.forEach((name, i) => {
  const b = document.createElement("button");
  b.textContent = name;
  b.onclick = () => {
    S.view = i;
    $("scope-wrap").hidden = i !== 6;
    paintTabs();
    draw();
  };
  $("tabs").append(b);
});

function paintTabs() {
  [...$("tabs").children].forEach((b, i) => b.classList.toggle("on", i === S.view));
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
$("pick").onclick = async () => {
  // Invoked as a plugin command so the page needs no bundled JS binding.
  const dir = await invoke("plugin:dialog|open", {
    options: { directory: true, multiple: false, title: "Choose a folder to scan" },
  });
  const path = Array.isArray(dir) ? dir[0] : dir;
  if (path) startScan(typeof path === "string" ? path : path.path);
};
$("path").onkeydown = (e) => { if (e.key === "Enter") $("go").click(); };

invoke("targets").then((list) => {
  for (const t of list) {
    const b = document.createElement("button");
    b.textContent = t.label;
    b.title = t.path;
    b.onclick = () => startScan(t.path);
    $("targets").append(b);
  }
});

// -------------------------------------------------------------------- scan

async function startScan(path) {
  if (!path) return;
  $("status").textContent = "scanning...";
  $("empty").textContent = `Scanning ${path}`;
  let unlisten = null;
  try {
    unlisten = await window.__TAURI__.event.listen("scan_progress", (e) => {
      $("status").textContent = `scanning... ${Number(e.payload).toLocaleString()} entries`;
      $("empty").textContent = `Scanning ${path}\n${Number(e.payload).toLocaleString()} entries`;
    });
    const ov = await invoke("scan_dir", { path, size: S.size });
    if (unlisten) unlisten();
    S.scanned = true;
    S.node = 0;
    tray.clear();
    paintTray();
    $("results").hidden = true;
    $("dupe-card").hidden = false;
    $("dupe-total").textContent = "";
    $("dupe-actions").hidden = true;
    $("dupe-note").textContent = "";
    paintSnaps();
    $("path").value = path;
    applyOverview(ov);
    await crumbs();
    await draw();
  } catch (e) {
    if (unlisten) unlisten();
    $("status").textContent = String(e);
    $("empty").textContent = String(e);
  }
}

async function refreshOverview() {
  if (!S.scanned) return;
  applyOverview(await invoke("overview", { size: S.size }));
}

function applyOverview(ov) {
  $("volume-card").hidden = false;
  $("wins-card").hidden = false;
  $("types-card").hidden = false;
  $("empty").hidden = true;
  $("status").textContent = `${ov.elapsed_ms} ms scan`;

  const shown = [ov.freeable, ov.allocated, ov.logical][S.size];
  $("title-name").textContent = ov.path;
  $("title-sub").textContent =
    `${human(shown)} · ${ov.files.toLocaleString()} files · ${ov.dirs.toLocaleString()} folders` +
    (ov.denied ? ` · ${ov.denied} unreadable` : "") +
    (ov.shared ? ` · ${ov.shared} sharing blocks` : "");

  const total = Math.max(1, Number(ov.volume_total));
  const scanned = Math.min(Number(ov.allocated), total);
  const other = Math.max(0, Number(ov.volume_used) - scanned);
  const a = (scanned / total) * 360;
  const b = a + (other / total) * 360;
  $("donut").style.background =
    `conic-gradient(#a8734a 0 ${a}deg, #cdbfa6 ${a}deg ${b}deg, #e8e2d4 ${b}deg 360deg)`;
  $("v-scanned").textContent = human(scanned);
  $("v-other").textContent = human(other);
  $("v-free").textContent = human(ov.volume_free);
  $("scanpath").textContent = ov.path;
  $("scanmeta").textContent =
    `freeable ${human(ov.freeable)} of ${human(ov.allocated)} allocated`;

  $("wins").replaceChildren(...ov.wins.map((w) => {
    const li = document.createElement("li");
    const l = document.createElement("span");
    l.innerHTML = `<b>${w.label}</b><br><small>${w.items.toLocaleString()} files</small>`;
    const r = document.createElement("span");
    r.textContent = human(w.bytes);
    li.append(l, r);
    li.title = w.path;
    li.onclick = () => go(w.id);
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
  if (u8.byteLength < 16) return;
  const dv = new DataView(u8.buffer, u8.byteOffset);
  const pw = dv.getUint32(0, true), ph = dv.getUint32(4, true);
  const n = dv.getUint32(8, true);
  S.geom = dv.getUint32(12, true);

  const rects = new Array(n);
  let off = 16;
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

  for (const c of [image, overlay]) {
    c.width = pw; c.height = ph;
    c.style.width = w + "px"; c.style.height = h + "px";
  }
  const px = new Uint8ClampedArray(u8.buffer, u8.byteOffset + off, pw * ph * 4);
  ictx.putImageData(new ImageData(px, pw, ph), 0, 0);
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
    octx.strokeStyle = "#3a352c";
    octx.fillStyle = "rgba(255,255,255,0.18)";
    traceRect(S.hover);
    octx.fill();
    octx.stroke();
  }
  if (si >= 0) {
    octx.lineWidth = 3;
    octx.strokeStyle = "#1d1a15";
    octx.fillStyle = "rgba(255,255,255,0.34)";
    traceRect(si);
    octx.fill();
    octx.stroke();
    octx.setLineDash([5, 3]);
    octx.lineWidth = 1;
    octx.strokeStyle = "#fdfaf2";
    traceRect(si);
    octx.stroke();
    octx.setLineDash([]);
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
};
$("tray-clear").onclick = () => { tray.clear(); paintTray(); };

async function paintTray() {
  $("tray-card").hidden = tray.size === 0;
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
    tray.clear();
    paintTray();
    $("results").hidden = true;
    $("dupe-card").hidden = false;
    $("dupe-total").textContent = "";
    $("dupe-actions").hidden = true;
    $("dupe-note").textContent = "";
    paintSnaps();
    $("status").textContent =
      `staged ${r.moved} items, ${human(r.total_bytes)}` +
      (r.skipped ? ` (${r.skipped} changed, skipped)` : "");
    await paintPending();
    await startScan($("path").value.trim());
  } catch (e) {
    $("tray-total").textContent = String(e);
  }
};

async function paintPending() {
  let list = [];
  try { list = await invoke("cleanup_list"); } catch { return; }
  $("pending-card").hidden = list.length === 0;
  $("pending").replaceChildren(...list.map((m) => {
    const li = document.createElement("li");
    const n = document.createElement("span");
    n.className = "name";
    n.textContent = `${human(m.total_bytes)} · ${m.items} items · ${m.expired ? "expired" : m.expires_in_days + "d left"}`;
    const acts = document.createElement("span");
    acts.className = "acts";
    const undo = document.createElement("button");
    undo.textContent = "Undo";
    undo.onclick = async () => {
      const n = await invoke("cleanup_restore", { id: m.id });
      $("status").textContent = `restored ${n} items`;
      await paintPending();
      await startScan($("path").value.trim());
    };
    const del = document.createElement("button");
    del.className = "danger";
    del.textContent = "Commit";
    del.onclick = async () => {
      if (!confirm(`Permanently delete ${m.items} items (${human(m.total_bytes)})? This cannot be undone.`)) return;
      const freed = await invoke("cleanup_commit", { id: m.id });
      $("status").textContent = `freed ${human(freed)}`;
      await paintPending();
    };
    acts.append(undo, del);
    li.append(n, acts);
    return li;
  }));
}

// ---------------------------------------------------------------- stage 5

function showResults(title, items) {
  $("results-title").textContent = title;
  $("results-list").replaceChildren(...items);
  $("results").hidden = false;
}
$("results-close").onclick = () => { $("results").hidden = true; };

// --- snapshots ------------------------------------------------------------

$("snap-save").onclick = async () => {
  $("snap-save").textContent = "saving...";
  try {
    await invoke("snapshot_save");
    await paintSnaps();
    $("status").textContent = "snapshot saved";
  } catch (e) {
    $("status").textContent = String(e);
  }
  $("snap-save").textContent = "Save snapshot";
};

async function paintSnaps() {
  let list = [];
  try { list = await invoke("snapshot_list"); } catch { return; }
  $("snap-card").hidden = !S.scanned;
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
    $("status").textContent = String(e);
    return;
  }
  if (!rows.length) {
    showResults("No changes between those snapshots", []);
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
  showResults(`${rows.length} change(s), net ${sign}${human(Math.abs(net))}`, items);
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
    showResults(`Duplicates — ${human(d.total_reclaimable)} reclaimable`, items);
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
    showResults(
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
  S.node = id;
  await crumbs();
  await draw();
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
});

overlay.parentElement.addEventListener("mouseleave", () => {
  S.hover = -1;
  paintOverlay();
});

overlay.parentElement.addEventListener("click", (e) => {
  if (!S.rects.length) return;
  const r = image.getBoundingClientRect();
  const i = hit(e.clientX - r.left, e.clientY - r.top);
  if (i < 0) { S.sel = -1; paintOverlay(); return; }
  S.sel = S.rects[i].id;
  inspect(S.sel);
  paintOverlay();
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

const STORAGE_ONLY = ["tabs", "controls", "stage", "title"];

function setMode(mode) {
  M.mode = mode;
  [...$("modes").children].forEach((b) => b.classList.toggle("on", b.dataset.mode === mode));
  STORAGE_ONLY.forEach((id) => { $(id).hidden = mode !== "storage"; });
  $("apps-panel").hidden = mode !== "apps";
  $("mon-panel").hidden = mode !== "monitor";
  $("rail").hidden = mode !== "storage";
  $("inspector").hidden = mode !== "storage";
  $("filter").hidden = mode !== "storage";
  if (mode === "apps" && !M.apps.length) loadApps();
  if (mode === "storage") draw();
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

$("apps-refresh").onclick = loadApps;
$("apps-guesses").onchange = loadApps;

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
  btn.disabled = d.stage_count === 0;
  btn.onclick = () => reviewUninstall(d);
  box.append(btn);

  const note = document.createElement("p");
  note.className = "muted tiny";
  note.textContent = `${d.stage_count} item(s), ${human(Number(d.stage_bytes))} would be staged. Nothing is deleted until you commit.`;
  box.append(note);
}

// --- review sheet ---------------------------------------------------------

// Nothing is staged until this sheet is confirmed.
function reviewUninstall(d) {
  $("sheet-title").textContent = `Uninstall ${d.app.name}`;
  $("sheet-sub").textContent =
    `${d.stage_count} item(s), ${human(Number(d.stage_bytes))} will be moved to staging.`;

  const body = $("sheet-body");
  body.replaceChildren();

  if (d.unload.length) {
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = `Will be stopped first so nothing recreates its files: ${d.unload.join(", ")}`;
    body.append(p);
  }

  const go = d.items.filter((i) => i.removable);
  const keep = d.items.filter((i) => !i.removable);

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
    const b = document.createElement("span");
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
      const r = await invoke("app_uninstall", { idx: d.app.idx });
      $("sheet").hidden = true;
      $("status").textContent =
        `staged ${r.count} item(s), ${human(Number(r.bytes))} - undo from Staged, manifest ${r.manifest}`;
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
  $("sheet").hidden = false;
}

$("sheet-cancel").onclick = () => { $("sheet").hidden = true; };
$("sheet").onclick = (e) => { if (e.target === $("sheet")) $("sheet").hidden = true; };

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
