use memorious_core::canvas_fold::{fold_wire_ops, Layers, StrokeItem, Tool};
use serde_json::json;

#[test]
fn canvas_layers_and_erasers_match_the_coverage_contract() {
    let ops = vec![
        json!({"op":"stroke","cid":"pencil","tool":"pencil","size":8,"seed":7,"pts":[[8,16,0.6,0],[56,16,0.6,0]]}),
        json!({"op":"stroke","cid":"pen","tool":"pen","size":5,"seed":1,"pts":[[8,32,0.6,0],[56,32,0.6,0]]}),
    ];
    let items = fold_wire_ops(&ops);
    let mut buf = memorious_core::canvas_render::InkBuffers::new(64, 48);
    buf.replay(&items, [0., 0., 1.]);
    assert!(buf.alpha(Layers::ALL)[16 * 64 + 32] > 0.2);
    assert!(buf.alpha(Layers::ALL)[32 * 64 + 32] > 0.9);
    assert_eq!(
        buf.alpha(Layers {
            pencil: false,
            pen: true,
            image: true
        })[16 * 64 + 32],
        0.
    );
    let before = buf.alpha(Layers::ALL)[32 * 64 + 32];
    buf.stroke(&StrokeItem {
        id: "erase".into(),
        tool: Tool::Eraser,
        size: 10.,
        seed: 1,
        op: Some(1.),
        only: Some(vec!["pencil".into()]),
        pts: vec![[32., 0., 0.5, 0.], [32., 47., 0.5, 0.]],
    });
    assert_eq!(buf.alpha(Layers::ALL)[32 * 64 + 32], before);
    assert_eq!(buf.alpha(Layers::ALL)[16 * 64 + 32], 0.);
}
