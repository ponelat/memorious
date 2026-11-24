//! GTK canvas chrome and input. Folding, selections, periods and ink replay live in core.
use super::Worker;
use adw::prelude::*;
use anyhow::Context;
use gtk::{cairo, gdk, glib};
use memorious_core::canvas_session::{current, now, CanvasSession};
use memorious_core::{
    canvas_fold::{self as fold, Item, OpData, Poly, StrokeItem, Tool, WireRecord, Xf},
    canvas_render::InkBuffers,
    ink,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

pub(super) struct Canvas {
    pub window: adw::Window,
    area: gtk::DrawingArea,
    status: gtk::Label,
    state: RefCell<CanvasSession>,
    worker: Worker,
    busy: Cell<bool>,
    ready: Cell<bool>,
    cache: RefCell<HashMap<String, (u64, i32, i32, [f64; 3], cairo::ImageSurface)>>,
    undo: gtk::Button,
    redo: gtk::Button,
    size: gtk::Scale,
    opacity: gtk::Scale,
    periods: gtk::Box,
    tools: gtk::DropDown,
    zooming: Cell<u64>,
    settled: Cell<bool>,
}
impl Canvas {
    pub fn new(parent: &adw::ApplicationWindow, worker: Worker) -> Rc<Self> {
        let window = adw::Window::builder()
            .title("Memorious — Canvas")
            .transient_for(parent)
            .default_width(1100)
            .default_height(800)
            .build();
        let saved: Value = std::fs::read(glib::user_config_dir().join("memorious/canvas.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or(Value::Null);
        let mut state = CanvasSession::new();
        state.dim = saved["dim"].as_bool().unwrap_or(true);
        for (key, dest) in [("sizes", &mut state.sizes), ("opacity", &mut state.opacity)] {
            if let Ok(values) = serde_json::from_value::<[f64; 3]>(saved[key].clone()) {
                if values.iter().all(|v| v.is_finite()) {
                    *dest = values;
                }
            }
        }
        for v in &mut state.sizes {
            *v = v.clamp(1., 40.);
        }
        for v in &mut state.opacity {
            *v = v.clamp(0.05, 1.);
        }
        let c = Rc::new(Self {
            window,
            area: gtk::DrawingArea::builder()
                .hexpand(true)
                .vexpand(true)
                .focusable(true)
                .build(),
            status: gtk::Label::new(Some("Opening canvas…")),
            state: RefCell::new(state),
            worker,
            busy: Cell::new(false),
            ready: Cell::new(false),
            cache: RefCell::new(HashMap::new()),
            undo: gtk::Button::from_icon_name("edit-undo-symbolic"),
            redo: gtk::Button::from_icon_name("edit-redo-symbolic"),
            size: gtk::Scale::with_range(gtk::Orientation::Horizontal, 1., 40., 0.5),
            opacity: gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.05, 1., 0.05),
            periods: gtk::Box::new(gtk::Orientation::Vertical, 6),
            tools: gtk::DropDown::from_strings(&[
                "Pencil (D)",
                "Pen (W)",
                "Eraser (E)",
                "Lasso (S)",
                "Rectangle (R)",
                "Timeline (T)",
                "Pan",
            ]),
            zooming: Cell::new(0),
            settled: Cell::new(true),
        });
        c.build();
        c.poll();
        let weak = Rc::downgrade(&c);
        glib::timeout_add_local(std::time::Duration::from_millis(2500), move || {
            let Some(c) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !c.window.is_visible() {
                return glib::ControlFlow::Break;
            }
            c.poll();
            glib::ControlFlow::Continue
        });
        c.window.present();
        c
    }
    pub fn has_pending(&self) -> bool {
        !self.state.borrow().pending.is_empty() || self.busy.get()
    }
    fn build(self: &Rc<Self>) {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new("Canvas", &current())));
        self.undo.set_tooltip_text(Some("Undo (Ctrl+Z)"));
        self.redo.set_tooltip_text(Some("Redo (Ctrl+Shift+Z)"));
        header.pack_start(&self.undo);
        header.pack_start(&self.redo);
        let options = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Layers and periods")
            .build();
        let pop = gtk::Popover::new();
        let settings = gtk::Box::new(gtk::Orientation::Vertical, 8);
        settings.set_margin_top(12);
        settings.set_margin_bottom(12);
        settings.set_margin_start(12);
        settings.set_margin_end(12);
        for name in ["pencil", "pen", "image"] {
            let b = gtk::CheckButton::with_label(name);
            b.set_active(true);
            let weak = Rc::downgrade(self);
            b.connect_toggled(move |b| {
                if let Some(c) = weak.upgrade() {
                    let mut s = c.state.borrow_mut();
                    match name {
                        "pencil" => s.layers.pencil = b.is_active(),
                        "pen" => s.layers.pen = b.is_active(),
                        _ => s.layers.image = b.is_active(),
                    };
                    s.selection = None;
                    drop(s);
                    c.area.queue_draw();
                }
            });
            settings.append(&b);
        }
        let dim = gtk::CheckButton::with_label("Dim earlier periods");
        dim.set_active(self.state.borrow().dim);
        let weak = Rc::downgrade(self);
        dim.connect_toggled(move |b| {
            if let Some(c) = weak.upgrade() {
                c.state.borrow_mut().dim = b.is_active();
                c.save_options();
                c.area.queue_draw();
            }
        });
        settings.append(&dim);
        settings.append(&self.periods);
        let other = gtk::Entry::builder()
            .placeholder_text("YYYY-Www or YYYY-MM")
            .build();
        let weak = Rc::downgrade(self);
        other.connect_activate(move |e| {
            if let Some(c) = weak.upgrade() {
                let p = e.text().to_string();
                if ink::is_period_key(&p)
                    && ink::period_start_ms(&p) <= ink::period_start_ms(&current())
                {
                    c.state.borrow_mut().shown.insert(p, true);
                    c.period_controls();
                    c.poll();
                    e.set_text("");
                } else {
                    c.status
                        .set_text("Choose a current or earlier week or month");
                }
            }
        });
        settings.append(&other);
        pop.set_child(Some(&settings));
        options.set_popover(Some(&pop));
        header.pack_end(&options);
        let dark = gtk::Button::from_icon_name("weather-clear-night-symbolic");
        dark.set_tooltip_text(Some("Light / dark paper"));
        dark.connect_clicked(|_| {
            let m = adw::StyleManager::default();
            m.set_color_scheme(if m.is_dark() {
                adw::ColorScheme::ForceLight
            } else {
                adw::ColorScheme::ForceDark
            });
        });
        header.pack_end(&dark);
        body.append(&header);
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        toolbar.set_margin_start(8);
        toolbar.set_margin_end(8);
        let tools = self.tools.clone();
        tools.set_widget_name("canvas-tools");
        let weak = Rc::downgrade(self);
        tools.connect_selected_notify(move |b| {
            if let Some(c) = weak.upgrade() {
                c.tool(
                    [
                        "pencil", "pen", "eraser", "lasso", "rect", "timeline", "pan",
                    ][b.selected() as usize],
                );
            }
        });
        toolbar.append(&tools);
        self.size.set_size_request(110, -1);
        self.size.set_tooltip_text(Some("Tool size"));
        self.size.set_value(self.state.borrow().sizes[0]);
        toolbar.append(&self.size);
        self.opacity.set_size_request(90, -1);
        self.opacity.set_tooltip_text(Some("Tool opacity"));
        self.opacity.set_value(self.state.borrow().opacity[0]);
        toolbar.append(&self.opacity);
        for (button, action) in [(&self.undo, "undo"), (&self.redo, "redo")] {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(c) = weak.upgrade() {
                    c.history(action);
                }
            });
        }
        for (text, action) in [
            ("Swap (X)", "swap"),
            ("Duplicate (C)", "copy"),
            ("Delete (Q)", "delete"),
            ("−", "out"),
            ("100%", "reset"),
            ("+", "in"),
            ("Fit", "fit"),
        ] {
            let b = gtk::Button::with_label(text);
            let weak = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(c) = weak.upgrade() {
                    c.action(action);
                }
            });
            toolbar.append(&b);
        }
        let toolbar_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&toolbar)
            .build();
        body.append(&toolbar_scroll);
        body.append(&self.area);
        body.append(&self.status);
        self.window.set_content(Some(&body));
        self.area.set_widget_name("canvas-surface");
        let weak = Rc::downgrade(self);
        self.area.set_draw_func(move |_, cr, w, h| {
            if let Some(c) = weak.upgrade() {
                c.paint(cr, w, h);
            }
        });
        for (scale, is_size) in [(&self.size, true), (&self.opacity, false)] {
            let weak = Rc::downgrade(self);
            scale.connect_value_changed(move |v| {
                if let Some(c) = weak.upgrade() {
                    let mut s = c.state.borrow_mut();
                    let i = s.tool_index();
                    if is_size {
                        s.sizes[i] = v.value();
                    } else {
                        s.opacity[i] = v.value();
                    }
                    drop(s);
                    c.save_options();
                }
            });
        }
        let drag = gtk::GestureDrag::new();
        drag.set_button(0);
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |g, x, y| {
            if let Some(c) = weak.upgrade() {
                c.area.grab_focus();
                if g.current_event().is_some_and(|e| e.device_tool().is_some()) {
                    return;
                }
                c.begin(x, y, g.current_button(), None);
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            if let Some(c) = weak.upgrade() {
                if c.state.borrow().stylus {
                    return;
                }
                c.motion(dx, dy, None);
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(c) = weak.upgrade() {
                if !c.state.borrow().stylus {
                    c.end();
                }
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_cancel(move |_, _| {
            if let Some(c) = weak.upgrade() {
                c.state.borrow_mut().points.clear();
                c.area.queue_draw();
            }
        });
        self.area.add_controller(drag);
        let stylus = gtk::GestureStylus::builder().stylus_only(true).build();
        let weak = Rc::downgrade(self);
        stylus.connect_down(move |g, x, y| {
            if let Some(c) = weak.upgrade() {
                c.state.borrow_mut().stylus = true;
                c.begin(x, y, 1, g.axis(gdk::AxisUse::Pressure));
            }
        });
        let weak = Rc::downgrade(self);
        stylus.connect_motion(move |g, x, y| {
            if let Some(c) = weak.upgrade() {
                let start = c.state.borrow().start;
                c.motion(x - start[0], y - start[1], g.axis(gdk::AxisUse::Pressure));
            }
        });
        let weak = Rc::downgrade(self);
        stylus.connect_up(move |_, _, _| {
            if let Some(c) = weak.upgrade() {
                c.end();
                c.state.borrow_mut().stylus = false;
            }
        });
        self.area.add_controller(stylus);
        let pinch = gtk::GestureZoom::new();
        let zoom_base = Rc::new(Cell::new(1.));
        let base = zoom_base.clone();
        let weak = Rc::downgrade(self);
        pinch.connect_begin(move |_, _| {
            base.set(1.);
            if let Some(c) = weak.upgrade() {
                c.state.borrow_mut().points.clear();
            }
        });
        let weak = Rc::downgrade(self);
        pinch.connect_scale_changed(move |g, scale| {
            if let Some(c) = weak.upgrade() {
                let (x, y) = g
                    .bounding_box_center()
                    .unwrap_or((c.area.width() as f64 / 2., c.area.height() as f64 / 2.));
                c.zoom(scale / zoom_base.replace(scale), x, y);
            }
        });
        let touch_pan = gtk::GestureDrag::builder().n_points(2).button(0).build();
        touch_pan.group_with(&pinch);
        let tap_start = Rc::new(Cell::new(0_i64));
        let started = tap_start.clone();
        let weak = Rc::downgrade(self);
        touch_pan.connect_drag_begin(move |_, x, y| {
            started.set(now());
            if let Some(c) = weak.upgrade() {
                c.state.borrow_mut().points.clear();
                c.begin(x, y, 2, None);
            }
        });
        let weak = Rc::downgrade(self);
        touch_pan.connect_drag_update(move |_, dx, dy| {
            if let Some(c) = weak.upgrade() {
                c.motion(dx, dy, None);
            }
        });
        let weak = Rc::downgrade(self);
        touch_pan.connect_drag_end(move |_, dx, dy| {
            if let Some(c) = weak.upgrade() {
                c.end();
                if dx.hypot(dy) < 6. && now() - tap_start.get() < 300 {
                    c.history("undo");
                }
            }
        });
        self.area.add_controller(touch_pan);
        self.area.add_controller(pinch);
        let redo_tap = gtk::GestureClick::builder().n_points(3).button(0).build();
        let weak = Rc::downgrade(self);
        redo_tap.connect_released(move |_, _, _, _| {
            if let Some(c) = weak.upgrade() {
                c.state.borrow_mut().points.clear();
                c.history("redo");
            }
        });
        self.area.add_controller(redo_tap);
        let menu = gtk::GestureClick::builder().button(3).build();
        let weak = Rc::downgrade(self);
        menu.connect_released(move |_, _, x, y| {
            if let Some(c) = weak.upgrade() {
                let s = c.state.borrow();
                if (s.start[0] - x).hypot(s.start[1] - y) > 6. {
                    return;
                }
                drop(s);
                let pop = gtk::Popover::new();
                pop.set_parent(&c.area);
                pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                let buttons = gtk::Box::new(gtk::Orientation::Vertical, 4);
                for (title, tool) in [
                    ("Pencil (D)", "pencil"),
                    ("Pen (W)", "pen"),
                    ("Eraser (E)", "eraser"),
                    ("Lasso (S)", "lasso"),
                    ("Rectangle (R)", "rect"),
                    ("Timeline (T)", "timeline"),
                    ("Pan", "pan"),
                ] {
                    let b = gtk::Button::with_label(title);
                    let weak = Rc::downgrade(&c);
                    let close = pop.downgrade();
                    b.connect_clicked(move |_| {
                        if let Some(c) = weak.upgrade() {
                            c.tool(tool);
                        }
                        if let Some(pop) = close.upgrade() {
                            pop.popdown();
                        }
                    });
                    buttons.append(&b);
                }
                pop.set_child(Some(&buttons));
                pop.connect_closed(|p| p.unparent());
                pop.popup();
            }
        });
        self.area.add_controller(menu);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |e, _, dy| {
            if let Some(c) = weak.upgrade() {
                let pos = e
                    .current_event()
                    .and_then(|e| e.position())
                    .unwrap_or((c.area.width() as f64 / 2., c.area.height() as f64 / 2.));
                c.zoom((-dy * 0.12).exp(), pos.0, pos.1);
            }
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(c) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::space {
                c.state.borrow_mut().space = true;
                return glib::Propagation::Stop;
            }
            if matches!(key, gdk::Key::z | gdk::Key::Z)
                && mods.contains(gdk::ModifierType::CONTROL_MASK)
            {
                c.history(if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                    "redo"
                } else {
                    "undo"
                });
                return glib::Propagation::Stop;
            }
            let action = match key {
                gdk::Key::d => "pencil",
                gdk::Key::w => "pen",
                gdk::Key::e => "eraser",
                gdk::Key::s => "lasso",
                gdk::Key::r => "rect",
                gdk::Key::t => "timeline",
                gdk::Key::x => "swap",
                gdk::Key::c => "copy",
                gdk::Key::q | gdk::Key::Delete => "delete",
                gdk::Key::Escape => "deselect",
                _ => return glib::Propagation::Proceed,
            };
            if matches!(
                action,
                "pencil" | "pen" | "eraser" | "lasso" | "rect" | "timeline"
            ) {
                c.tool(action);
            } else {
                c.action(action);
            }
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(self);
        keys.connect_key_released(move |_, key, _, _| {
            if key == gdk::Key::space {
                if let Some(c) = weak.upgrade() {
                    c.state.borrow_mut().space = false;
                }
            }
        });
        self.area.add_controller(keys);
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(c) = weak.upgrade() {
                if c.has_pending() {
                    c.status.set_text("Saving ink — retrying before closing");
                    c.poll();
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(self);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(c) = weak.upgrade() {
                c.area.queue_draw();
            }
        });
        self.period_controls();
    }
    fn save_options(&self) {
        let s = self.state.borrow();
        let p = glib::user_config_dir().join("memorious/canvas.json");
        let _ = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(
                p.with_extension("tmp"),
                serde_json::to_vec(&json!({"dim":s.dim,"sizes":s.sizes,"opacity":s.opacity}))?,
            )?;
            std::fs::rename(p.with_extension("tmp"), p)?;
            Ok(())
        })();
    }
    fn tool(&self, name: &str) {
        let mut s = self.state.borrow_mut();
        if s.tool != name {
            s.previous = std::mem::replace(&mut s.tool, name.into());
        }
        s.selection = None;
        s.points.clear();
        let i = s.tool_index();
        let size = s.sizes[i];
        let opacity = s.opacity[i];
        drop(s);
        self.size.set_value(size);
        self.opacity.set_value(opacity);
        self.area
            .set_cursor_from_name(Some(if name == "pan" { "grab" } else { "crosshair" }));
        self.area.queue_draw();
    }
    fn history(self: &Rc<Self>, action: &str) {
        let mut s = self.state.borrow_mut();
        let period = current();
        let mut log = s.log(&period).log;
        let cid = if action == "undo" {
            log.undo()
        } else {
            log.redo()
        };
        if cid.is_some() {
            s.pending.push((
                period,
                log.ops
                    .last()
                    .unwrap()
                    .data
                    .to_wire(cid.as_deref().unwrap()),
            ));
            s.revision += 1;
            s.selection = None;
        }
        drop(s);
        self.changed();
    }
    fn action(self: &Rc<Self>, action: &str) {
        if action == "swap" {
            let tool = self.state.borrow().previous.clone();
            self.tool(&tool);
            return;
        }
        if matches!(action, "in" | "out" | "reset") {
            let z = self.state.borrow().view[2];
            self.zoom(
                match action {
                    "in" => 1.25,
                    "out" => 0.8,
                    _ => 1. / z,
                },
                self.area.width() as f64 / 2.,
                self.area.height() as f64 / 2.,
            );
            return;
        }
        if action == "delete"
            && self
                .state
                .borrow()
                .selection
                .as_ref()
                .is_some_and(|s| s.ids.keys().any(|p| p != &current()))
        {
            let dialog=adw::AlertDialog::builder().heading("Delete ink from earlier periods?").body("Earlier-period edits cannot be undone from this canvas. The journal keeps the edit as an event.").build();
            dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_close_response("cancel");
            let weak = Rc::downgrade(self);
            dialog.connect_response(None, move |_, r| {
                if r == "delete" {
                    if let Some(c) = weak.upgrade() {
                        c.state.borrow_mut().edit("delete");
                        c.changed();
                    }
                }
            });
            dialog.present(Some(&self.window));
            return;
        }
        let mut s = self.state.borrow_mut();
        match action {
            "copy" => {
                s.drag_delta = [16., 16.];
                s.edit("copy");
            }
            "delete" => s.edit("delete"),
            "deselect" => s.selection = None,
            "fit" => {
                let mut items = Vec::new();
                for p in s.periods.keys() {
                    if s.visible(p) {
                        items.extend(s.log(p).items());
                    }
                }
                if items.is_empty() {
                    return;
                }
                let b = fold::bbox_of(&items);
                let z = ((self.area.width() as f64 - 80.) / (b[2] - b[0]).max(1.))
                    .min((self.area.height() as f64 - 80.) / (b[3] - b[1]).max(1.))
                    .clamp(0.1, 20.);
                s.view = [b[0] - 40. / z, b[1] - 40. / z, z];
            }
            _ => {}
        }
        drop(s);
        self.changed();
    }
    fn zoom(self: &Rc<Self>, factor: f64, x: f64, y: f64) {
        let mut s = self.state.borrow_mut();
        let w = s.world(x, y);
        let z = (s.view[2] * factor).clamp(0.1, 20.);
        s.view = [w[0] - x / z, w[1] - y / z, z];
        drop(s);
        let generation = self.zooming.get().wrapping_add(1);
        self.zooming.set(generation);
        self.settled.set(false);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(140), move || {
            if let Some(c) = weak.upgrade() {
                if c.zooming.get() == generation {
                    c.settled.set(true);
                    c.area.queue_draw();
                }
            }
        });
        self.area.queue_draw();
    }
    fn begin(&self, x: f64, y: f64, button: u32, pressure: Option<f64>) {
        if !self.ready.get() {
            return;
        }
        let mut s = self.state.borrow_mut();
        s.start = [x, y];
        s.last_screen = [x, y];
        s.last_time = now();
        s.drag_delta = [0., 0.];
        s.scale = 1.;
        s.pan = button != 1 || s.space || s.tool == "pan";
        if s.pan {
            return;
        }
        let w = s.world(x, y);
        s.moving = false;
        s.scaling = false;
        if let Some(sel) = &s.selection {
            let b = sel.bounds;
            let corner = (w[0] - b[2]).hypot(w[1] - b[3]) < 14. / s.view[2];
            if corner || fold::pip(&sel.poly, w[0], w[1]) {
                s.moving = !corner;
                s.scaling = corner;
                return;
            }
        }
        s.selection = None;
        s.points = vec![[w[0], w[1], pressure.unwrap_or(0.5), 0.]];
        if !matches!(s.tool.as_str(), "timeline" | "lasso" | "rect") {
            s.shown.insert(current(), true);
        }
    }
    fn motion(&self, dx: f64, dy: f64, pressure: Option<f64>) {
        let mut s = self.state.borrow_mut();
        let x = s.start[0] + dx;
        let y = s.start[1] + dy;
        if s.pan {
            s.view[0] -= (x - s.last_screen[0]) / s.view[2];
            s.view[1] -= (y - s.last_screen[1]) / s.view[2];
            s.last_screen = [x, y];
            drop(s);
            self.area.queue_draw();
            return;
        }
        if s.moving {
            s.drag_delta = [dx / s.view[2], dy / s.view[2]];
        } else if s.scaling {
            let b = s.selection.as_ref().unwrap().bounds;
            let start = s.world(s.start[0], s.start[1]);
            let point = s.world(x, y);
            let initial = (start[0] - b[0]).hypot(start[1] - b[1]).max(1e-6);
            s.scale = ((point[0] - b[0]).hypot(point[1] - b[1]) / initial).clamp(0.1, 20.);
        } else if !s.points.is_empty() {
            let w = s.world(x, y);
            let last = *s.points.last().unwrap();
            let dt = (now() - s.last_time).max(1) as f64;
            let speed = (w[0] - last[0]).hypot(w[1] - last[1]) / dt;
            let p = pressure
                .unwrap_or(last[2] * 0.8 + (0.95 - 0.28 * speed).clamp(0.25, 0.95) * 0.2)
                .clamp(0.02, 1.);
            let point = if s.tool == "pen" {
                [
                    last[0] + (w[0] - last[0]) * 0.4,
                    last[1] + (w[1] - last[1]) * 0.4,
                    last[2] + (p - last[2]) * 0.25,
                    0.,
                ]
            } else {
                [w[0], w[1], p, 0.]
            };
            if (point[0] - last[0]).hypot(point[1] - last[1]) > 0.1 {
                s.points.push(point);
            }
            s.last_time = now();
        }
        s.last_screen = [x, y];
        drop(s);
        self.area.queue_draw();
    }
    fn end(self: &Rc<Self>) {
        let mut s = self.state.borrow_mut();
        if s.pan {
            s.pan = false;
            drop(s);
            self.area.queue_draw();
            return;
        }
        if s.moving || s.scaling {
            s.moving = false;
            s.scaling = false;
            s.edit("move");
            drop(s);
            self.changed();
            return;
        }
        if s.points.is_empty() {
            return;
        }
        let mut pts = std::mem::take(&mut s.points);
        let tool = s.tool.clone();
        if matches!(tool.as_str(), "lasso" | "rect" | "timeline") {
            let mut poly: Poly = pts.iter().map(|p| [p[0], p[1]]).collect();
            if tool == "rect" {
                let a = poly[0];
                let b = *poly.last().unwrap();
                poly = vec![a, [b[0], a[1]], b, [a[0], b[1]]];
            }
            if poly.len() > 2 {
                let tolerance = 1.5 / s.view[2];
                s.select(fold::simplify_poly(&poly, tolerance));
            }
        } else {
            if tool == "pen" {
                let raw = s.world(s.last_screen[0], s.last_screen[1]);
                for _ in 0..6 {
                    let p = *pts.last().unwrap();
                    pts.push([
                        p[0] + (raw[0] - p[0]) * 0.5,
                        p[1] + (raw[1] - p[1]) * 0.5,
                        p[2],
                        p[3],
                    ]);
                }
            }
            let i = s.tool_index();
            let pts = memorious_core::canvas_input::simplify(
                &pts,
                if tool == "pencil" { 0. } else { 0.4 },
            );
            let op = OpData::Stroke {
                tool: match tool.as_str() {
                    "pen" => Tool::Pen,
                    "eraser" => Tool::Eraser,
                    _ => Tool::Pencil,
                },
                seed: now() as u32,
                size: s.sizes[i],
                opacity: Some(s.opacity[i]),
                only: if tool == "eraser" {
                    s.layers.only_list()
                } else {
                    None
                },
                pts,
            };
            s.add(current(), op);
        }
        drop(s);
        self.changed();
    }
    fn changed(self: &Rc<Self>) {
        self.area.queue_draw();
        self.poll();
    }
    fn period_controls(self: &Rc<Self>) {
        while let Some(child) = self.periods.first_child() {
            self.periods.remove(&child);
        }
        let keys: Vec<_> = {
            let s = self.state.borrow();
            let mut keys: Vec<_> = s.periods.keys().chain(s.shown.keys()).cloned().collect();
            keys.sort_by_key(|k| ink::period_start_ms(k));
            keys.dedup();
            keys.reverse();
            keys
        };
        for period in keys {
            let b = gtk::CheckButton::with_label(&period);
            b.set_active(self.state.borrow().visible(&period));
            let weak = Rc::downgrade(self);
            b.connect_toggled(move |b| {
                if let Some(c) = weak.upgrade() {
                    c.state
                        .borrow_mut()
                        .shown
                        .insert(period.clone(), b.is_active());
                    c.state.borrow_mut().selection = None;
                    c.poll();
                    c.area.queue_draw();
                }
            });
            self.periods.append(&b);
        }
    }
    fn poll(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        let (periods, pending) = {
            let s = self.state.borrow();
            let mut p: Vec<_> = s.shown.keys().chain(s.periods.keys()).cloned().collect();
            p.push(current());
            p.sort();
            p.dedup();
            (
                p.iter()
                    .map(|p| (p.clone(), s.periods.get(p).map(|p| p.cursor).unwrap_or(0)))
                    .collect::<Vec<_>>(),
                s.pending.clone(),
            )
        };
        let weak = Rc::downgrade(self);
        self.worker.call(
            move |rt, state| {
                rt.block_on(async {
                    let session = state.as_ref().context("Journal is locked")?;
                    let months = session.request("ink_months", json!({})).await?;
                    let mut pages = Vec::new();
                    let mut confirmed = Vec::new();
                    for (period, cursor) in periods {
                        let page = session
                            .request("ink_list", json!({"period":period,"after":cursor}))
                            .await?;
                        pages.push((period, page));
                    }
                    for (period, op) in pending {
                        // Pull first: an append whose reply was lost is acknowledged by its cid.
                        let cid = op["cid"].as_str().unwrap_or("");
                        let existing = pages
                            .iter()
                            .filter(|(p, _)| p == &period)
                            .flat_map(|(_, p)| p["events"].as_array().into_iter().flatten())
                            .any(|r| r["data"]["cid"] == cid);
                        if !existing {
                            session
                                .request("ink_append", json!({"period":period,"ops":[op]}))
                                .await?;
                        }
                        confirmed.push((period, cid.to_string()));
                    }
                    // Re-read from arrival cursors so new records carry author, sequence and times.
                    for (period, page) in &mut pages {
                        let after = page["cursor"].as_i64().unwrap_or(0);
                        let fresh = session
                            .request("ink_list", json!({"period":period,"after":after}))
                            .await?;
                        if let Some(events) = fresh["events"].as_array() {
                            page["events"]
                                .as_array_mut()
                                .context("Missing ink events")?
                                .extend(events.iter().cloned());
                        }
                        page["cursor"] = fresh["cursor"].clone();
                    }
                    Ok((months, pages, confirmed))
                })
            },
            move |result| {
                let Some(c) = weak.upgrade() else {
                    return;
                };
                c.busy.set(false);
                match result {
                    Ok((months, pages, confirmed)) => {
                        let mut s = c.state.borrow_mut();
                        let old = s.periods.len();
                        let mut changed = !confirmed.is_empty();
                        for period in months["months"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                        {
                            s.periods.entry(period.into()).or_default();
                        }
                        for (period, page) in pages {
                            if let Some(me) = page["device_id"].as_str() {
                                s.me = me.into();
                            }
                            let p = s.periods.entry(period).or_default();
                            for record in page["events"].as_array().into_iter().flatten() {
                                if let Ok(r) = serde_json::from_value::<WireRecord>(record.clone())
                                {
                                    if !p.records.iter().any(|e| e.event_id == r.event_id) {
                                        p.records.push(r);
                                        changed = true;
                                    }
                                }
                            }
                            p.cursor = page["cursor"].as_i64().unwrap_or(p.cursor);
                        }
                        for (period, cid) in confirmed {
                            s.pending.retain(|(p, op)| p != &period || op["cid"] != cid);
                        }
                        let pending = s.pending.len();
                        let new = s.periods.len() != old;
                        if changed {
                            s.revision += 1;
                        }
                        drop(s);
                        c.ready.set(true);
                        c.status.set_text(if pending == 0 {
                            "Saved locally"
                        } else {
                            "Saving ink…"
                        });
                        if new {
                            c.period_controls();
                        }
                        let s = c.state.borrow();
                        let log = s.log(&current());
                        c.undo.set_sensitive(log.log.can_undo());
                        c.redo.set_sensitive(log.log.can_redo());
                        drop(s);
                        if changed {
                            c.cache.borrow_mut().clear();
                            c.area.queue_draw();
                        }
                        if pending > 0 {
                            c.poll();
                        }
                    }
                    Err(e) => c.status.set_text(&format!("Ink pending — retrying: {e}")),
                }
            },
        );
    }
    fn paint(&self, cr: &cairo::Context, w: i32, h: i32) {
        if w <= 0 || h <= 0 {
            return;
        }
        let dark = adw::StyleManager::default().is_dark();
        cr.set_source_rgb(
            if dark { 0.08 } else { 0.98 },
            if dark { 0.08 } else { 0.97 },
            if dark { 0.08 } else { 0.95 },
        );
        let _ = cr.paint();
        let s = self.state.borrow();
        let period = current();
        let mut keys: Vec<_> = s
            .periods
            .keys()
            .chain(std::iter::once(&period))
            .filter(|p| s.visible(p))
            .cloned()
            .collect();
        keys.sort_by_key(|p| ink::period_start_ms(p));
        keys.dedup();
        for p in keys {
            let mut items = s.display_items(&p);
            if p == period
                && !s.points.is_empty()
                && matches!(s.tool.as_str(), "pencil" | "pen" | "eraser")
            {
                let i = s.tool_index();
                items.push(Item::Stroke(StrokeItem {
                    id: "live".into(),
                    tool: match s.tool.as_str() {
                        "pen" => Tool::Pen,
                        "eraser" => Tool::Eraser,
                        _ => Tool::Pencil,
                    },
                    seed: 1,
                    size: s.sizes[i],
                    op: Some(s.opacity[i]),
                    only: s.layers.only_list(),
                    pts: s.points.clone(),
                }));
            }
            let alpha = if p != period && s.visible(&period) && s.dim {
                0.3
            } else {
                1.
            };
            for it in &items {
                if let Item::Image(i) = it {
                    if !s.layers.image {
                        continue;
                    }
                    cr.set_source_rgba(0.5, 0.5, 0.5, alpha * 0.3);
                    cr.rectangle(
                        (i.rect[0] - s.view[0]) * s.view[2],
                        (i.rect[1] - s.view[1]) * s.view[2],
                        i.rect[2] * s.view[2],
                        i.rect[3] * s.view[2],
                    );
                    let _ = cr.fill();
                }
            }
            // Coverage is cached between redraws; only the active gesture bypasses it.
            let live = (p == period && !s.points.is_empty()) || s.moving || s.scaling;
            let key_revision =
                s.revision + ((s.layers.pencil as u64) << 60) + ((s.layers.pen as u64) << 61);
            let mut cache = self.cache.borrow_mut();
            let valid = !live
                && cache.get(&p).is_some_and(|(r, cw, ch, v, _)| {
                    *r == key_revision
                        && *cw == w
                        && *ch == h
                        && (*v == s.view || s.pan || !self.settled.get())
                });
            if !valid {
                let mut b = InkBuffers::new(w as usize, h as usize);
                b.replay(&items, s.view);
                let pixels = b.alpha(s.layers);
                let mut bytes = vec![0_u8; w as usize * h as usize * 4];
                for (i, a) in pixels.iter().enumerate() {
                    let a = (a.clamp(0., 1.) * 255.).round() as u32;
                    let pixel = (a << 24) | (a << 16) | (((a * 82) / 255) << 8);
                    bytes[i * 4..i * 4 + 4].copy_from_slice(&pixel.to_ne_bytes());
                }
                if let Ok(surface) =
                    cairo::ImageSurface::create_for_data(bytes, cairo::Format::ARgb32, w, h, w * 4)
                {
                    cache.insert(p.clone(), (key_revision, w, h, s.view, surface));
                }
            }
            if let Some((_, _, _, view, surface)) = cache.get(&p) {
                let _ = cr.save();
                let ratio = s.view[2] / view[2];
                cr.scale(ratio, ratio);
                cr.translate(
                    (view[0] - s.view[0]) * view[2],
                    (view[1] - s.view[1]) * view[2],
                );
                let _ = cr.set_source_surface(surface, 0., 0.);
                let _ = cr.paint_with_alpha(alpha);
                let _ = cr.restore();
            }
            if live {
                cache.remove(&p);
            }
        }
        if let Some(sel) = &s.selection {
            let xf = Xf {
                dx: s.drag_delta[0],
                dy: s.drag_delta[1],
                s: s.scale,
                cx: sel.bounds[0],
                cy: sel.bounds[1],
            };
            let poly = fold::xf_poly(&sel.poly, &xf);
            draw_poly(cr, &poly, s.view);
            let b = sel.bounds;
            let p = fold::xf_xy(b[2], b[3], &xf);
            cr.rectangle(
                (p[0] - s.view[0]) * s.view[2] - 5.,
                (p[1] - s.view[1]) * s.view[2] - 5.,
                10.,
                10.,
            );
            let _ = cr.fill();
        }
        if !s.points.is_empty() && matches!(s.tool.as_str(), "rect" | "lasso" | "timeline") {
            let mut poly: Poly = s.points.iter().map(|p| [p[0], p[1]]).collect();
            if s.tool == "rect" {
                let a = poly[0];
                let b = *poly.last().unwrap();
                poly = vec![a, [b[0], a[1]], b, [a[0], b[1]]];
            }
            draw_poly(cr, &poly, s.view);
        }
    }
}
fn draw_poly(cr: &cairo::Context, poly: &[[f64; 2]], view: [f64; 3]) {
    if poly.is_empty() {
        return;
    }
    let _ = cr.save();
    cr.set_source_rgb(1., 0.32, 0.);
    cr.set_line_width(1.);
    cr.set_dash(&[5., 4.], 0.);
    for (i, p) in poly.iter().enumerate() {
        let x = (p[0] - view[0]) * view[2];
        let y = (p[1] - view[1]) * view[2];
        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    cr.close_path();
    let _ = cr.stroke();
    let _ = cr.restore();
}

impl Canvas {
    pub fn smoke(self: &Rc<Self>, verify: bool, done: impl FnOnce() + 'static) {
        let c = self.clone();
        let started = std::time::Instant::now();
        let mut stage = 0;
        let mut done = Some(done);
        glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
            assert!(
                started.elapsed().as_secs() < 30,
                "canvas timed out: {}",
                c.status.text()
            );
            if !c.ready.get() || c.has_pending() {
                return glib::ControlFlow::Continue;
            }
            let count = c.state.borrow().log(&current()).items().len();
            if verify {
                assert_eq!(count, 2, "canvas must survive an encrypted reopen");
            } else {
                match stage {
                    0 => {
                        assert_eq!(count, 0);
                        c.tool("pen");
                        c.begin(80., 100., 1, None);
                        c.motion(100., 0., None);
                        c.end();
                        stage += 1;
                        return glib::ControlFlow::Continue;
                    }
                    1 => {
                        assert_eq!(count, 1);
                        c.tool("rect");
                        c.begin(70., 90., 1, None);
                        c.motion(130., 30., None);
                        c.end();
                        assert!(c.state.borrow().selection.is_some());
                        let b = c.state.borrow().selection.as_ref().unwrap().bounds;
                        c.begin(b[2], b[3], 1, None);
                        c.motion(0., 20., None);
                        c.end();
                        assert!(
                            c.state.borrow().pending.last().unwrap().1["xf"]["s"]
                                .as_f64()
                                .unwrap()
                                > 1.,
                            "dragging the scale handle vertically must enlarge the selection"
                        );
                        c.action("copy");
                        stage += 1;
                        return glib::ControlFlow::Continue;
                    }
                    2 => {
                        assert_eq!(count, 2);
                        c.undo.emit_clicked();
                        stage += 1;
                        return glib::ControlFlow::Continue;
                    }
                    3 => {
                        assert_eq!(count, 1);
                        c.redo.emit_clicked();
                        stage += 1;
                        return glib::ControlFlow::Continue;
                    }
                    _ => {
                        assert_eq!(count, 2);
                        c.zoom(1.25, 100., 100.);
                        c.begin(300., 300., 2, None);
                        c.motion(20., 30., None);
                        c.end();
                    }
                }
            }
            if let Ok(path) = std::env::var("MEMORIOUS_UI_SCREENSHOT") {
                let paintable = gtk::WidgetPaintable::new(Some(&c.window));
                let snapshot = gtk::Snapshot::new();
                paintable.snapshot(&snapshot, c.window.width() as f64, c.window.height() as f64);
                if let Some(node) = snapshot.to_node() {
                    let surface = c.window.surface().unwrap();
                    let renderer = gtk::gsk::Renderer::for_surface(&surface).unwrap();
                    let texture = renderer.render_texture(&node, None);
                    texture.save_to_png(path).unwrap();
                    renderer.unrealize();
                }
            }
            std::fs::write(
                super::data_dir().join(if verify {
                    ".native-canvas-verified"
                } else {
                    ".native-canvas-tested"
                }),
                b"ok",
            )
            .unwrap();
            c.window.destroy();
            done.take().unwrap()();
            glib::ControlFlow::Break
        });
    }
}
