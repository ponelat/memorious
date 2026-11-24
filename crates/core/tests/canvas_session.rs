use memorious_core::{
    canvas_fold::{OpData, Tool},
    canvas_session::{current, CanvasSession},
};
fn stroke(x: f64) -> OpData {
    OpData::Stroke {
        tool: Tool::Pen,
        seed: 1,
        size: 5.,
        opacity: None,
        only: None,
        pts: vec![[x, 10., 0.5, 0.], [x + 10., 10., 0.5, 0.]],
    }
}
#[test]
fn repeated_selection_move_holds_only_original_pieces() {
    let mut s = CanvasSession::new();
    let period = current();
    s.add(period.clone(), stroke(10.));
    s.add(period.clone(), stroke(50.));
    s.select(vec![[5., 5.], [25., 5.], [25., 15.], [5., 15.]]);
    assert!(s.selection.is_some());
    s.drag_delta = [40., 0.];
    s.edit("move");
    assert!(s.selection.is_some());
    s.drag_delta = [40., 0.];
    s.edit("move");
    let items = s.log(&period).items();
    let xs: Vec<_> = items
        .iter()
        .filter_map(|it| match it {
            memorious_core::canvas_fold::Item::Stroke(s) => Some(s.pts[0][0]),
            _ => None,
        })
        .collect();
    assert!(
        xs.contains(&50.),
        "moving the held selection must leave other ink alone: {xs:?}"
    );
    assert!(xs.contains(&90.));
}
#[test]
fn timeline_duplicate_brings_earlier_ink_to_current_period() {
    let mut s = CanvasSession::new();
    let current = current();
    let old = "2026-01";
    s.add(old.into(), stroke(10.));
    s.periods.entry(old.into()).or_default();
    s.shown.insert(old.into(), true);
    s.tool = "timeline".into();
    s.select(vec![[0., 0.], [30., 0.], [30., 20.], [0., 20.]]);
    s.drag_delta = [16., 16.];
    s.edit("copy");
    assert_eq!(s.log(old).items().len(), 1);
    assert_eq!(s.log(&current).items().len(), 1);
    assert!(s.selection.is_some());
    assert_eq!(
        s.pending.iter().filter(|(p, _)| p == old).count(),
        1,
        "copy must leave earlier period unchanged"
    );
}

#[test]
fn preview_moves_selected_ink_without_appending_an_edit() {
    let mut s = CanvasSession::new();
    let p = current();
    s.add(p.clone(), stroke(10.));
    s.select(vec![[5., 5.], [25., 5.], [25., 15.], [5., 15.]]);
    s.moving = true;
    s.drag_delta = [20., 0.];
    let before = s.pending.len();
    let preview = s.display_items(&p);
    assert_eq!(s.pending.len(), before);
    assert_eq!(memorious_core::canvas_fold::bbox_of(&preview)[0], 27.);
    assert_eq!(
        memorious_core::canvas_fold::bbox_of(&s.log(&p).items())[0],
        7.
    );
}
