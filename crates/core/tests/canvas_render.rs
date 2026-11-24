use memorious_core::{canvas_fold::StrokeItem, canvas_render::InkBuffers};
use serde_json::Value;
#[test]
fn coverage_matches_web_reference_for_pressure_opacity_and_layer_specific_erasure() {
    let cases: Value = serde_json::from_str(include_str!(
        "../../../apps/web/tests/canvas-vectors/render.json"
    ))
    .unwrap();
    for (case, v) in cases.as_array().unwrap().iter().enumerate() {
        let mut b = InkBuffers::new(32, 24);
        for stroke in v["strokes"].as_array().unwrap() {
            let mut s = stroke.clone();
            s["id"] = "test".into();
            b.stroke(&serde_json::from_value::<StrokeItem>(s).unwrap());
        }
        for (name, got) in [
            ("pencil", &b.pencil),
            ("pen", &b.pen),
            ("opacity", &b.opacity),
        ] {
            for (i, (a, e)) in got.iter().zip(v[name].as_array().unwrap()).enumerate() {
                assert!(
                    (*a as f64 - e.as_f64().unwrap()).abs() < 1e-3,
                    "case {case}, {name}[{i}]: {a} != {e}"
                );
            }
        }
    }
}
