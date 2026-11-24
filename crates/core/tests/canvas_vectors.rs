//! Golden-vector conformance for `canvas_fold` (same vectors the TS reference generated and the Swift port passes).
use memorious_core::canvas_fold::*;
use serde_json::{json, Value};

fn load() -> Value {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../apps/web/tests/canvas-vectors/vectors.json");
    serde_json::from_str(&std::fs::read_to_string(p).expect("read vectors.json")).expect("parse vectors.json")
}

/// Structural equality; numbers within `tol`.
fn approx(a: &Value, b: &Value, tol: f64, path: &str) -> Result<(), String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            if (x - y).abs() <= tol || (x == y) { Ok(()) } else { Err(format!("{path}: {x} != {y}")) }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Err(format!("{path}: array len {} != {}", x.len(), y.len()));
            }
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                approx(p, q, tol, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                match (x.get(k), y.get(k)) {
                    (Some(p), Some(q)) => approx(p, q, tol, &format!("{path}.{k}"))?,
                    (Some(Value::Null), None) | (None, Some(Value::Null)) => {}
                    _ => return Err(format!("{path}.{k}: key present on one side only ({:?} vs {:?})", x.get(k), y.get(k))),
                }
            }
            Ok(())
        }
        _ if a == b => Ok(()),
        _ => Err(format!("{path}: {a} != {b}")),
    }
}

fn evs(v: &Value) -> Vec<Ev> {
    serde_json::from_value(v.clone()).expect("evs")
}
fn tov<T: serde::Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap()
}

fn run_case(c: &Value) -> Result<(), String> {
    let kind = c["kind"].as_str().unwrap();
    let inp = &c["input"];
    let exp = &c["expected"];
    let tol = 1e-9;
    let got: Value = match kind {
        "fold" => tov(&fold(&evs(&inp["evs"]), &evs(&inp["extra"]))),
        "pieces" => {
            let items = fold(&evs(&inp["evs"]), &[]);
            let poly: Poly = serde_json::from_value(inp["poly"].clone()).unwrap();
            let layers: Layers = serde_json::from_value(inp["layers"].clone()).unwrap();
            let p = pieces_in(&items, &poly, &layers);
            json!({"items": items, "pieces": p.pieces, "ids": p.ids, "bbox": bbox_of(&items)})
        }
        "pip" => {
            let poly: Poly = serde_json::from_value(inp["poly"].clone()).unwrap();
            let pts: Vec<[f64; 2]> = serde_json::from_value(inp["pts"].clone()).unwrap();
            json!(pts.iter().map(|p| pip(&poly, p[0], p[1])).collect::<Vec<_>>())
        }
        "split" => {
            let pts: Vec<Pt> = serde_json::from_value(inp["pts"].clone()).unwrap();
            let poly: Poly = serde_json::from_value(inp["poly"].clone()).unwrap();
            tov(&split_by_poly(&pts, &poly))
        }
        "xf" => {
            let f: Xf = serde_json::from_value(inp["xf"].clone()).unwrap();
            let poly: Poly = serde_json::from_value(inp["poly"].clone()).unwrap();
            let items: Vec<Item> = serde_json::from_value(inp["items"].clone()).unwrap();
            json!({"poly": xf_poly(&poly, &f), "items": items.iter().map(|i| xf_item(i, &f)).collect::<Vec<_>>()})
        }
        "simplify" => {
            let pts: Poly = serde_json::from_value(inp["pts"].clone()).unwrap();
            tov(&simplify_poly(&pts, inp["tol"].as_f64().unwrap()))
        }
        "wire" => match inp["wire"].as_object().and_then(OpData::from_wire) {
            Some(d) => d.to_wire(inp["wire"].get("cid").and_then(Value::as_str).unwrap_or("none")),
            None => Value::Null,
        },
        "oplog" => run_oplog(inp),
        k => return Err(format!("unknown kind {k}")),
    };
    approx(&got, exp, tol, "")
}

fn run_oplog(inp: &Value) -> Value {
    let me = inp["me"].as_str().unwrap();
    let initial: Vec<Op> = serde_json::from_value(inp["initial"].clone()).unwrap();
    let mut log = if initial.is_empty() { MonthLog::new("2026-01", me) } else { MonthLog::restore("2026-01", me, initial) };
    // deterministic clock + cids, same as gen.ts: clock starts 1000 and +1 per add; cids c1, c2, ...
    let (mut clock, mut cid_n) = (1000.0, 0);
    let mut next = || {
        clock += 1.0;
        cid_n += 1;
        (format!("c{cid_n}"), clock)
    };
    let mut rets: Vec<Value> = Vec::new();
    for s in inp["steps"].as_array().unwrap() {
        match s["do"].as_str().unwrap() {
            "add" => {
                let data: OpData = serde_json::from_value(s["data"].clone()).unwrap();
                let (cid, t) = next();
                rets.push(json!(log.add_with(data, cid, t)));
            }
            "confirm" => {
                log.confirm(s["cid"].as_str().unwrap(), s["eid"].as_str().unwrap(), s["t"].as_f64().unwrap());
                rets.push(Value::Null);
            }
            "merge" => {
                let recs: Vec<WireRecord> = serde_json::from_value(s["records"].clone()).unwrap();
                rets.push(json!(log.merge(&recs)));
            }
            "undo" => {
                // only consume the clock / cid when an op is actually added
                let r = if log.can_undo() {
                    let (cid, t) = next();
                    log.undo_with(cid, t)
                } else {
                    None
                };
                rets.push(json!(r));
            }
            "redo" => {
                let r = if log.can_redo() {
                    let (cid, t) = next();
                    log.redo_with(cid, t)
                } else {
                    None
                };
                rets.push(json!(r));
            }
            "adopt" => {
                log.adopt_device(s["id"].as_str().unwrap());
                rets.push(Value::Null);
            }
            d => panic!("step {d}"),
        }
    }
    let events: Vec<Value> = log
        .events()
        .iter()
        .map(|e| {
            let mut v = tov(&e.data);
            let o = v.as_object_mut().unwrap();
            o.insert("id".into(), json!(e.id));
            o.insert("active".into(), json!(e.active));
            v
        })
        .collect();
    json!({
        "rets": rets,
        "ops": tov(&log.ops),
        "wire": log.wire(),
        "events": events,
        "pending": log.pending().iter().map(|o| o.cid.clone()).collect::<Vec<_>>(),
        "canUndo": log.can_undo(),
        "canRedo": log.can_redo(),
        "hasContent": log.has_content(),
        "version": log.version,
        "me": log.me,
    })
}

#[test]
fn golden_vectors() {
    let v = load();
    assert_eq!(v["tolerance"].as_f64(), Some(1e-9));
    let cases = v["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 66, "vector count changed; regenerate / update the port");
    let mut fails = Vec::new();
    for c in cases {
        if let Err(e) = run_case(c) {
            fails.push(format!("{}: {e}", c["name"].as_str().unwrap()));
        }
    }
    assert!(fails.is_empty(), "{} of {} cases failed:\n{}", fails.len(), cases.len(), fails.join("\n"));
}

#[test]
fn convenience_fold_from_stored_ops() {
    let ops = vec![
        json!({"op":"stroke","tool":"pen","seed":7,"size":2,"pts":[[0,0,0.5,0],[10,0,0.5,0]],"cid":"a"}),
        json!({"op":"stroke","tool":"pen","seed":8,"size":2,"pts":[[0,5,0.5,0],[10,5,0.5,0]],"cid":"b"}),
        json!({"op":"undo","target":"b","cid":"u"}),
        json!({"op":"sparkle","cid":"x"}),
    ];
    let items = fold_wire_ops(&ops);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id(), "a");
}

#[test]
fn wire_reader_uses_latest_fold_and_only_uncovered_ops() {
    let stroke = json!({"op":"stroke","cid":"original","tool":"pen","seed":1,"size":5,"pts":[[0,0,0.5,0],[10,10,0.5,0]]});
    let records: Vec<WireRecord> = serde_json::from_value(json!([
        {"event_id":"a","device_id":"dev-a","seq":1,"recorded_at":1,"data":stroke},
        {"event_id":"b","device_id":"dev-b","seq":1,"recorded_at":2,"data":{"op":"ink_fold","items":[{"id":"folded","tool":"pen","seed":1,"size":5,"pts":[[0,0,0.5,0],[10,10,0.5,0]]}],"supersedes":[{"select":"ink","period":"2026-W48","up_to":{"dev-a":1}}]}},
        {"event_id":"c","device_id":"dev-a","seq":2,"recorded_at":0,"data":{"op":"stroke","cid":"late","tool":"pen","seed":2,"size":5,"pts":[[20,20,0.5,0]]}},
        {"event_id":"d","device_id":"dev-a","seq":3,"recorded_at":3,"data":{"op":"future-op"}}
    ])).unwrap();
    let items = fold_wire_records(&records);
    assert_eq!(
        items.iter().map(Item::id).collect::<Vec<_>>(),
        vec!["folded", "late"]
    );
}

#[test]
fn fold_base_handles_late_undo_and_legacy_records_without_sequence() {
    let records:Vec<WireRecord>=serde_json::from_value(json!([
        {"event_id":"fold","device_id":"dev-b","seq":2,"recorded_at":2,"data":{"op":"ink_fold","items":[{"id":"base","tool":"pen","seed":1,"size":5,"pts":[[1,1,0.5,0]]}],"supersedes":[{"select":"ink","up_to":{"dev-a":1}}]}},
        {"event_id":"legacy","device_id":"dev-a","recorded_at":1,"data":{"op":"stroke","tool":"pen","size":5,"seed":1,"pts":[[2,2,0.5,0]]}},
        {"event_id":"undo","device_id":"dev-b","seq":3,"recorded_at":3,"data":{"op":"undo","target":"base"}}
    ])).unwrap();
    let items = fold_wire_records(&records);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id(), "legacy");
}
