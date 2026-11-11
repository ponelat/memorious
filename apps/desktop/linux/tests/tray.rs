#![cfg(feature = "gui")]
#[test]
fn tray_menu_has_the_dbusmenu_wire_shape() {
    let layout = memorious_linux::tray::menu_layout();
    assert_eq!(layout.type_().as_str(), "(u(ia{sv}av))");
    let root = layout.child_value(1);
    let children = root.child_value(2);
    assert_eq!(children.n_children(), 3);
}
