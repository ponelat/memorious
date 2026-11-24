//! Platform-independent canvas coverage. Same Gaussian dabs and Catmull-Rom
//! replay as the web renderer; pixels carry coverage, never journal state.
use crate::canvas_fold::{Item, Layers, Pt, StrokeItem, Tool};

pub struct InkBuffers {
    pub width: usize,
    pub height: usize,
    pub pencil: Vec<f32>,
    pub pen: Vec<f32>,
    pub opacity: Vec<f32>,
    pub view: [f64; 3],
    prev: Vec<f32>,
    sid: Vec<u32>,
    seq: u32,
}
impl InkBuffers {
    pub fn new(width: usize, height: usize) -> Self {
        let n = width * height;
        Self {
            width,
            height,
            pencil: vec![0.; n],
            pen: vec![0.; n],
            opacity: vec![0.; n],
            prev: vec![0.; n],
            sid: vec![0; n],
            seq: 0,
            view: [0., 0., 1.],
        }
    }
    pub fn replay(&mut self, items: &[Item], view: [f64; 3]) {
        self.pencil.fill(0.);
        self.pen.fill(0.);
        self.opacity.fill(0.);
        self.sid.fill(0);
        self.seq = 0;
        self.view = view;
        for it in items {
            if let Item::Stroke(s) = it {
                self.stroke(s);
            }
        }
    }
    fn gaussian(
        &mut self,
        center: [f64; 2],
        sigma: [f64; 2],
        angle: f64,
        a: f64,
        pen: bool,
        op: f64,
    ) {
        let [cx, cy] = center;
        let [s1, s2] = sigma;
        if ![cx, cy, s1, s2, angle, a, op].iter().all(|v| v.is_finite()) || s1 <= 0. || s2 <= 0. {
            return;
        }
        let c = angle.cos();
        let s = angle.sin();
        let v1 = s1 * s1;
        let v2 = s2 * s2;
        let hx = 3. * (v1 * c * c + v2 * s * s).sqrt();
        let hy = 3. * (v1 * s * s + v2 * c * c).sqrt();
        let x0 = (cx - hx - 1.).floor().max(0.) as usize;
        let x1 = (cx + hx + 1.).ceil().min(self.width as f64 - 1.) as usize;
        let y0 = (cy - hy - 1.).floor().max(0.) as usize;
        let y1 = (cy + hy + 1.).ceil().min(self.height as f64 - 1.) as usize;
        if x0 >= self.width || y0 >= self.height {
            return;
        }
        let ia = c * c / v1 + s * s / v2;
        let ic = s * s / v1 + c * c / v2;
        let ib = 2. * c * s * (1. / v1 - 1. / v2);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f64 + 0.5 - cx;
                let dy = y as f64 + 0.5 - cy;
                let q = ia * dx * dx + ib * dx * dy + ic * dy * dy;
                if q >= 9. {
                    continue;
                }
                let k = (q * 1024. / 9.) as usize;
                let w = (((-(k as f64) / 1024. * 4.5).exp() - (-4.5_f64).exp())
                    / (1. - (-4.5_f64).exp())) as f32;
                let i = y * self.width + x;
                if pen {
                    self.pen[i] = (self.pen[i] as f64 + a * w as f64) as f32;
                    if op > 0. && w > 0.3 {
                        if self.sid[i] != self.seq {
                            self.sid[i] = self.seq;
                            self.prev[i] = self.opacity[i];
                        }
                        let t = (1. - (1. - self.prev[i] as f64) * (1. - op)) as f32;
                        self.opacity[i] = self.opacity[i].max(t);
                    }
                } else {
                    self.pencil[i] = (self.pencil[i] as f64 + a * w as f64) as f32;
                }
            }
        }
    }
    fn erase(&mut self, center: [f64; 2], r: f64, a: f64, only: &Option<Vec<String>>) {
        let [cx, cy] = center;
        let ep = only
            .as_ref()
            .is_none_or(|v| v.iter().any(|s| s == "pencil"));
        let en = only.as_ref().is_none_or(|v| v.iter().any(|s| s == "pen"));
        let x0 = (cx - r - 1.).floor().max(0.) as usize;
        let x1 = (cx + r + 1.).ceil().min(self.width as f64 - 1.) as usize;
        let y0 = (cy - r - 1.).floor().max(0.) as usize;
        let y1 = (cy + r + 1.).ceil().min(self.height as f64 - 1.) as usize;
        if x0 >= self.width || y0 >= self.height {
            return;
        }
        for y in y0..=y1 {
            for x in x0..=x1 {
                let d = (x as f64 + 0.5 - cx).hypot(y as f64 + 0.5 - cy);
                let w = (r - d + 0.5).clamp(0., 1.);
                let k = 1. - a * w;
                let i = y * self.width + x;
                if ep {
                    self.pencil[i] = (self.pencil[i] as f64 * k) as f32;
                }
                if en {
                    self.pen[i] = (self.pen[i] as f64 * k) as f32;
                    self.opacity[i] = (self.opacity[i] as f64 * k) as f32;
                }
            }
        }
    }
    pub fn stroke(&mut self, st: &StrokeItem) {
        if st.pts.is_empty()
            || !st.size.is_finite()
            || st.size <= 0.
            || st.pts.iter().flatten().any(|x| !x.is_finite())
        {
            return;
        }
        self.seq = self.seq.wrapping_add(1);
        let mut stamper = Stamper {
            b: self,
            st,
            seed: st.seed,
            next: 0.,
            started: false,
            tangent: 0.,
        };
        let mut pts = vec![st.pts[0]];
        pts.extend_from_slice(&st.pts);
        pts.push(*st.pts.last().unwrap());
        for seg in pts.windows(4) {
            stamper.segment(seg);
        }
        let last = *st.pts.last().unwrap();
        if !stamper.started {
            stamper.stamp(last);
        }
        if st.tool == Tool::Pen {
            stamper.pen(last, true);
        }
    }
    pub fn alpha(&self, layers: Layers) -> Vec<f32> {
        self.pencil
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                let d = self.pen[i] as f64;
                let t = ((d - 0.42) / 0.16).clamp(0., 1.);
                let a = if layers.pen {
                    t * t * (3. - 2. * t) * self.opacity[i] as f64
                } else {
                    0.
                };
                let p = p as f64;
                let b = if layers.pencil && p > 0.002 {
                    0.9 * p / (p + 0.4) * if p < 0.15 { p * 6.67 } else { 1. }
                } else {
                    0.
                };
                (1. - (1. - a) * (1. - b)) as f32
            })
            .collect()
    }
}
struct Stamper<'a> {
    b: &'a mut InkBuffers,
    st: &'a StrokeItem,
    seed: u32,
    next: f64,
    started: bool,
    tangent: f64,
}
impl Stamper<'_> {
    fn screen(&self, q: Pt) -> [f64; 2] {
        [
            (q[0] - self.b.view[0]) * self.b.view[2],
            (q[1] - self.b.view[1]) * self.b.view[2],
        ]
    }
    fn pen(&mut self, q: Pt, cap: bool) {
        let r = self.b.view[2] * self.st.size * 0.5 * (0.4 + 0.6 * q[2].max(0.02).sqrt());
        let s2 = r / 1.177;
        let s1 = s2 * 1.6;
        let sp = (0.25 * s1).max(0.35);
        self.b.gaussian(
            self.screen(q),
            if cap { [s2, s2] } else { [s1, s2] },
            if cap { 0. } else { self.tangent },
            if cap { 0.5 } else { sp / (2.507 * s1) },
            true,
            self.st.op.unwrap_or(1.),
        );
        if !cap {
            self.next = sp;
            self.started = true;
        }
    }
    fn stamp(&mut self, q: Pt) {
        match self.st.tool {
            Tool::Pencil => {
                let s = self.b.view[2] * self.st.size * 0.5 * (0.55 + 0.45 * q[2]) / 2.3;
                let reference = 0.32 * s;
                let sp = reference.max(0.35);
                let flow = 0.27 * (0.1 + 0.9 * q[2].max(0.02).powf(1.5));
                self.seed = self.seed.wrapping_add(0x6d2b79f5);
                let mut t = (self.seed ^ (self.seed >> 15)).wrapping_mul(1 | self.seed);
                t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
                let random = (t ^ (t >> 14)) as f64 / 4294967296.;
                let a = (1. - (1. - flow).powf(sp / reference))
                    * (0.7 + 0.6 * random)
                    * self.st.op.unwrap_or(1.);
                self.b.gaussian(self.screen(q), [s, s], 0., a, false, 0.);
                self.next = sp;
                self.started = true;
            }
            Tool::Pen => {
                if !self.started {
                    self.pen(q, true);
                }
                self.pen(q, false);
            }
            Tool::Eraser => {
                let r = self.st.size * self.b.view[2] * 1.2;
                let sp = (0.12 * r).max(0.35);
                let op = self.st.op.unwrap_or(1.);
                let a = if op >= 1. {
                    1.
                } else {
                    1. - (1. - op).powf(sp / (2. * r))
                };
                self.b.erase(self.screen(q), r, a, &self.st.only);
                self.next = sp;
                self.started = true;
            }
        }
    }
    fn segment(&mut self, p: &[Pt]) {
        if !self.started {
            self.tangent = (p[2][1] - p[1][1]).atan2(p[2][0] - p[1][0]);
            self.stamp(p[1]);
        }
        let count = (((p[2][0] - p[1][0]).hypot(p[2][1] - p[1][1]) * self.b.view[2] / 1.5).ceil()
            as usize)
            .max(1);
        let mut prev = p[1];
        for k in 1..=count {
            let t = k as f64 / count as f64;
            let mut cur = [0.; 4];
            for (n, v) in cur.iter_mut().enumerate() {
                *v = if n < 2 {
                    0.5 * (2. * p[1][n]
                        + (-p[0][n] + p[2][n]) * t
                        + (2. * p[0][n] - 5. * p[1][n] + 4. * p[2][n] - p[3][n]) * t * t
                        + (-p[0][n] + 3. * p[1][n] - 3. * p[2][n] + p[3][n]) * t * t * t)
                } else {
                    p[1][n] + (p[2][n] - p[1][n]) * t
                };
            }
            let d = (cur[0] - prev[0]).hypot(cur[1] - prev[1]) * self.b.view[2];
            if d > 1e-6 {
                if d > 0.4 || !self.started {
                    self.tangent = (cur[1] - prev[1]).atan2(cur[0] - prev[0]);
                }
                let mut pos = 0.;
                while self.next <= d - pos {
                    pos += self.next;
                    let f = pos / d;
                    let mut q = [0.; 4];
                    for n in 0..4 {
                        q[n] = prev[n] + (cur[n] - prev[n]) * f;
                    }
                    self.stamp(q);
                }
                self.next -= d - pos;
            }
            prev = cur;
        }
    }
}
