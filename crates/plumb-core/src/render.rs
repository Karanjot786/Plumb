//! CPU rasterizer. Layout and pixels both happen here, and the frontend does
//! one `putImageData`; see the design doc §6.2 for why that is the only thing
//! that behaves the same on WKWebView, WebView2 and WebKitGTK.
//!
//! Output is straight RGBA: every pixel is drawn opaque over an opaque ground,
//! so premultiplied and straight are the same bytes.

use crate::layout::{
    self, age_sections, layout, polar_center, size_of, Opts, Rect, View, AGE_BUCKETS, AGE_YEARS,
    HEADER,
};
use crate::{NodeId, Tree};
use std::collections::HashMap;
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, Rect as SkRect, Stroke,
    Transform,
};

// ------------------------------------------------------------------ palette

type Rgb = (u8, u8, u8);

// Dark ground, Apple dark-mode system colours. `PAPER` is the canvas and `INK`
// the text; every view dims by mixing toward `PAPER`, so dimming now darkens.
pub const PAPER: Rgb = (0x1C, 0x1C, 0x1E);
pub const INK: Rgb = (0xF2, 0xF2, 0xF7);
const FAINT: Rgb = (0x8E, 0x8E, 0x93);

const VIDEO: Rgb = (0xFF, 0x37, 0x5F);
const AUDIO: Rgb = (0x5E, 0x5C, 0xE6);
const IMAGE: Rgb = (0x30, 0xB0, 0xC7);
const DOCUMENT: Rgb = (0xFF, 0x9F, 0x0A);
const DEVELOPER: Rgb = (0x0A, 0x84, 0xFF);
const ARCHIVE: Rgb = (0xBF, 0x5A, 0xF2);
const OTHER: Rgb = (0x63, 0x63, 0x66);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Video,
    Audio,
    Image,
    Document,
    Developer,
    Archive,
    Other,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Video,
        Kind::Audio,
        Kind::Image,
        Kind::Document,
        Kind::Developer,
        Kind::Archive,
        Kind::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Audio => "audio",
            Kind::Image => "image",
            Kind::Document => "document",
            Kind::Developer => "developer",
            Kind::Archive => "archive",
            Kind::Other => "other",
        }
    }

    pub fn rgb(self) -> Rgb {
        match self {
            Kind::Video => VIDEO,
            Kind::Audio => AUDIO,
            Kind::Image => IMAGE,
            Kind::Document => DOCUMENT,
            Kind::Developer => DEVELOPER,
            Kind::Archive => ARCHIVE,
            Kind::Other => OTHER,
        }
    }

    pub fn css(self) -> String {
        let (r, g, b) = self.rgb();
        format!("#{r:02X}{g:02X}{b:02X}")
    }
}

pub fn kind_of(name: &str) -> Kind {
    let ext = match name.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() && e.len() <= 8 => e,
        _ => return Kind::Other,
    };
    let mut buf = [0u8; 8];
    let n = ext.len().min(8);
    buf[..n].copy_from_slice(&ext.as_bytes()[..n]);
    buf[..n].make_ascii_lowercase();
    match &buf[..n] {
        b"mp4" | b"mov" | b"mkv" | b"avi" | b"webm" | b"m4v" | b"wmv" | b"flv" | b"mpg"
        | b"mpeg" | b"m2ts" => Kind::Video,
        b"mp3" | b"wav" | b"flac" | b"aac" | b"m4a" | b"ogg" | b"aiff" | b"aif" | b"wma" => {
            Kind::Audio
        }
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"heic" | b"tiff" | b"tif" | b"bmp" | b"webp"
        | b"svg" | b"raw" | b"dng" | b"psd" | b"icns" => Kind::Image,
        b"pdf" | b"doc" | b"docx" | b"txt" | b"md" | b"rtf" | b"xls" | b"xlsx" | b"ppt"
        | b"pptx" | b"pages" | b"numbers" | b"key" | b"csv" | b"epub" => Kind::Document,
        b"rs" | b"go" | b"py" | b"js" | b"ts" | b"tsx" | b"jsx" | b"c" | b"h" | b"cpp" | b"hpp"
        | b"java" | b"kt" | b"swift" | b"rb" | b"php" | b"sh" | b"json" | b"toml" | b"yaml"
        | b"yml" | b"lock" | b"rlib" | b"dylib" | b"so" | b"jar" | b"wasm" | b"class" | b"o"
        | b"a" => Kind::Developer,
        b"zip" | b"tar" | b"gz" | b"tgz" | b"bz2" | b"xz" | b"7z" | b"rar" | b"dmg" | b"iso"
        | b"pkg" | b"deb" | b"rpm" | b"zst" => Kind::Archive,
        _ => Kind::Other,
    }
}

/// Fresh to stale, one entry per `layout::AGE_BUCKETS` bucket.
const AGE_RAMP: [Rgb; 6] = [
    (0x30, 0xD1, 0x58),
    (0xA3, 0xD9, 0x4B),
    (0xFF, 0xD6, 0x0A),
    (0xFF, 0x9F, 0x0A),
    (0xFF, 0x45, 0x3A),
    (0x9A, 0x6B, 0x8F),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorMode {
    ByType,
    ByFolder,
    ByAge,
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// Label colour that reads on `bg`: dark text on light fills (orange, yellow,
/// teal), light text on everything else.
fn ink_on(bg: Rgb) -> Rgb {
    let l = 0.2126 * bg.0 as f32 + 0.7152 * bg.1 as f32 + 0.0722 * bg.2 as f32;
    debug_assert!(PAPER.0 < 0x40 && INK.0 > 0xC0, "ink_on assumes a dark PAPER and a light INK");
    if l > 150.0 { PAPER } else { INK }
}

fn hsl(h: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let q = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (q(r), q(g), q(b))
}

fn color_of(t: &Tree, id: NodeId, mode: ColorMode, now: u32) -> Rgb {
    let i = id as usize;
    match mode {
        ColorMode::ByType => {
            if t.flags[i].is_dir() {
                mix(OTHER, PAPER, 0.35)
            } else {
                kind_of(t.name(id)).rgb()
            }
        }
        ColorMode::ByFolder => {
            // Mid saturation: vivid enough to separate siblings on the dark
            // ground, calm enough that type and age colours still read louder.
            let p = t.parent[i];
            let seed = if p == crate::NO_PARENT { id } else { p };
            let h = (seed.wrapping_mul(2_654_435_761) >> 8) % 360;
            hsl(h as f32, 0.45, 0.50)
        }
        ColorMode::ByAge => {
            let m = t.mtime[i];
            if m == 0 {
                OTHER
            } else {
                AGE_RAMP[layout::age_bucket(now, m)]
            }
        }
    }
}

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

// --------------------------------------------------------------------- font

/// 5x7 bitmap font, one byte per column, bit 0 = top row, ASCII 32..=126.
/// A built-in font is the only way to put text in the image without adding a
/// shaper; labels here are single-line names and sizes.
// Estimates for a 12 px system font. JS draws the real glyphs, so a few
// pixels of error here only change where a name gets its ".." cut.
const GLYPH_W: f32 = 6.5;
const GLYPH_H: f32 = 12.0;

pub struct Label { pub x: f32, pub y: f32, pub text: String, pub color: Rgb }

struct Canvas {
    pm: Pixmap,
    /// Device pixels per CSS pixel.
    s: f32,
    labels: Vec<Label>,
}

impl Canvas {
    fn dev(&self, v: f32) -> f32 {
        (v * self.s).round()
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgb, a: u8) {
        // Snap to device pixels; a treemap that ignores this is muddy.
        let (x0, y0) = (self.dev(x), self.dev(y));
        let (x1, y1) = (self.dev(x + w), self.dev(y + h));
        let Some(r) = SkRect::from_ltrb(x0, y0, x1.max(x0 + 1.0), y1.max(y0 + 1.0)) else {
            return;
        };
        let mut p = Paint::default();
        p.set_color_rgba8(c.0, c.1, c.2, a);
        p.anti_alias = false;
        self.pm.fill_rect(r, &p, Transform::identity(), None);
    }

    fn border(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgb, width: f32) {
        let (x0, y0) = (self.dev(x), self.dev(y));
        let (x1, y1) = (self.dev(x + w), self.dev(y + h));
        if x1 - x0 < 2.0 || y1 - y0 < 2.0 {
            return;
        }
        let i = width * self.s / 2.0;
        let mut pb = PathBuilder::new();
        pb.move_to(x0 + i, y0 + i);
        pb.line_to(x1 - i, y0 + i);
        pb.line_to(x1 - i, y1 - i);
        pb.line_to(x0 + i, y1 - i);
        pb.close();
        let Some(path) = pb.finish() else { return };
        let mut p = Paint::default();
        p.set_color_rgba8(c.0, c.1, c.2, 255);
        p.anti_alias = false;
        let stroke = Stroke { width: width * self.s, ..Default::default() };
        self.pm.stroke_path(&path, &p, &stroke, Transform::identity(), None);
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgb, a: u8) {
        let Some(path) = PathBuilder::from_circle(cx * self.s, cy * self.s, r * self.s) else {
            return;
        };
        let mut p = Paint::default();
        p.set_color_rgba8(c.0, c.1, c.2, a);
        p.anti_alias = true;
        self.pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
    }

    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: Rgb, w: f32) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0 * self.s, y0 * self.s);
        pb.line_to(x1 * self.s, y1 * self.s);
        let Some(path) = pb.finish() else { return };
        let mut p = Paint::default();
        p.set_color_rgba8(c.0, c.1, c.2, 255);
        p.anti_alias = true;
        let stroke = Stroke { width: w * self.s, ..Default::default() };
        self.pm.stroke_path(&path, &p, &stroke, Transform::identity(), None);
    }

    /// Annulus sector as a polyline. tiny-skia has no arc primitive and 4
    /// degree segments are indistinguishable at these radii.
    #[allow(clippy::too_many_arguments)]
    fn arc(&mut self, cx: f32, cy: f32, a0: f32, sweep: f32, r0: f32, r1: f32, c: Rgb, a: u8) {
        let n = ((sweep / 0.07).ceil() as usize).clamp(2, 512);
        let mut pb = PathBuilder::new();
        for k in 0..=n {
            let ang = a0 + sweep * k as f32 / n as f32;
            let (x, y) = (cx + r1 * ang.cos(), cy + r1 * ang.sin());
            if k == 0 {
                pb.move_to(x * self.s, y * self.s);
            } else {
                pb.line_to(x * self.s, y * self.s);
            }
        }
        for k in (0..=n).rev() {
            let ang = a0 + sweep * k as f32 / n as f32;
            let (x, y) = (cx + r0 * ang.cos(), cy + r0 * ang.sin());
            pb.line_to(x * self.s, y * self.s);
        }
        pb.close();
        let Some(path) = pb.finish() else { return };
        let mut p = Paint::default();
        p.set_color_rgba8(c.0, c.1, c.2, a);
        p.anti_alias = true;
        self.pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
    }

    fn text_w(&self, chars: usize) -> f32 { chars as f32 * GLYPH_W }

    fn line_h(&self) -> f32 { GLYPH_H }

    /// `x, y` is the top-left of the text box in CSS pixels. Recorded, not
    /// painted: the window draws it in the system font.
    fn text(&mut self, x: f32, y: f32, s: &str, c: Rgb) {
        self.labels.push(Label { x, y, text: s.to_string(), color: c });
    }
}

/// Longest prefix of `s` that fits `avail` CSS pixels, with a ".." marker when
/// it had to cut.
fn fit(_c: &Canvas, s: &str, avail: f32) -> Option<String> {
    let n = (avail / GLYPH_W).floor() as usize;
    if n < 3 {
        return None;
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return Some(s.to_string());
    }
    let mut out: String = chars[..n - 2].iter().collect();
    out.push_str("..");
    Some(out)
}

// ------------------------------------------------------------------- render

pub struct RenderOpts {
    pub view: View,
    pub color: ColorMode,
    /// Device pixel ratio.
    pub dpr: f32,
    /// Rects whose name does not contain this (lowercased) are dimmed.
    pub filter: String,
    pub lay: Opts,
}

pub fn px_size(o: &RenderOpts) -> (u32, u32) {
    let w = (o.lay.w * o.dpr).round().max(1.0) as u32;
    let h = (o.lay.h * o.dpr).round().max(1.0) as u32;
    (w.min(8192), h.min(8192))
}

pub fn render(t: &Tree, root: NodeId, o: &RenderOpts) -> (Vec<u8>, Vec<Rect>, Vec<Label>) {
    let (pw, ph) = px_size(o);
    let Some(mut pm) = Pixmap::new(pw, ph) else { return (Vec::new(), Vec::new(), Vec::new()) };
    pm.fill(Color::from_rgba8(PAPER.0, PAPER.1, PAPER.2, 255));
    let mut c = Canvas { pm, s: o.dpr, labels: Vec::new() };

    let rects = layout(t, root, o.view, &o.lay);
    let needle = o.filter.to_lowercase();
    let dim: Vec<bool> = if needle.is_empty() {
        vec![false; rects.len()]
    } else {
        rects.iter().map(|r| !t.name(r.id).to_lowercase().contains(&needle)).collect()
    };

    match o.view {
        View::Treemap => draw_treemap(&mut c, t, &rects, &dim, o),
        View::Folders => draw_folders(&mut c, t, &rects, &dim, o),
        View::Sunburst => draw_sunburst(&mut c, t, &rects, &dim, o),
        View::Flame => draw_flame(&mut c, t, &rects, &dim, o),
        View::Bubbles => draw_bubbles(&mut c, t, &rects, &dim, o),
        View::MindMap => draw_mind_map(&mut c, t, &rects, &dim, o),
        View::TopSizes => draw_top_sizes(&mut c, t, root, &rects, &dim, o),
        View::AgeMap => draw_age_map(&mut c, t, &rects, &dim, o),
    }

    (c.pm.take(), rects, c.labels)
}

fn fill_of(t: &Tree, r: &Rect, dim: bool, o: &RenderOpts) -> Rgb {
    let base = color_of(t, r.id, o.color, o.lay.now);
    if dim {
        mix(base, PAPER, 0.72)
    } else {
        base
    }
}

fn draw_treemap(c: &mut Canvas, t: &Tree, rects: &[Rect], dim: &[bool], o: &RenderOpts) {
    // A tile is a container when one of its children also got drawn; those get
    // a header strip with the name, leaves get their own label.
    let drawn: std::collections::HashSet<NodeId> = rects.iter().map(|r| r.id).collect();
    let container: Vec<bool> = rects
        .iter()
        .map(|r| t.flags[r.id as usize].is_dir() && t.children(r.id).any(|ch| drawn.contains(&ch)))
        .collect();

    for (i, r) in rects.iter().enumerate() {
        let f = fill_of(t, r, dim[i], o);
        if container[i] {
            c.rect(r.x, r.y, r.w, r.h, mix(f, PAPER, 0.55), 255);
            c.rect(r.x, r.y, r.w, HEADER.min(r.h), mix(f, INK, 0.06), 255);
        } else {
            c.rect(r.x, r.y, r.w, r.h, f, 255);
        }
        c.border(r.x, r.y, r.w, r.h, mix(f, PAPER, 0.55), 1.0);
    }
    for (i, r) in rects.iter().enumerate() {
        if r.w < 60.0 || r.h < 16.0 {
            continue;
        }
        let name = t.name(r.id);
        if container[i] {
            if let Some(s) = fit(c, name, r.w - 8.0) {
                let y = r.y + (HEADER - c.line_h()) / 2.0;
                c.text(r.x + 4.0, y, &s, INK);
            }
        } else if let Some(s) = fit(c, name, r.w - 8.0) {
            let f = fill_of(t, r, dim[i], o);
            let ink = ink_on(f);
            c.text(r.x + 4.0, r.y + 4.0, &s, ink);
            if r.h > 8.0 + c.line_h() * 2.0 {
                let size = human(size_of(t, r.id, o.lay.size));
                if let Some(sz) = fit(c, &size, r.w - 8.0) {
                    c.text(r.x + 4.0, r.y + 6.0 + c.line_h(), &sz, mix(ink, f, 0.35));
                }
            }
        }
    }
}

fn draw_folders(c: &mut Canvas, t: &Tree, rects: &[Rect], dim: &[bool], o: &RenderOpts) {
    for (i, r) in rects.iter().enumerate() {
        let f = fill_of(t, r, dim[i], o);
        c.rect(r.x, r.y, r.w, r.h, mix(f, PAPER, 0.62), 255);
        c.rect(r.x, r.y, r.w, r.h.min(4.0), f, 255);
        c.border(r.x, r.y, r.w, r.h, mix(f, PAPER, 0.30), 1.0);
        if r.w < 60.0 || r.h < 16.0 {
            continue;
        }
        if let Some(s) = fit(c, t.name(r.id), r.w - 12.0) {
            c.text(r.x + 6.0, r.y + 10.0, &s, INK);
        }
        if r.h > 14.0 + c.line_h() * 2.0 {
            let bytes = human(size_of(t, r.id, o.lay.size));
            if let Some(s) = fit(c, &bytes, r.w - 12.0) {
                c.text(r.x + 6.0, r.y + 12.0 + c.line_h(), &s, mix(INK, PAPER, 0.30));
            }
        }
    }
}

fn draw_sunburst(c: &mut Canvas, t: &Tree, rects: &[Rect], dim: &[bool], o: &RenderOpts) {
    let (cx, cy, rmax) = polar_center(&o.lay);
    for (i, r) in rects.iter().enumerate() {
        let f = fill_of(t, r, dim[i], o);
        if r.depth == 0 {
            c.circle(cx, cy, r.h, mix(f, PAPER, 0.45), 255);
            continue;
        }
        c.arc(cx, cy, r.x, r.w, r.y, r.y + r.h - 1.0, f, 255);
    }
    if let Some(root) = rects.first() {
        if let Some(s) = fit(c, t.name(root.id), rmax * 0.30) {
            let w = c.text_w(s.chars().count());
            c.text(cx - w / 2.0, cy - c.line_h() / 2.0, &s, INK);
        }
    }
}

fn draw_flame(c: &mut Canvas, t: &Tree, rects: &[Rect], dim: &[bool], o: &RenderOpts) {
    for (i, r) in rects.iter().enumerate() {
        let f = fill_of(t, r, dim[i], o);
        c.rect(r.x, r.y, r.w, r.h, f, 255);
        c.border(r.x, r.y, r.w, r.h, mix(f, PAPER, 0.50), 1.0);
        if r.w < 60.0 || r.h < 16.0 {
            continue;
        }
        if let Some(s) = fit(c, t.name(r.id), r.w - 8.0) {
            c.text(r.x + 4.0, r.y + (r.h - c.line_h()) / 2.0, &s, INK);
        }
    }
}

fn draw_bubbles(c: &mut Canvas, t: &Tree, rects: &[Rect], dim: &[bool], o: &RenderOpts) {
    let drawn: std::collections::HashSet<NodeId> = rects.iter().map(|r| r.id).collect();
    let container: Vec<bool> = rects
        .iter()
        .map(|r| t.flags[r.id as usize].is_dir() && t.children(r.id).any(|ch| drawn.contains(&ch)))
        .collect();

    for (i, r) in rects.iter().enumerate() {
        let f = fill_of(t, r, dim[i], o);
        let rad = r.w / 2.0;
        let (cx, cy) = (r.x + rad, r.y + rad);
        let inner = if t.flags[r.id as usize].is_dir() { mix(f, PAPER, 0.55) } else { f };
        c.circle(cx, cy, rad, mix(f, INK, 0.10), 255);
        c.circle(cx, cy, rad - 1.0, inner, 255);
    }
    // Labels last, or a nested bubble paints over its parent's name. A
    // container's name goes in the free cap above its children.
    for (i, r) in rects.iter().enumerate() {
        let rad = r.w / 2.0;
        if rad * 2.0 < 60.0 {
            continue;
        }
        let (cx, cy) = (r.x + rad, r.y + rad);
        let Some(s) = fit(c, t.name(r.id), rad * 1.5) else { continue };
        let w = c.text_w(s.chars().count());
        let y = if container[i] { cy - rad * 0.82 } else { cy - c.line_h() / 2.0 };
        let ink = if container[i] { INK } else { ink_on(fill_of(t, r, dim[i], o)) };
        c.text(cx - w / 2.0, y, &s, ink);
    }
}

fn draw_mind_map(c: &mut Canvas, t: &Tree, rects: &[Rect], dim: &[bool], o: &RenderOpts) {
    let mut at: HashMap<NodeId, (f32, f32)> = HashMap::with_capacity(rects.len());
    for r in rects {
        at.insert(r.id, (r.x + r.w / 2.0, r.y + r.h / 2.0));
    }
    let total = size_of(t, rects.first().map_or(0, |r| r.id), o.lay.size).max(1) as f64;
    for r in rects.iter().skip(1) {
        let mut p = t.parent[r.id as usize];
        while p != crate::NO_PARENT {
            if let Some(&(px, py)) = at.get(&p) {
                let share = size_of(t, r.id, o.lay.size) as f64 / total;
                let w = (0.6 + 5.0 * share.sqrt() as f32).min(6.0);
                c.line(px, py, r.x + r.w / 2.0, r.y + r.h / 2.0, mix(FAINT, PAPER, 0.45), w);
                break;
            }
            p = t.parent[p as usize];
        }
    }
    for (i, r) in rects.iter().enumerate() {
        let f = fill_of(t, r, dim[i], o);
        let rad = r.w / 2.0;
        c.circle(r.x + rad, r.y + rad, rad, f, 255);
        if rad < 7.0 {
            continue;
        }
        if let Some(s) = fit(c, t.name(r.id), 90.0) {
            c.text(r.x + r.w + 3.0, r.y + rad - c.line_h() / 2.0, &s, INK);
        }
    }
}

fn draw_top_sizes(
    c: &mut Canvas,
    t: &Tree,
    root: NodeId,
    rects: &[Rect],
    dim: &[bool],
    o: &RenderOpts,
) {
    let max = rects.iter().map(|r| size_of(t, r.id, o.lay.size)).max().unwrap_or(1).max(1);
    let scan = size_of(t, root, o.lay.size).max(1);
    for (i, r) in rects.iter().enumerate() {
        let bytes = size_of(t, r.id, o.lay.size);
        let f = fill_of(t, r, dim[i], o);
        c.rect(r.x, r.y, r.w, r.h, mix(PAPER, INK, 0.03), 255);
        let bw = ((r.w - 4.0) * (bytes as f32 / max as f32)).max(2.0);
        c.rect(r.x, r.y, bw, r.h, mix(f, PAPER, 0.15), 255);
        let label = format!("{}  {}", human(bytes), t.name(r.id));
        if let Some(s) = fit(c, &label, r.w - 90.0) {
            c.text(r.x + 6.0, r.y + (r.h - c.line_h()) / 2.0, &s, INK);
        }
        let pct = format!("{:.1}%", bytes as f64 * 100.0 / scan as f64);
        let w = c.text_w(pct.chars().count());
        c.text(r.x + r.w - w - 6.0, r.y + (r.h - c.line_h()) / 2.0, &pct, mix(INK, PAPER, 0.35));
    }
}

fn draw_age_map(c: &mut Canvas, t: &Tree, rects: &[Rect], dim: &[bool], o: &RenderOpts) {
    let (hist_h, heat_end) = age_sections(&o.lay);
    let now_y = layout::year_month(o.lay.now).0;

    c.text(0.0, 4.0, "AGE OF FILES", mix(INK, PAPER, 0.40));
    c.text(0.0, hist_h + 2.0, "MODIFIED BY MONTH", mix(INK, PAPER, 0.40));
    c.text(0.0, heat_end + 4.0, "BIG AND UNTOUCHED", mix(INK, PAPER, 0.40));

    let bw = o.lay.w / 6.0;
    for (k, (_, label)) in AGE_BUCKETS.iter().enumerate() {
        if let Some(s) = fit(c, label, bw - 8.0) {
            let w = c.text_w(s.chars().count());
            c.text(k as f32 * bw + (bw - w) / 2.0, hist_h - 14.0, &s, mix(INK, PAPER, 0.30));
        }
    }

    let chh = ((heat_end - hist_h) - 26.0) / AGE_YEARS as f32;
    if chh >= c.line_h() + 1.0 {
        for row in 0..AGE_YEARS {
            let y = hist_h + 18.0 + row as f32 * chh;
            let year = format!("{}", now_y - (AGE_YEARS as i32 - 1) + row as i32);
            c.text(4.0, y + (chh - c.line_h()) / 2.0, &year, mix(INK, PAPER, 0.35));
        }
    }

    let heat_max = rects
        .iter()
        .filter(|r| r.depth == 1)
        .map(|r| size_of(t, r.id, o.lay.size))
        .max()
        .unwrap_or(1)
        .max(1);

    for (i, r) in rects.iter().enumerate() {
        let f = fill_of(t, r, dim[i], o);
        match r.depth {
            0 => {
                let b = layout::age_bucket(o.lay.now, t.mtime[r.id as usize]);
                let col = if dim[i] { mix(AGE_RAMP[b], PAPER, 0.72) } else { AGE_RAMP[b] };
                c.rect(r.x, r.y, r.w, r.h, col, 255);
            }
            1 => {
                let v = (size_of(t, r.id, o.lay.size) as f32 / heat_max as f32).clamp(0.0, 1.0);
                let col = mix(PAPER, mix(AGE_RAMP[2], AGE_RAMP[4], v), v.sqrt());
                c.rect(r.x, r.y, r.w, r.h, if dim[i] { mix(col, PAPER, 0.6) } else { col }, 255);
            }
            _ => {
                let bytes = size_of(t, r.id, o.lay.size);
                c.rect(r.x, r.y, r.w, r.h, mix(PAPER, INK, 0.03), 255);
                c.rect(r.x, r.y, 3.0, r.h, f, 255);
                let days = o.lay.now.saturating_sub(t.mtime[r.id as usize]) / 86_400;
                let label = format!("{}  {}d  {}", human(bytes), days, t.name(r.id));
                if let Some(s) = fit(c, &label, r.w - 12.0) {
                    c.text(r.x + 8.0, r.y + (r.h - c.line_h()) / 2.0, &s, INK);
                }
            }
        }
    }
}
