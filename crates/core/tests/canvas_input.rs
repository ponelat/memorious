use memorious_core::canvas_input::simplify;
#[test]
fn points_are_rounded_and_pressure_corners_survive_simplification() {
    let pts = vec![
        [0.01, 0.03, 0.501, 0.001],
        [1., 0., 0.9, 0.],
        [2., 0., 0.5, 0.],
    ];
    let out = simplify(&pts, 0.4);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0], [0., 0., 0.5, 0.]);
    assert_eq!(
        simplify(
            &[[0., 0., 0.5, 0.], [1., 0., 0.5, 0.], [2., 0., 0.5, 0.]],
            0.4
        )
        .len(),
        2
    );
}
