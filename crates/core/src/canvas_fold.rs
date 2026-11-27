//! Pure canvas fold: a port of `apps/web/src/canvas/{select,oplog}.ts` (and the types of `ink.ts`).
//!
//! A month canvas is the fold of its append-only ink ops. Strokes and images append; a `delete` removes what
//! is inside a polygon; a `move` lifts what is inside a polygon and re-lands it (scaled / shifted) on top.
//! Strokes are cut at the polygon boundary; images are atomic (selected when the polygon holds their centre).
//! Undo / redo are ops naming a client id (`cid`); [`resolve_events`] applies them and [`fold`] folds the result.
//!
//! No I/O. Verified against the shared golden vectors (`apps/web/tests/canvas-vectors/vectors.json`, see
//! `tests/canvas_vectors.rs`), which the TypeScript reference and the Swift port also pass.
//!
//! Ergonomic entry points for the rest of the core:
//! * [`fold_wire_ops`]  - stored op JSON objects (with `cid`, in canvas order) -> folded [`Item`]s.
//! * [`fold_wire_records`] - `/api/ink`-style records -> folded [`Item`]s (canvas order applied for you).
//! * [`MonthLog`] - the full log with undo / redo bookkeeping (`merge`, `confirm`, `undo`, `redo`, ...).

use std::collections::{HashMap, HashSet};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

// ── types (ink.ts) ──────────────────────────────────────────────────────────

/// x, y (world), pressure 0-1, tilt 0-1.
pub type Pt = [f64; 4];
pub type Poly = Vec<[f64; 2]>;
pub type BBox = [f64; 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Pencil,
    Pen,
    Eraser,
}

/// Move/scale about (cx, cy): `p' = c + d + s*(p - c)`. No rotation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Xf {
    pub dx: f64,
    pub dy: f64,
    pub s: f64,
    pub cx: f64,
    pub cy: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    Pencil,
    Pen,
    Image,
}

impl Layer {
    pub fn name(self) -> &'static str {
        match self {
            Layer::Pencil => "pencil",
            Layer::Pen => "pen",
            Layer::Image => "image",
        }
    }
}

/// Which layers are visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layers {
    pub pencil: bool,
    pub pen: bool,
    pub image: bool,
}

impl Layers {
    pub const ALL: Layers = Layers { pencil: true, pen: true, image: true };
    pub fn get(&self, l: Layer) -> bool {
        match l {
            Layer::Pencil => self.pencil,
            Layer::Pen => self.pen,
            Layer::Image => self.image,
        }
    }
    /// Layers an edit touched, or None when all are visible (so every layer is affected).
    pub fn only_list(&self) -> Option<Vec<String>> {
        let v: Vec<String> = [Layer::Pencil, Layer::Pen, Layer::Image]
            .into_iter()
            .filter(|l| self.get(*l))
            .map(|l| l.name().to_string())
            .collect();
        if v.len() == 3 { None } else { Some(v) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrokeItem {
    /// Stable identity of this piece of ink: the op's id, or `<id>~<op>i<n>` once an op has cut / moved it.
    pub id: String,
    pub tool: Tool,
    pub seed: u32,
    pub size: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    pub pts: Vec<Pt>,
}

/// The constant `"tool": "image"` that distinguishes an image item from a stroke on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImageTag;
impl Serialize for ImageTag {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("image")
    }
}
impl<'de> Deserialize<'de> for ImageTag {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == "image" { Ok(ImageTag) } else { Err(de::Error::custom("not an image")) }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageItem {
    pub id: String,
    pub tool: ImageTag,
    pub hash: String,
    /// x, y, w, h in world units.
    pub rect: [f64; 4],
}

impl ImageItem {
    pub fn new(id: impl Into<String>, hash: impl Into<String>, rect: [f64; 4]) -> Self {
        ImageItem { id: id.into(), tool: ImageTag, hash: hash.into(), rect }
    }
}

/// What a folded month canvas holds, in draw order. Serialises to the same JSON as the TS `Item`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Item {
    Image(ImageItem),
    Stroke(StrokeItem),
}

impl Item {
    pub fn id(&self) -> &str {
        match self {
            Item::Stroke(s) => &s.id,
            Item::Image(i) => &i.id,
        }
    }
}

// ── wire ops (oplog.ts OpData) ──────────────────────────────────────────────

/// Seeds are uint32 but travel as JS numbers (1234.0 is fine); wrap like `>>> 0`.
fn de_seed<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = f64::deserialize(d)?;
    if !v.is_finite() {
        return Err(de::Error::custom("seed"));
    }
    let m = v.trunc() % 4294967296.0;
    let w = if m < 0.0 { m + 4294967296.0 } else { m };
    Ok(w as u32)
}

/// Points are 4 numbers; tolerate shorter legacy points (pressure 0.5, tilt 0) like the Swift codec.
fn de_pts<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Pt>, D::Error> {
    let raw = Vec::<Vec<f64>>::deserialize(d)?;
    raw.into_iter()
        .map(|n| {
            if n.len() < 2 {
                Err(de::Error::custom("point needs x, y"))
            } else {
                Ok([n[0], n[1], n.get(2).copied().unwrap_or(0.5), n.get(3).copied().unwrap_or(0.0)])
            }
        })
        .collect()
}

fn de_poly<'de, D: Deserializer<'de>>(d: D) -> Result<Poly, D::Error> {
    let raw = Vec::<Vec<f64>>::deserialize(d)?;
    raw.into_iter()
        .map(|n| if n.len() < 2 { Err(de::Error::custom("poly point needs x, y")) } else { Ok([n[0], n[1]]) })
        .collect()
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One op's data in its wire shape (without the `cid`). `serde_json::from_value` decodes it (unknown ops and
/// malformed ones are errors = ignored); `serde_json::to_value` encodes it, omitting optionals exactly as
/// the TS `toData` does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum OpData {
    Stroke {
        tool: Tool,
        #[serde(deserialize_with = "de_seed")]
        seed: u32,
        size: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opacity: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only: Option<Vec<String>>,
        #[serde(deserialize_with = "de_pts")]
        pts: Vec<Pt>,
    },
    Image {
        hash: String,
        rect: [f64; 4],
    },
    Move {
        #[serde(deserialize_with = "de_poly")]
        poly: Poly,
        xf: Xf,
        #[serde(default, skip_serializing_if = "is_false")]
        copy: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<String>>,
    },
    Delete {
        #[serde(deserialize_with = "de_poly")]
        poly: Poly,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        only: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<String>>,
    },
    Undo {
        target: String,
    },
    Redo {
        target: String,
    },
}

impl OpData {
    /// strokes, images, moves and deletes draw; undo / redo don't.
    pub fn is_drawable(&self) -> bool {
        !matches!(self, OpData::Undo { .. } | OpData::Redo { .. })
    }

    /// Decode a wire object (a `cid` key is ignored). None for unknown / malformed ops.
    pub fn from_wire(m: &Map<String, Value>) -> Option<OpData> {
        serde_json::from_value(Value::Object(m.clone())).ok()
    }

    /// The wire object for this op, with the cid attached (`toWire`).
    pub fn to_wire(&self, cid: &str) -> Value {
        let mut v = serde_json::to_value(self).expect("op serialises");
        v.as_object_mut().expect("object").insert("cid".into(), Value::String(cid.to_string()));
        v
    }
}

/// An in-memory event as `fold` reads it: an op's data plus its id and whether undo has left it active.
/// (The id is the op's cid; empty falls back to `s<n>` / `e<n>`, as the TS does for an undefined id.)
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Ev {
    #[serde(default)]
    pub id: String,
    pub data: OpData,
    #[serde(default = "yes")]
    pub active: bool,
}

fn yes() -> bool {
    true
}

impl Ev {
    pub fn new(id: impl Into<String>, active: bool, data: OpData) -> Ev {
        Ev { id: id.into(), data, active }
    }
}

// ── geometry ────────────────────────────────────────────────────────────────

pub fn layer_of(it: &Item) -> Layer {
    match it {
        Item::Image(_) => Layer::Image,
        Item::Stroke(s) => match s.tool {
            Tool::Pen => Layer::Pen,
            _ => Layer::Pencil,
        },
    }
}

/// Point in polygon (even-odd).
pub fn pip(poly: &[[f64; 2]], x: f64, y: f64) -> bool {
    let mut c = false;
    let n = poly.len();
    if n == 0 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let a = poly[i];
        let b = poly[j];
        if (a[1] > y) != (b[1] > y) && x < ((b[0] - a[0]) * (y - a[1])) / (b[1] - a[1]) + a[0] {
            c = !c;
        }
        j = i;
    }
    c
}

fn lerp_pt(a: &Pt, b: &Pt, t: f64) -> Pt {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, a[3] + (b[3] - a[3]) * t]
}

fn crossings(a: &Pt, b: &Pt, poly: &[[f64; 2]]) -> Vec<f64> {
    let mut ts = Vec::new();
    let rx = b[0] - a[0];
    let ry = b[1] - a[1];
    let n = poly.len();
    for i in 0..n {
        let c = poly[i];
        let d = poly[(i + 1) % n];
        let sx = d[0] - c[0];
        let sy = d[1] - c[1];
        let den = rx * sy - ry * sx;
        if den.abs() < 1e-12 {
            continue;
        }
        let t = ((c[0] - a[0]) * sy - (c[1] - a[1]) * sx) / den;
        let u = ((c[0] - a[0]) * ry - (c[1] - a[1]) * rx) / den;
        if t > 1e-9 && t < 1.0 - 1e-9 && (0.0..1.0).contains(&u) {
            ts.push(t);
        }
    }
    ts.sort_by(|p, q| p.partial_cmp(q).unwrap_or(std::cmp::Ordering::Equal));
    ts
}

/// Result of [`split_by_poly`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Split {
    pub ins: Vec<Vec<Pt>>,
    pub outs: Vec<Vec<Pt>>,
}

/// Split a polyline at the polygon boundary: pieces inside / outside, with interpolated points
/// (pressure, tilt too) at the cuts.
pub fn split_by_poly(pts: &[Pt], poly: &[[f64; 2]]) -> Split {
    let mut ins: Vec<Vec<Pt>> = Vec::new();
    let mut outs: Vec<Vec<Pt>> = Vec::new();
    if pts.len() == 1 {
        if pip(poly, pts[0][0], pts[0][1]) { ins.push(pts.to_vec()) } else { outs.push(pts.to_vec()) }
        return Split { ins, outs };
    }
    let mut cur: Option<Vec<Pt>> = None;
    let mut cur_in = false;
    fn flush(cur: &mut Option<Vec<Pt>>, cur_in: bool, ins: &mut Vec<Vec<Pt>>, outs: &mut Vec<Vec<Pt>>) {
        if let Some(c) = cur.take() {
            if !c.is_empty() {
                if cur_in { ins.push(c) } else { outs.push(c) }
            }
        }
    }
    for w in pts.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let mut ts = vec![0.0];
        ts.extend(crossings(a, b, poly));
        ts.push(1.0);
        for k in 0..ts.len() - 1 {
            if ts[k + 1] - ts[k] < 1e-9 {
                continue;
            }
            let m = lerp_pt(a, b, (ts[k] + ts[k + 1]) / 2.0);
            let is_in = pip(poly, m[0], m[1]);
            if cur.is_none() || cur_in != is_in {
                flush(&mut cur, cur_in, &mut ins, &mut outs);
                cur = Some(vec![lerp_pt(a, b, ts[k])]);
                cur_in = is_in;
            }
            if let Some(c) = cur.as_mut() {
                c.push(lerp_pt(a, b, ts[k + 1]));
            }
        }
    }
    flush(&mut cur, cur_in, &mut ins, &mut outs);
    Split { ins, outs }
}

pub fn xf_xy(x: f64, y: f64, f: &Xf) -> [f64; 2] {
    [f.cx + f.dx + f.s * (x - f.cx), f.cy + f.dy + f.s * (y - f.cy)]
}

pub fn xf_poly(poly: &[[f64; 2]], f: &Xf) -> Poly {
    poly.iter().map(|p| xf_xy(p[0], p[1], f)).collect()
}

pub fn is_identity(f: &Xf) -> bool {
    f.dx == 0.0 && f.dy == 0.0 && f.s == 1.0
}

pub fn xf_item(it: &Item, f: &Xf) -> Item {
    match it {
        Item::Image(i) => {
            let [x, y, w, h] = i.rect;
            let [nx, ny] = xf_xy(x, y, f);
            Item::Image(ImageItem { rect: [nx, ny, w * f.s, h * f.s], ..i.clone() })
        }
        Item::Stroke(s) => Item::Stroke(xf_stroke(s, f)),
    }
}

fn xf_stroke(s: &StrokeItem, f: &Xf) -> StrokeItem {
    StrokeItem {
        pts: s
            .pts
            .iter()
            .map(|q| {
                let [x, y] = xf_xy(q[0], q[1], f);
                [x, y, q[2], q[3]]
            })
            .collect(),
        size: s.size * f.s,
        ..s.clone()
    }
}

fn anchor(i: &ImageItem) -> (f64, f64) {
    (i.rect[0] + i.rect[2] / 2.0, i.rect[1] + i.rect[3] / 2.0)
}

// ── erased ink ──────────────────────────────────────────────────────────────
// The eraser works on pixels, so a stroke that has been erased over is still all there in the log. Selecting
// or moving it must not resurrect the erased parts, so before a piece is selected or moved, the points the
// erasers visibly wiped are dropped from it. A point counts as erased once the erasers after it that pass
// within their radius have left under ERASED_BELOW of it, and only erasers drawn with that point's layer
// visible count.

pub const ERASED_BELOW: f64 = 0.2;
/// x size, world units (same as the dab).
pub const ERASER_RADIUS: f64 = 1.2;

#[derive(Debug, Clone)]
pub struct Eraser<'a> {
    pub it: &'a StrokeItem,
    pub r: f64,
    pub bbox: BBox,
}

fn pts_box(pts: &[Pt]) -> BBox {
    let (mut x0, mut y0, mut x1, mut y1) = (1e9_f64, 1e9_f64, -1e9_f64, -1e9_f64);
    for q in pts {
        x0 = x0.min(q[0]);
        y0 = y0.min(q[1]);
        x1 = x1.max(q[0]);
        y1 = y1.max(q[1]);
    }
    [x0, y0, x1, y1]
}

/// The erasers drawn after item `k` that touch `layer`.
pub fn erasers_after(items: &[Item], k: usize, layer: Layer) -> Vec<Eraser<'_>> {
    let mut out = Vec::new();
    for e in items.iter().skip(k + 1) {
        let Item::Stroke(e) = e else { continue };
        if e.tool != Tool::Eraser {
            continue;
        }
        if let Some(only) = &e.only {
            if !only.iter().any(|l| l == layer.name()) {
                continue;
            }
        }
        let r = e.size * ERASER_RADIUS;
        let b = pts_box(&e.pts);
        out.push(Eraser { it: e, r, bbox: [b[0] - r, b[1] - r, b[2] + r, b[3] + r] });
    }
    out
}

pub fn dist_to_path(x: f64, y: f64, pts: &[Pt]) -> f64 {
    if pts.len() == 1 {
        return (x - pts[0][0]).hypot(y - pts[0][1]);
    }
    let mut best = f64::INFINITY;
    for w in pts.windows(2) {
        let (ax, ay) = (w[0][0], w[0][1]);
        let dx = w[1][0] - ax;
        let dy = w[1][1] - ay;
        let l2 = dx * dx + dy * dy;
        let t = if l2 != 0.0 { (((x - ax) * dx + (y - ay) * dy) / l2).clamp(0.0, 1.0) } else { 0.0 };
        best = best.min((x - ax - t * dx).hypot(y - ay - t * dy));
    }
    best
}

/// A piece cut into the runs of points that haven't been erased away.
pub fn visible_runs(pts: &[Pt], all: &[Eraser<'_>]) -> Vec<Vec<Pt>> {
    if pts.is_empty() {
        return Vec::new();
    }
    let [x0, y0, x1, y1] = pts_box(pts);
    // only erasers that come near this piece matter
    let erasers: Vec<&Eraser> = all
        .iter()
        .filter(|e| e.bbox[0] <= x1 && e.bbox[2] >= x0 && e.bbox[1] <= y1 && e.bbox[3] >= y0)
        .collect();
    if erasers.is_empty() {
        return vec![pts.to_vec()];
    }
    // a straight stroke is just its two end points, so look along it: sample often enough to see a gap the
    // width of the narrowest eraser
    let min_r = erasers.iter().map(|e| e.r).fold(f64::INFINITY, f64::min);
    let step = (min_r * 0.4).max(0.5);
    let mut dense: Vec<Pt> = vec![pts[0]];
    for i in 1..pts.len() {
        let (a, b) = (&pts[i - 1], &pts[i]);
        let n = ((b[0] - a[0]).hypot(b[1] - a[1]) / step).ceil().min(400.0) as usize;
        for k in 1..=n {
            dense.push(lerp_pt(a, b, k as f64 / n as f64));
        }
    }
    let mut runs: Vec<Vec<Pt>> = Vec::new();
    let mut cur: Vec<Pt> = Vec::new();
    for q in &dense {
        let mut left = 1.0;
        for e in &erasers {
            if q[0] < e.bbox[0] || q[0] > e.bbox[2] || q[1] < e.bbox[1] || q[1] > e.bbox[3] {
                continue;
            }
            if dist_to_path(q[0], q[1], &e.it.pts) <= e.r {
                left *= 1.0 - e.it.op.unwrap_or(1.0);
            }
        }
        if left < ERASED_BELOW {
            if !cur.is_empty() {
                runs.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(*q);
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    if dense.len() > 1 {
        // a lone survivor of a cut is a speck, not a stroke
        runs.into_iter().filter(|r| r.len() > 1).collect()
    } else {
        runs
    }
}

// ── fold ────────────────────────────────────────────────────────────────────

/// Fold events (then `extra`, e.g. unconfirmed local ones) into the drawn items, in order.
pub fn fold(evs: &[Ev], extra: &[Ev]) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    let mut n = 0usize;
    for ev in evs.iter().chain(extra.iter()) {
        if !ev.active || !ev.data.is_drawable() {
            continue;
        }
        n += 1;
        let id = |prefix: &str| if ev.id.is_empty() { format!("{prefix}{n}") } else { ev.id.clone() };
        let (poly, xf, copy, only, ids, is_move) = match &ev.data {
            OpData::Stroke { tool, seed, size, opacity, only, pts } => {
                items.push(Item::Stroke(StrokeItem {
                    id: id("s"),
                    tool: *tool,
                    seed: *seed,
                    size: *size,
                    op: *opacity,
                    only: only.clone(),
                    pts: pts.clone(),
                }));
                continue;
            }
            OpData::Image { hash, rect } => {
                items.push(Item::Image(ImageItem::new(id("s"), hash.clone(), *rect)));
                continue;
            }
            OpData::Move { poly, xf, copy, only, ids } => (poly, Some(xf), *copy, only, ids, true),
            OpData::Delete { poly, only, ids } => (poly, None, false, only, ids, false),
            _ => continue,
        };
        let ev_id = id("e");
        let scope: Option<HashSet<&str>> = ids.as_ref().map(|v| v.iter().map(|s| s.as_str()).collect());
        let mut keep: Vec<Item> = Vec::new();
        let mut moved: Vec<Item> = Vec::new();
        let mut piece_no: u64 = 0;
        for (k, it) in items.iter().enumerate() {
            if let Item::Stroke(s) = it {
                if s.tool == Tool::Eraser {
                    keep.push(it.clone()); // erasers are subtractive history, not selectable ink
                    continue;
                }
            }
            let out_of_layer = only.as_ref().is_some_and(|o| !o.iter().any(|l| l == layer_of(it).name()));
            let out_of_scope = scope.as_ref().is_some_and(|s| !s.contains(it.id()));
            if out_of_layer || out_of_scope {
                keep.push(it.clone()); // edit was made with that layer hidden / item isn't part of the selection
                continue;
            }
            match it {
                Item::Image(img) => {
                    let (ax, ay) = anchor(img);
                    if !pip(poly, ax, ay) {
                        keep.push(it.clone());
                        continue;
                    }
                    if is_move {
                        if copy {
                            keep.push(it.clone());
                        }
                        let mut m = xf_item(it, xf.expect("move has xf"));
                        if let Item::Image(mi) = &mut m {
                            mi.id = format!("{}~{}i{}", img.id, ev_id, piece_no);
                        }
                        piece_no += 1;
                        moved.push(m);
                    }
                }
                Item::Stroke(s) => {
                    let Split { ins, outs } = split_by_poly(&s.pts, poly);
                    if ins.is_empty() {
                        keep.push(it.clone());
                        continue;
                    }
                    if is_move {
                        let gone = erasers_after(&items, k, layer_of(it));
                        // what's actually there to move
                        let inside: Vec<Vec<Pt>> = ins.iter().flat_map(|p| visible_runs(p, &gone)).collect();
                        if copy {
                            keep.push(it.clone());
                        } else {
                            for (i, p) in outs.into_iter().enumerate() {
                                keep.push(Item::Stroke(StrokeItem {
                                    id: format!("{}~{}o{}", s.id, ev_id, i),
                                    pts: p,
                                    ..s.clone()
                                }));
                            }
                        }
                        for p in inside {
                            let mut m = xf_stroke(&StrokeItem { pts: p, ..s.clone() }, xf.expect("move has xf"));
                            // id uses pieceNo BEFORE the increment, the seed AFTER (>>> 0)
                            m.id = format!("{}~{}i{}", s.id, ev_id, piece_no);
                            piece_no += 1;
                            m.seed = ((s.seed as u64).wrapping_add(977u64.wrapping_mul(piece_no)) & 0xFFFF_FFFF) as u32;
                            moved.push(Item::Stroke(m));
                        }
                    } else {
                        for (i, p) in outs.into_iter().enumerate() {
                            keep.push(Item::Stroke(StrokeItem {
                                id: format!("{}~{}o{}", s.id, ev_id, i),
                                pts: p,
                                ..s.clone()
                            }));
                        }
                    }
                }
            }
        }
        keep.extend(moved); // moved pieces land on top (after any earlier erasers)
        items = keep;
    }
    items
}

pub fn bbox_of(items: &[Item]) -> BBox {
    let (mut x0, mut y0, mut x1, mut y1) = (1e9_f64, 1e9_f64, -1e9_f64, -1e9_f64);
    for it in items {
        match it {
            Item::Image(i) => {
                let [x, y, w, h] = i.rect;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + w);
                y1 = y1.max(y + h);
            }
            Item::Stroke(s) => {
                let m = s.size * 0.6;
                for q in &s.pts {
                    x0 = x0.min(q[0] - m);
                    y0 = y0.min(q[1] - m);
                    x1 = x1.max(q[0] + m);
                    y1 = y1.max(q[1] + m);
                }
            }
        }
    }
    [x0, y0, x1, y1]
}

/// What [`pieces_in`] selected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pieces {
    pub pieces: Vec<Item>,
    /// The items the pieces came from (what a move / delete op is scoped to).
    pub ids: Vec<String>,
}

/// What a polygon selects: the visible ink inside it (strokes cut at the boundary and minus anything erased,
/// images by centre); hidden layers can't be selected.
pub fn pieces_in(items: &[Item], poly: &[[f64; 2]], layers: &Layers) -> Pieces {
    let mut pieces = Vec::new();
    let mut ids = Vec::new();
    for (k, it) in items.iter().enumerate() {
        if let Item::Stroke(s) = it {
            if s.tool == Tool::Eraser {
                continue;
            }
        }
        if !layers.get(layer_of(it)) {
            continue;
        }
        match it {
            Item::Image(img) => {
                let (ax, ay) = anchor(img);
                if pip(poly, ax, ay) {
                    pieces.push(it.clone());
                    ids.push(img.id.clone());
                }
            }
            Item::Stroke(s) => {
                let gone = erasers_after(items, k, layer_of(it));
                let mut any = false;
                for p in split_by_poly(&s.pts, poly).ins {
                    for run in visible_runs(&p, &gone) {
                        pieces.push(Item::Stroke(StrokeItem { pts: run, ..s.clone() }));
                        any = true;
                    }
                }
                if any {
                    ids.push(s.id.clone());
                }
            }
        }
    }
    Pieces { pieces, ids }
}

/// 2D Ramer-Douglas-Peucker for lasso paths.
pub fn simplify_poly(pts: &[[f64; 2]], tol: f64) -> Poly {
    let n = pts.len();
    if n < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((a, b)) = stack.pop() {
        let (pa, pb) = (pts[a], pts[b]);
        let dx = pb[0] - pa[0];
        let dy = pb[1] - pa[1];
        let l2 = dx * dx + dy * dy;
        let mut md = 0.0;
        let mut mi: Option<usize> = None;
        for i in a + 1..b {
            let t = if l2 != 0.0 { (((pts[i][0] - pa[0]) * dx + (pts[i][1] - pa[1]) * dy) / l2).clamp(0.0, 1.0) } else { 0.0 };
            let d = (pts[i][0] - pa[0] - t * dx).hypot(pts[i][1] - pa[1] - t * dy);
            if d > md {
                md = d;
                mi = Some(i);
            }
        }
        if let Some(mi) = mi {
            if md > tol && mi > 0 {
                keep[mi] = true;
                stack.push((a, mi));
                stack.push((mi, b));
            }
        }
    }
    pts.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

// ── op log (oplog.ts) ───────────────────────────────────────────────────────

/// One op in the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Op {
    pub cid: String,
    /// Author device id.
    pub dev: String,
    /// recorded_at once confirmed; creation time while pending.
    pub t: f64,
    /// Server event id; None while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eid: Option<String>,
    pub data: OpData,
}

/// One ink event as `/api/ink` returns it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WireRecord {
    pub event_id: String,
    pub device_id: String,
    pub recorded_at: f64,
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub data: Map<String, Value>,
}

/// Canvas order: `t`, then event id. (TS uses `localeCompare`; this is plain byte order, which agrees for
/// same-case ASCII ids - ULIDs / PocketBase ids / lowercase hex.)
fn canvas_cmp(a: &Op, b: &Op) -> std::cmp::Ordering {
    a.t.partial_cmp(&b.t)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.eid.as_deref().unwrap_or("").cmp(b.eid.as_deref().unwrap_or("")))
}

/// Which drawable ops are active after applying undo / redo ops in order (cid -> active).
pub fn resolve_active(ops: &[Op]) -> HashMap<String, bool> {
    let mut active: HashMap<String, bool> = HashMap::new();
    for o in ops {
        if o.data.is_drawable() {
            active.insert(o.cid.clone(), true);
        }
    }
    for o in ops {
        match &o.data {
            OpData::Undo { target } if active.contains_key(target) => {
                active.insert(target.clone(), false);
            }
            OpData::Redo { target } if active.contains_key(target) => {
                active.insert(target.clone(), true);
            }
            _ => {}
        }
    }
    active
}

/// The `Ev` list for [`fold`], with undo / redo applied. `ops` must already be in canvas order.
pub fn resolve_events(ops: &[Op]) -> Vec<Ev> {
    let active = resolve_active(ops);
    ops.iter()
        .filter(|o| o.data.is_drawable())
        .map(|o| Ev { id: o.cid.clone(), data: o.data.clone(), active: active.get(&o.cid).copied().unwrap_or(true) })
        .collect()
}

/// A month's canvas as an append-only log of ops. Time and cids are injected (`add_with`) so it is
/// deterministic; `add` uses the wall clock and a fresh uuid.
#[derive(Debug, Clone)]
pub struct MonthLog {
    pub month: String,
    pub me: String,
    pub ops: Vec<Op>,
    /// Bumped on every change.
    pub version: u64,
}

impl MonthLog {
    pub fn new(month: impl Into<String>, me: impl Into<String>) -> Self {
        MonthLog { month: month.into(), me: me.into(), ops: Vec::new(), version: 0 }
    }

    pub fn restore(month: impl Into<String>, me: impl Into<String>, ops: Vec<Op>) -> Self {
        let mut l = MonthLog::new(month, me);
        l.ops = ops;
        l.resort();
        l
    }

    fn resort(&mut self) {
        let (mut confirmed, pending): (Vec<Op>, Vec<Op>) = std::mem::take(&mut self.ops).into_iter().partition(|o| o.eid.is_some());
        confirmed.sort_by(canvas_cmp); // stable, like JS
        confirmed.extend(pending);
        self.ops = confirmed;
        self.version += 1;
    }

    pub fn pending(&self) -> Vec<&Op> {
        self.ops.iter().filter(|o| o.eid.is_none()).collect()
    }

    /// Append a local op with an explicit cid and creation time. Returns the cid.
    pub fn add_with(&mut self, data: OpData, cid: impl Into<String>, t: f64) -> String {
        let cid = cid.into();
        self.ops.push(Op { cid: cid.clone(), dev: self.me.clone(), t, eid: None, data });
        self.version += 1;
        cid
    }

    /// Append a local op (fresh uuid cid, wall clock ms).
    pub fn add(&mut self, data: OpData) -> String {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        self.add_with(data, uuid::Uuid::now_v7().to_string(), t)
    }

    /// The server stored our op: stamp it.
    pub fn confirm(&mut self, cid: &str, eid: &str, t: f64) {
        let Some(o) = self.ops.iter_mut().find(|x| x.cid == cid) else { return };
        if o.eid.is_some() {
            return;
        }
        o.eid = Some(eid.to_string());
        o.t = t;
        self.resort();
    }

    /// Fold server records in. Returns whether anything visible changed. Unknown or malformed ops are skipped.
    pub fn merge(&mut self, records: &[WireRecord]) -> bool {
        let mut changed = false;
        for r in records {
            let Some(Value::String(opn)) = r.data.get("op") else { continue };
            if !matches!(opn.as_str(), "stroke" | "image" | "move" | "delete" | "undo" | "redo") {
                continue;
            }
            // ops from clients that predate cids use the event id
            let cid = match r.data.get("cid") {
                Some(Value::String(c)) => c.clone(),
                _ => r.event_id.clone(),
            };
            if let Some(have) = self.ops.iter_mut().find(|o| o.cid == cid) {
                if have.eid.is_none() {
                    have.eid = Some(r.event_id.clone());
                    have.t = r.recorded_at;
                    have.dev = r.device_id.clone();
                    changed = true;
                }
                continue;
            }
            let Some(data) = OpData::from_wire(&r.data) else { continue };
            self.ops.push(Op { cid, dev: r.device_id.clone(), t: r.recorded_at, eid: Some(r.event_id.clone()), data });
            changed = true;
        }
        if changed {
            self.resort();
        }
        changed
    }

    /// Ev list for [`fold`], with undo / redo applied.
    pub fn events(&self) -> Vec<Ev> {
        resolve_events(&self.ops)
    }

    /// The folded canvas.
    pub fn items(&self) -> Vec<Item> {
        fold(&self.events(), &[])
    }

    fn own_active_drawables(&self) -> Vec<&Op> {
        let active = resolve_active(&self.ops);
        self.ops
            .iter()
            .filter(|o| o.dev == self.me && o.data.is_drawable() && active.get(&o.cid).copied().unwrap_or(false))
            .collect()
    }

    pub fn can_undo(&self) -> bool {
        !self.own_active_drawables().is_empty()
    }

    /// Undo my most recent active drawing: appends an undo op (returns its cid).
    pub fn undo(&mut self) -> Option<String> {
        let target = self.own_active_drawables().last()?.cid.clone();
        Some(self.add(OpData::Undo { target }))
    }

    /// Like `undo` with an explicit cid / time (deterministic).
    pub fn undo_with(&mut self, cid: impl Into<String>, t: f64) -> Option<String> {
        let target = self.own_active_drawables().last()?.cid.clone();
        Some(self.add_with(OpData::Undo { target }, cid, t))
    }

    /// My undone ops, most recent last, that a redo could bring back: replay my ops; any new drawing ends the chain.
    fn redo_stack(&self) -> Vec<String> {
        let mut stack: Vec<String> = Vec::new();
        for o in self.ops.iter().filter(|o| o.dev == self.me) {
            match &o.data {
                OpData::Undo { target } => stack.push(target.clone()),
                OpData::Redo { target } => {
                    if let Some(i) = stack.iter().rposition(|s| s == target) {
                        stack.remove(i);
                    }
                }
                _ => stack.clear(),
            }
        }
        stack
    }

    fn redo_target(&self) -> Option<String> {
        let top = self.redo_stack().pop()?;
        (resolve_active(&self.ops).get(&top) == Some(&false)).then_some(top)
    }

    pub fn can_redo(&self) -> bool {
        self.redo_target().is_some()
    }

    pub fn redo(&mut self) -> Option<String> {
        let target = self.redo_target()?;
        Some(self.add(OpData::Redo { target }))
    }

    pub fn redo_with(&mut self, cid: impl Into<String>, t: f64) -> Option<String> {
        let target = self.redo_target()?;
        Some(self.add_with(OpData::Redo { target }, cid, t))
    }

    /// After learning this device's real id, claim the ops made before we knew it.
    pub fn adopt_device(&mut self, id: &str) {
        if id == self.me {
            return;
        }
        for o in self.ops.iter_mut().filter(|o| o.dev == self.me) {
            o.dev = id.to_string();
        }
        self.me = id.to_string();
        self.version += 1;
    }

    pub fn has_content(&self) -> bool {
        self.events().iter().any(|e| e.active)
    }

    /// The wire shape of every op (data plus cid).
    pub fn wire(&self) -> Vec<Value> {
        self.ops.iter().map(|o| o.data.to_wire(&o.cid)).collect()
    }
}

// ── convenience entry points for the core ───────────────────────────────────

/// Stored ink ops (JSON objects with `cid`, e.g. `{"op":"stroke","tool":"pen","seed":1,"size":2,"pts":[[x,y,p,t]],"cid":"..."}`)
/// given in canvas order -> undo / redo resolved -> folded items. Unknown / malformed entries are ignored; a
/// missing `cid` falls back to `n<index>`.
pub fn fold_wire_ops(ops: &[Value]) -> Vec<Item> {
    let parsed: Vec<Op> = ops
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let m = v.as_object()?;
            let data = OpData::from_wire(m)?;
            let cid = m.get("cid").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("n{i}"));
            Some(Op { cid, dev: String::new(), t: 0.0, eid: None, data })
        })
        .collect();
    fold(&resolve_events(&parsed), &[])
}

/// `/api/ink`-style records (any order; duplicates by cid collapse) -> items, with canvas order applied.
pub fn fold_wire_records(records: &[WireRecord]) -> Vec<Item> {
    CanvasLog::from_records("", "", records).items()
}

/// A canvas: one period's operation log, folded on demand.
#[derive(Debug, Clone)]
pub struct CanvasLog {
    pub log: MonthLog,
}
impl CanvasLog {
    pub fn from_records(period: &str, me: &str, records: &[WireRecord]) -> Self {
        let mut log = MonthLog::new(period, me);
        log.merge(records);
        Self { log }
    }
    pub fn items(&self) -> Vec<Item> {
        fold(&self.log.events(), &[])
    }
}
