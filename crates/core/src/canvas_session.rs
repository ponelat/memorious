//! Toolkit-independent state for a native canvas. Journal I/O belongs to the host.
use crate::{
    canvas_fold::{self as fold, CanvasLog, Item, Layers, OpData, Poly, Pt, WireRecord, Xf},
    ink,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub fn current() -> String {
    ink::week_key(now())
}
#[derive(Default)]
pub struct Period {
    pub records: Vec<WireRecord>,
    pub cursor: i64,
}
pub struct Selection {
    pub poly: Poly,
    pub ids: BTreeMap<String, Vec<String>>,
    pub bounds: [f64; 4],
}
pub struct CanvasSession {
    pub periods: BTreeMap<String, Period>,
    pub shown: BTreeMap<String, bool>,
    pub pending: Vec<(String, Value)>,
    pub me: String,
    pub tool: String,
    pub previous: String,
    pub layers: Layers,
    pub dim: bool,
    pub view: [f64; 3],
    pub sizes: [f64; 3],
    pub opacity: [f64; 3],
    pub points: Vec<Pt>,
    pub start: [f64; 2],
    pub last_screen: [f64; 2],
    pub pan: bool,
    pub space: bool,
    pub selection: Option<Selection>,
    pub moving: bool,
    pub scaling: bool,
    pub drag_delta: [f64; 2],
    pub scale: f64,
    pub last_time: i64,
    pub revision: u64,
    pub stylus: bool,
}
impl Default for CanvasSession {
    fn default() -> Self {
        Self::new()
    }
}
impl CanvasSession {
    pub fn new() -> Self {
        let period = current();
        let prev = ink::previous_period(&period).unwrap();
        let mut shown = BTreeMap::new();
        shown.insert(period, true);
        shown.insert(prev, true);
        Self {
            periods: BTreeMap::new(),
            shown,
            pending: Vec::new(),
            me: String::new(),
            tool: "pencil".into(),
            previous: "eraser".into(),
            layers: Layers::ALL,
            dim: true,
            view: [0., 0., 1.],
            sizes: [4.5, 5., 5.],
            opacity: [0.7, 1., 0.4],
            points: Vec::new(),
            start: [0., 0.],
            last_screen: [0., 0.],
            pan: false,
            space: false,
            selection: None,
            moving: false,
            scaling: false,
            drag_delta: [0., 0.],
            scale: 1.,
            last_time: 0,
            revision: 0,
            stylus: false,
        }
    }

    pub fn log(&self, period: &str) -> CanvasLog {
        let records = self
            .periods
            .get(period)
            .map(|p| p.records.as_slice())
            .unwrap_or(&[]);
        let mut log = CanvasLog::from_records(period, &self.me, records);
        for (_, v) in self.pending.iter().filter(|(p, _)| p == period) {
            if let Some(op) = v.as_object().and_then(OpData::from_wire) {
                let cid = v["cid"].as_str().unwrap_or("");
                if !log.log.ops.iter().any(|o| o.cid == cid) {
                    log.log.add_with(op, cid, now() as f64);
                }
            }
        }
        log
    }
    /// Floating selection preview; the actual edit is appended only on release.
    pub fn display_items(&self, period: &str) -> Vec<Item> {
        let mut log = self.log(period);
        if self.moving || self.scaling {
            if let Some(sel) = &self.selection {
                if let Some(ids) = sel.ids.get(period) {
                    log.log.add_with(
                        OpData::Move {
                            poly: sel.poly.clone(),
                            xf: Xf {
                                dx: self.drag_delta[0],
                                dy: self.drag_delta[1],
                                s: self.scale,
                                cx: sel.bounds[0],
                                cy: sel.bounds[1],
                            },
                            copy: false,
                            only: self.layers.only_list(),
                            ids: Some(ids.clone()),
                        },
                        "preview",
                        now() as f64,
                    );
                }
            }
        }
        log.items()
    }
    pub fn add(&mut self, period: String, data: OpData) -> String {
        let mut log = self.log(&period).log;
        let cid = log.add(data);
        self.periods.entry(period.clone()).or_default();
        self.pending
            .push((period, log.ops.last().unwrap().data.to_wire(&cid)));
        self.revision += 1;
        cid
    }
    pub fn world(&self, x: f64, y: f64) -> [f64; 2] {
        [
            self.view[0] + x / self.view[2],
            self.view[1] + y / self.view[2],
        ]
    }
    pub fn tool_index(&self) -> usize {
        match self.tool.as_str() {
            "pen" => 1,
            "eraser" => 2,
            _ => 0,
        }
    }
    pub fn visible(&self, p: &str) -> bool {
        self.shown.get(p).copied().unwrap_or(p == current())
    }
    pub fn select(&mut self, poly: Poly) {
        let mut ids = BTreeMap::new();
        let mut pieces = Vec::new();
        let period = current();
        for p in self.periods.keys().chain(std::iter::once(&period)) {
            if !self.visible(p) || (self.tool != "timeline" && p != &period) {
                continue;
            }
            let selected = fold::pieces_in(&self.log(p).items(), &poly, &self.layers);
            if !selected.ids.is_empty() {
                ids.insert(p.clone(), selected.ids);
                pieces.extend(selected.pieces);
            }
        }
        self.selection = if pieces.is_empty() {
            None
        } else {
            Some(Selection {
                poly,
                ids,
                bounds: fold::bbox_of(&pieces),
            })
        };
    }
    pub fn edit(&mut self, action: &str) {
        let Some(sel) = self.selection.take() else {
            return;
        };
        let xf = Xf {
            dx: self.drag_delta[0],
            dy: self.drag_delta[1],
            s: self.scale,
            cx: sel.bounds[0],
            cy: sel.bounds[1],
        };
        let period = current();
        let mut moved_poly = fold::xf_poly(&sel.poly, &xf);
        let mut results: Vec<(String, String, Vec<String>)> = Vec::new();
        for (p, ids) in &sel.ids {
            if action == "copy" && p != &period {
                let selected = fold::pieces_in(
                    &self
                        .log(p)
                        .items()
                        .into_iter()
                        .filter(|i| ids.contains(&i.id().to_string()))
                        .collect::<Vec<_>>(),
                    &sel.poly,
                    &self.layers,
                );
                for it in selected.pieces {
                    let op = match fold::xf_item(&it, &xf) {
                        Item::Stroke(s) => OpData::Stroke {
                            tool: s.tool,
                            seed: s.seed,
                            size: s.size,
                            opacity: s.op,
                            only: s.only,
                            pts: s.pts,
                        },
                        Item::Image(i) => OpData::Image {
                            hash: i.hash,
                            rect: i.rect,
                        },
                    };
                    let cid = self.add(period.clone(), op);
                    results.push((period.clone(), cid, Vec::new()));
                }
            } else {
                let op = if action == "delete" {
                    OpData::Delete {
                        poly: sel.poly.clone(),
                        only: self.layers.only_list(),
                        ids: Some(ids.clone()),
                    }
                } else {
                    OpData::Move {
                        poly: sel.poly.clone(),
                        xf,
                        copy: action == "copy",
                        only: self.layers.only_list(),
                        ids: Some(ids.clone()),
                    }
                };
                let cid = self.add(p.clone(), op);
                results.push((p.clone(), cid, ids.clone()));
            }
        }
        if action != "delete" {
            // Keep the exact resulting pieces selected, including their derived ids.
            let mut new_ids = BTreeMap::new();
            let mut pieces = Vec::new();
            let mut periods: Vec<_> = results.iter().map(|(p, _, _)| p.clone()).collect();
            periods.sort();
            periods.dedup();
            for p in periods {
                let mut candidates = self.log(&p).items();
                candidates.retain(|it| {
                    results
                        .iter()
                        .filter(|(q, _, _)| q == &p)
                        .any(|(_, cid, ids)| {
                            it.id() == cid
                                || ids
                                    .iter()
                                    .any(|id| it.id().starts_with(&format!("{id}~{cid}i")))
                        })
                });
                let result = fold::pieces_in(&candidates, &moved_poly, &self.layers);
                if !result.ids.is_empty() {
                    new_ids.insert(p, result.ids);
                    pieces.extend(result.pieces);
                }
            }
            if !pieces.is_empty() {
                self.selection = Some(Selection {
                    poly: std::mem::take(&mut moved_poly),
                    ids: new_ids,
                    bounds: fold::bbox_of(&pieces),
                });
            }
        }
        self.drag_delta = [0., 0.];
        self.scale = 1.;
    }
}
