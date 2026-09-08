//! Pure geometry. Every view is one function returning `Vec<Rect>` in CSS
//! pixels. Nothing here knows about colours, images, or the UI.
//!
//! The render budget is pixels, not nodes: a rect smaller than `MIN_AREA` or
//! thinner than `MIN_SIDE` is never emitted, and the whole list is capped at
//! `MAX_RECTS`. That cap is what makes hit-testing a linear scan and the
//! rasterizer's cost independent of tree size.

use crate::{NodeId, Tree};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub id: NodeId,
    pub depth: u16,
}

pub const MIN_AREA: f32 = 16.0;
pub const MIN_SIDE: f32 = 3.0;
pub const MAX_RECTS: usize = 5_000;

/// Treemap header strip: a directory tile taller than this gets its name in a
/// strip and its children below it.
pub const HEADER: f32 = 14.0;
pub const BAR_ROW: f32 = 26.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Treemap,
    Folders,
    Sunburst,
    Flame,
    Bubbles,
    MindMap,
    TopSizes,
    AgeMap,
}

/// How the frontend must hit-test the rect list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Geom {
    /// `x, y, w, h` are pixels.
    Cartesian,
    /// `x` = start angle (rad), `y` = inner radius, `w` = sweep, `h` = ring width.
    Polar,
    /// `x, y, w, h` is the bounding box of a circle.
    Circle,
}

impl View {
    pub fn geom(self) -> Geom {
        match self {
            View::Sunburst => Geom::Polar,
            View::Bubbles | View::MindMap => Geom::Circle,
            _ => Geom::Cartesian,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SizeMode {
    /// `sub_excl` — what deleting actually frees. The product's default.
    Freeable,
    Allocated,
    Logical,
}

/// Top Sizes scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Here,
    FilesAnywhere,
    FoldersAnywhere,
}

#[derive(Clone, Copy, Debug)]
pub struct Opts {
    pub w: f32,
    pub h: f32,
    /// Levels of tree to draw, 1 = direct children only.
    pub levels: u16,
    pub size: SizeMode,
    pub scope: Scope,
    /// Unix seconds, supplied by the caller so layout stays pure.
    pub now: u32,
}

pub fn size_of(t: &Tree, id: NodeId, m: SizeMode) -> u64 {
    let i = id as usize;
    match m {
        SizeMode::Freeable => t.sub_excl[i],
        SizeMode::Allocated => t.sub_blocks[i],
        SizeMode::Logical => t.sub_logical[i],
    }
}

/// Children with non-zero weight, heaviest first, ties broken on id.
///
/// That tie-break is the `resquarify` property we actually need: the layout is
/// a pure function of the node set, so navigating in and back out, or changing
/// size mode, never reshuffles tiles of equal weight.
fn kids(t: &Tree, id: NodeId, m: SizeMode) -> Vec<(NodeId, f64)> {
    let mut v: Vec<(NodeId, f64)> = t
        .children(id)
        .map(|c| (c, size_of(t, c, m) as f64))
        .filter(|&(_, w)| w > 0.0)
        .collect();
    v.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

pub fn layout(t: &Tree, root: NodeId, view: View, o: &Opts) -> Vec<Rect> {
    let mut out = Vec::new();
    if t.is_empty() || root as usize >= t.len() || o.w < 4.0 || o.h < 4.0 {
        return out;
    }
    match view {
        View::Treemap => treemap(t, root, 0.0, 0.0, o.w, o.h, 0, o, &mut out),
        View::Folders => folders(t, root, o, &mut out),
        View::Sunburst => sunburst(t, root, o, &mut out),
        View::Flame => flame(t, root, 0.0, o.w, 0, o, &mut out),
        View::Bubbles => bubbles(t, root, 0.0, 0.0, o.w, o.h, 0, o, &mut out),
        View::MindMap => mind_map(t, root, o, &mut out),
        View::TopSizes => top_sizes(t, root, o, &mut out),
        View::AgeMap => age_map(t, root, o, &mut out),
    }
    debug_check(t, view, o, &out);
    out
}

// ---------------------------------------------------------------- squarified

/// Bruls/Huizing/van Wijk. The suffix array is the fix for the O(n^2) in
/// every naive port: recomputing the remaining sum inside the row loop costs
/// ~1.25e9 additions on a 50k-child directory.
fn squarify(
    items: &[(NodeId, f64)],
    mut x: f32,
    mut y: f32,
    mut w: f32,
    mut h: f32,
    out: &mut Vec<(NodeId, f32, f32, f32, f32)>,
) {
    let n = items.len();
    let mut suffix = vec![0f64; n + 1];
    for k in (0..n).rev() {
        suffix[k] = suffix[k + 1] + items[k].1;
    }

    let mut i = 0;
    while i < n {
        let remaining = suffix[i];
        if remaining <= 0.0 || w < 1.0 || h < 1.0 {
            break;
        }
        // Pixels per weight unit, for the sub-rectangle still to be filled.
        let scale = (w as f64) * (h as f64) / remaining;
        // Items are sorted descending, so once the head of the remainder is
        // sub-pixel every tile after it is too. This is the cull that keeps a
        // 50k-child directory cheap.
        if items[i].1 * scale < MIN_AREA as f64 {
            break;
        }
        let side = w.min(h) as f64;

        let (mut j, mut sum, mut max, mut min) = (i, 0.0f64, 0.0f64, f64::INFINITY);
        let mut best = f64::INFINITY;
        while j < n {
            let a = items[j].1 * scale;
            let (nmax, nmin) = (max.max(a), min.min(a));
            let ns = sum + a;
            let ratio = worst(ns, nmax, nmin, side);
            if j > i && ratio > best {
                break;
            }
            best = ratio;
            sum = ns;
            max = nmax;
            min = nmin;
            j += 1;
        }

        let thick = (sum / side) as f32;
        let mut off = 0.0f32;
        for k in i..j {
            let a = items[k].1 * scale;
            let len = ((a / sum) as f32 * side as f32).clamp(0.0, side as f32 - off);
            if w <= h {
                out.push((items[k].0, x + off, y, len, thick));
            } else {
                out.push((items[k].0, x, y + off, thick, len));
            }
            off += len;
        }
        if w <= h {
            y += thick;
            h -= thick;
        } else {
            x += thick;
            w -= thick;
        }
        i = j;
    }
}

fn worst(sum: f64, max: f64, min: f64, side: f64) -> f64 {
    if sum <= 0.0 || min <= 0.0 {
        return f64::INFINITY;
    }
    let s2 = sum * sum;
    let side2 = side * side;
    (side2 * max / s2).max(s2 / (side2 * min))
}

#[allow(clippy::too_many_arguments)]
fn treemap(
    t: &Tree,
    id: NodeId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    depth: u16,
    o: &Opts,
    out: &mut Vec<Rect>,
) {
    if out.len() >= MAX_RECTS || depth >= o.levels || w < MIN_SIDE || h < MIN_SIDE {
        return;
    }
    let items = kids(t, id, o.size);
    if items.is_empty() {
        return;
    }
    let mut placed = Vec::new();
    squarify(&items, x, y, w, h, &mut placed);

    for (cid, cx, cy, cw, ch) in placed {
        if out.len() >= MAX_RECTS {
            break;
        }
        if cw < MIN_SIDE || ch < MIN_SIDE || cw * ch < MIN_AREA {
            continue;
        }
        out.push(Rect { x: cx, y: cy, w: cw, h: ch, id: cid, depth });
        if t.flags[cid as usize].is_dir() && depth + 1 < o.levels && ch > HEADER + 6.0 && cw > 12.0
        {
            treemap(t, cid, cx + 1.0, cy + HEADER, cw - 2.0, ch - HEADER - 1.0, depth + 1, o, out);
        }
    }
}

// ------------------------------------------------------------------- folders

fn folders(t: &Tree, root: NodeId, o: &Opts, out: &mut Vec<Rect>) {
    let items = kids(t, root, o.size);
    if items.is_empty() {
        return;
    }
    let n = items.len().min(MAX_RECTS);
    // Cards are wide, so names survive truncation.
    let cols = (((n as f32) * o.w / (o.h * 3.0)).sqrt().ceil() as usize).clamp(1, n);
    let rows = n.div_ceil(cols);
    let (cw, ch) = (o.w / cols as f32, o.h / rows as f32);
    if cw < MIN_SIDE + 6.0 || ch < MIN_SIDE + 6.0 {
        return;
    }
    for (k, &(id, _)) in items.iter().take(n).enumerate() {
        let (c, r) = (k % cols, k / cols);
        out.push(Rect {
            x: c as f32 * cw + 3.0,
            y: r as f32 * ch + 3.0,
            w: cw - 6.0,
            h: ch - 6.0,
            id,
            depth: 0,
        });
    }
}

// ------------------------------------------------------------------ sunburst

/// Centre and outer radius shared by the two radial views and the rasterizer.
pub fn polar_center(o: &Opts) -> (f32, f32, f32) {
    let r = (o.w.min(o.h) / 2.0 - 8.0).max(8.0);
    (o.w / 2.0, o.h / 2.0, r)
}

fn sunburst(t: &Tree, root: NodeId, o: &Opts, out: &mut Vec<Rect>) {
    let (_, _, rmax) = polar_center(o);
    let r0 = rmax * 0.16;
    let ring = (rmax - r0) / o.levels.max(1) as f32;
    out.push(Rect { x: 0.0, y: 0.0, w: std::f32::consts::TAU, h: r0, id: root, depth: 0 });
    sunburst_ring(t, root, 0.0, std::f32::consts::TAU, r0, ring, 0, o, out);
}

#[allow(clippy::too_many_arguments)]
fn sunburst_ring(
    t: &Tree,
    id: NodeId,
    a0: f32,
    sweep: f32,
    r_in: f32,
    ring: f32,
    depth: u16,
    o: &Opts,
    out: &mut Vec<Rect>,
) {
    if out.len() >= MAX_RECTS || depth >= o.levels || sweep <= 0.0 {
        return;
    }
    let items = kids(t, id, o.size);
    let total: f64 = items.iter().map(|i| i.1).sum();
    if total <= 0.0 {
        return;
    }
    let r_mid = r_in + ring / 2.0;
    let mut a = a0;
    for (cid, wgt) in items {
        if out.len() >= MAX_RECTS {
            break;
        }
        let s = sweep * (wgt / total) as f32;
        // Arc length at mid radius stands in for the width of a cartesian rect.
        if s * r_mid < MIN_SIDE || s * r_mid * ring < MIN_AREA {
            break;
        }
        out.push(Rect { x: a, y: r_in, w: s, h: ring, id: cid, depth: depth + 1 });
        if t.flags[cid as usize].is_dir() {
            sunburst_ring(t, cid, a, s, r_in + ring, ring, depth + 1, o, out);
        }
        a += s;
    }
}

// --------------------------------------------------------------------- flame

/// Icicle rows divide the canvas height by the depth budget, the way d3's
/// partition layout does, so the chart always fills its frame.
fn flame_row(o: &Opts) -> f32 {
    o.h / o.levels.max(1) as f32
}

fn flame(t: &Tree, id: NodeId, x: f32, w: f32, depth: u16, o: &Opts, out: &mut Vec<Rect>) {
    if out.len() >= MAX_RECTS || depth >= o.levels || w < MIN_SIDE {
        return;
    }
    let row = flame_row(o);
    let y = depth as f32 * row;
    let items = kids(t, id, o.size);
    let total: f64 = items.iter().map(|i| i.1).sum();
    if total <= 0.0 {
        return;
    }
    let mut cx = x;
    for (cid, wgt) in items {
        if out.len() >= MAX_RECTS {
            break;
        }
        let cw = w * (wgt / total) as f32;
        if cw < MIN_SIDE {
            break;
        }
        out.push(Rect { x: cx, y, w: cw - 0.5, h: row - 1.0, id: cid, depth });
        if t.flags[cid as usize].is_dir() {
            flame(t, cid, cx, cw, depth + 1, o, out);
        }
        cx += cw;
    }
}

// ------------------------------------------------------------------- bubbles

// ponytail: circles are inscribed in squarified cells rather than packed with
// d3's front-chain algorithm. Areas stay proportional and nesting works; swap
// in packEnclose if the gaps ever start to matter.
#[allow(clippy::too_many_arguments)]
fn bubbles(
    t: &Tree,
    id: NodeId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    depth: u16,
    o: &Opts,
    out: &mut Vec<Rect>,
) {
    if out.len() >= MAX_RECTS || depth >= o.levels || w < MIN_SIDE || h < MIN_SIDE {
        return;
    }
    let items = kids(t, id, o.size);
    if items.is_empty() {
        return;
    }
    let mut placed = Vec::new();
    squarify(&items, x, y, w, h, &mut placed);
    for (cid, cx, cy, cw, ch) in placed {
        if out.len() >= MAX_RECTS {
            break;
        }
        let r = cw.min(ch) / 2.0 * 0.94;
        if r * 2.0 < MIN_SIDE + 2.0 {
            continue;
        }
        let (mx, my) = (cx + cw / 2.0, cy + ch / 2.0);
        out.push(Rect { x: mx - r, y: my - r, w: r * 2.0, h: r * 2.0, id: cid, depth });
        if t.flags[cid as usize].is_dir() && depth + 1 < o.levels {
            // Square that fits inside the circle, so children stay enclosed.
            let s = r * 1.30;
            bubbles(t, cid, mx - s / 2.0, my - s / 2.0, s, s, depth + 1, o, out);
        }
    }
}

// ------------------------------------------------------------------ mind map

fn mind_map(t: &Tree, root: NodeId, o: &Opts, out: &mut Vec<Rect>) {
    let (cx, cy, rmax) = polar_center(o);
    let step = (rmax - 10.0) / o.levels.max(1) as f32;
    let total = size_of(t, root, o.size).max(1) as f64;
    out.push(Rect { x: cx - 7.0, y: cy - 7.0, w: 14.0, h: 14.0, id: root, depth: 0 });
    mind_arm(t, root, 0.0, std::f32::consts::TAU, step, 1, total, o, (cx, cy), out);
}

#[allow(clippy::too_many_arguments)]
fn mind_arm(
    t: &Tree,
    id: NodeId,
    a0: f32,
    sweep: f32,
    step: f32,
    depth: u16,
    total: f64,
    o: &Opts,
    c: (f32, f32),
    out: &mut Vec<Rect>,
) {
    if out.len() >= MAX_RECTS || depth > o.levels || sweep <= 0.0 {
        return;
    }
    let items = kids(t, id, o.size);
    let sum: f64 = items.iter().map(|i| i.1).sum();
    if sum <= 0.0 {
        return;
    }
    let r = depth as f32 * step;
    let mut a = a0;
    for (cid, wgt) in items {
        if out.len() >= MAX_RECTS {
            break;
        }
        let s = sweep * (wgt / sum) as f32;
        if s * r < 10.0 {
            break;
        }
        let mid = a + s / 2.0;
        let dot = 4.0 + 11.0 * (wgt / total).sqrt() as f32;
        let (px, py) = (c.0 + r * mid.cos(), c.1 + r * mid.sin());
        out.push(Rect { x: px - dot, y: py - dot, w: dot * 2.0, h: dot * 2.0, id: cid, depth });
        if t.flags[cid as usize].is_dir() {
            mind_arm(t, cid, a, s, step, depth + 1, total, o, c, out);
        }
        a += s;
    }
}

// ----------------------------------------------------------------- top sizes

/// The ids Top Sizes ranks, heaviest first. Public so the inspector can reuse
/// the same ordering for LARGEST INSIDE.
pub fn ranked(t: &Tree, root: NodeId, o: &Opts) -> Vec<NodeId> {
    let mut v: Vec<NodeId> = match o.scope {
        Scope::Here => t.children(root).collect(),
        Scope::FilesAnywhere => {
            t.descendants(root).filter(|&i| !t.flags[i as usize].is_dir()).collect()
        }
        Scope::FoldersAnywhere => {
            t.descendants(root).filter(|&i| t.flags[i as usize].is_dir()).collect()
        }
    };
    v.sort_unstable_by(|&a, &b| size_of(t, b, o.size).cmp(&size_of(t, a, o.size)).then(a.cmp(&b)));
    v.truncate(MAX_RECTS);
    v
}

fn top_sizes(t: &Tree, root: NodeId, o: &Opts, out: &mut Vec<Rect>) {
    let rows = ((o.h / BAR_ROW).floor() as usize).min(MAX_RECTS);
    for (k, id) in ranked(t, root, o).into_iter().take(rows).enumerate() {
        if size_of(t, id, o.size) == 0 {
            break;
        }
        out.push(Rect { x: 0.0, y: k as f32 * BAR_ROW, w: o.w, h: BAR_ROW - 4.0, id, depth: 0 });
    }
}

// ------------------------------------------------------------------- age map

/// Age bucket upper bounds in days, and their labels.
pub const AGE_BUCKETS: [(u32, &str); 6] = [
    (7, "1 week"),
    (30, "1 month"),
    (90, "3 months"),
    (365, "1 year"),
    (730, "2 years"),
    (u32::MAX, "older"),
];

pub fn age_bucket(now: u32, mtime: u32) -> usize {
    let days = now.saturating_sub(mtime) / 86_400;
    AGE_BUCKETS.iter().position(|&(d, _)| days < d).unwrap_or(5)
}

/// Civil year/month from a unix timestamp. Proleptic Gregorian, no leap
/// seconds, which is all a heatmap axis needs.
pub fn year_month(ts: u32) -> (i32, u32) {
    let days = (ts / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y as i32, m as u32)
}

/// Age Map is three stacked sections; `depth` on each rect says which one it
/// came from. Returns (histogram bottom, heatmap bottom).
pub fn age_sections(o: &Opts) -> (f32, f32) {
    let hist = o.h * 0.26;
    (hist, hist + o.h * 0.40)
}

pub const AGE_YEARS: usize = 12;

fn age_map(t: &Tree, root: NodeId, o: &Opts, out: &mut Vec<Rect>) {
    let (hist_h, heat_end) = age_sections(o);

    let mut bucket_bytes = [0u64; 6];
    let mut bucket_big = [(0u64, root); 6];
    let now_y = year_month(o.now).0;
    let y0 = now_y - (AGE_YEARS as i32 - 1);
    // Bytes and the biggest single file per (year, month), so hovering a cell
    // still selects a real node.
    let mut cell: Vec<(u64, u64, NodeId)> = vec![(0, 0, root); AGE_YEARS * 12];

    for id in t.descendants(root) {
        let i = id as usize;
        if t.flags[i].is_dir() || t.mtime[i] == 0 {
            continue;
        }
        let bytes = size_of(t, id, o.size);
        if bytes == 0 {
            continue;
        }
        let b = age_bucket(o.now, t.mtime[i]);
        bucket_bytes[b] += bytes;
        if bytes > bucket_big[b].0 {
            bucket_big[b] = (bytes, id);
        }
        let (y, m) = year_month(t.mtime[i]);
        if y >= y0 && y <= now_y {
            let k = (y - y0) as usize * 12 + (m - 1) as usize;
            cell[k].0 += bytes;
            if bytes > cell[k].1 {
                cell[k].1 = bytes;
                cell[k].2 = id;
            }
        }
    }

    // Histogram: six bars across the top.
    let maxb = bucket_bytes.iter().copied().max().unwrap_or(0).max(1);
    let bw = o.w / 6.0;
    for (k, &bytes) in bucket_bytes.iter().enumerate() {
        if bytes == 0 {
            continue;
        }
        let bh = (hist_h - 26.0) * (bytes as f32 / maxb as f32);
        if bh < MIN_SIDE {
            continue;
        }
        out.push(Rect {
            x: k as f32 * bw + 6.0,
            y: hist_h - 18.0 - bh,
            w: bw - 12.0,
            h: bh,
            id: bucket_big[k].1,
            depth: 0,
        });
    }

    // Heatmap: one row per year, one column per month.
    let left = 46.0;
    let cw = (o.w - left - 8.0) / 12.0;
    let chh = ((heat_end - hist_h) - 26.0) / AGE_YEARS as f32;
    if cw > MIN_SIDE && chh > MIN_SIDE {
        for (k, &(bytes, _, id)) in cell.iter().enumerate() {
            if bytes == 0 || out.len() >= MAX_RECTS {
                continue;
            }
            let (r, c) = (k / 12, k % 12);
            out.push(Rect {
                x: left + c as f32 * cw + 1.0,
                y: hist_h + 18.0 + r as f32 * chh + 1.0,
                w: cw - 2.0,
                h: chh - 2.0,
                id,
                depth: 1,
            });
        }
    }

    // Big and untouched: files a year old or more, biggest first.
    let mut old: Vec<NodeId> = t
        .descendants(root)
        .filter(|&i| {
            let i = i as usize;
            !t.flags[i].is_dir() && t.mtime[i] != 0 && age_bucket(o.now, t.mtime[i]) >= 3
        })
        .collect();
    old.sort_unstable_by(|&a, &b| size_of(t, b, o.size).cmp(&size_of(t, a, o.size)).then(a.cmp(&b)));
    let rows = (((o.h - heat_end) - 24.0) / 20.0).floor().max(0.0) as usize;
    for (k, &id) in old.iter().take(rows).enumerate() {
        if out.len() >= MAX_RECTS || size_of(t, id, o.size) == 0 {
            break;
        }
        out.push(Rect {
            x: 0.0,
            y: heat_end + 22.0 + k as f32 * 20.0,
            w: o.w,
            h: 18.0,
            id,
            depth: 2,
        });
    }
}

// ---------------------------------------------------------------- invariants

fn debug_check(t: &Tree, view: View, o: &Opts, out: &[Rect]) {
    debug_assert!(out.len() <= MAX_RECTS, "rect budget blown: {}", out.len());
    if view.geom() != Geom::Cartesian {
        return;
    }
    for r in out {
        debug_assert!(r.w >= 0.0 && r.h >= 0.0, "negative rect {r:?}");
        debug_assert!(
            r.x >= -0.5 && r.y >= -0.5 && r.x + r.w <= o.w + 0.5 && r.y + r.h <= o.h + 0.5,
            "rect escapes canvas: {r:?} in {}x{}",
            o.w,
            o.h
        );
    }
    if view != View::Treemap {
        return;
    }

    // Every rect lies inside its nearest drawn ancestor.
    let mut pos = std::collections::HashMap::with_capacity(out.len());
    for (i, r) in out.iter().enumerate() {
        pos.insert(r.id, i);
    }
    for r in out {
        let mut p = t.parent[r.id as usize];
        while p != crate::NO_PARENT {
            if let Some(&i) = pos.get(&p) {
                let q = out[i];
                debug_assert!(
                    r.x >= q.x - 0.5
                        && r.y >= q.y - 0.5
                        && r.x + r.w <= q.x + q.w + 0.5
                        && r.y + r.h <= q.y + q.h + 0.5,
                    "{r:?} escapes parent {q:?}"
                );
                break;
            }
            p = t.parent[p as usize];
        }
    }

    // No two rects at the same depth overlap. O(n^2) per depth, so only run it
    // where the group is small enough to stay cheap in a debug build.
    let maxd = out.iter().map(|r| r.depth).max().unwrap_or(0);
    for d in 0..=maxd {
        let g: Vec<&Rect> = out.iter().filter(|r| r.depth == d).collect();
        if g.len() > 400 {
            continue;
        }
        for a in 0..g.len() {
            for b in a + 1..g.len() {
                let (p, q) = (g[a], g[b]);
                let overlap = p.x + p.w > q.x + 0.5
                    && q.x + q.w > p.x + 0.5
                    && p.y + p.h > q.y + 0.5
                    && q.y + q.h > p.y + 0.5;
                debug_assert!(!overlap, "depth {d} overlap: {p:?} vs {q:?}");
            }
        }
    }
}
