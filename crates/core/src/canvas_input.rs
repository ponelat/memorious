//! Capture-time point simplification shared by native shells.
use crate::canvas_fold::Pt;
/// Four-dimensional RDP (pressure and tilt have weight 3), followed by the
/// wire rounding used by the canvas reference. Pencil passes tolerance zero.
pub fn simplify(pts: &[Pt], tolerance: f64) -> Vec<Pt> {
    if pts.is_empty() {
        return Vec::new();
    }
    let mut keep = vec![tolerance <= 0.; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = if pts.len() > 2 && tolerance > 0. {
        vec![(0, pts.len() - 1)]
    } else {
        Vec::new()
    };
    while let Some((a, b)) = stack.pop() {
        let mut ab = [0.; 4];
        for n in 0..4 {
            ab[n] = (pts[b][n] - pts[a][n]) * if n < 2 { 1. } else { 3. };
        }
        let ab2 = ab.iter().map(|v| v * v).sum::<f64>();
        let mut max = 0.;
        let mut at = a;
        for i in a + 1..b {
            let mut ap = [0.; 4];
            for n in 0..4 {
                ap[n] = (pts[i][n] - pts[a][n]) * if n < 2 { 1. } else { 3. };
            }
            let t = if ab2 > 0. {
                ap.iter().zip(ab).map(|(p, b)| p * b).sum::<f64>() / ab2
            } else {
                0.
            }
            .clamp(0., 1.);
            let d = ap
                .iter()
                .zip(ab)
                .map(|(p, b)| (p - t * b).powi(2))
                .sum::<f64>()
                .sqrt();
            if d > max {
                max = d;
                at = i;
            }
        }
        if max > tolerance {
            keep[at] = true;
            stack.push((a, at));
            stack.push((at, b));
        }
    }
    pts.iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(p, _)| {
            let mut p = *p;
            for (n, v) in p.iter_mut().enumerate() {
                let scale = if n < 2 { 10. } else { 100. };
                *v = (*v * scale).round() / scale;
            }
            p
        })
        .collect()
}
