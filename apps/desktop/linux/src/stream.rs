//! Native feed presentation. No journal state lives here.
use adw::prelude::*;
use gtk::{glib, pango};
use std::{cell::RefCell, rc::Rc, time::Duration};

pub fn install_style(display: &gtk::gdk::Display) {
    let css = gtk::CssProvider::new();
    css.load_from_data(include_str!("stream.css"));
    gtk::style_context_add_provider_for_display(
        display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

pub fn watch_scroll(list: &gtk::Box, adjustment: &gtk::Adjustment) {
    let weak_list = list.downgrade();
    let target = adw::CallbackAnimationTarget::new(move |opacity| {
        fn fade(widget: &gtk::Widget, opacity: f64) {
            if widget.has_css_class("stream-fade") {
                widget.set_opacity(opacity);
            }
            let mut child = widget.first_child();
            while let Some(widget) = child {
                fade(&widget, opacity);
                child = widget.next_sibling();
            }
        }
        if let Some(list) = weak_list.upgrade() {
            fade(list.upcast_ref(), opacity);
        }
    });
    let animation = adw::TimedAnimation::new(list, 0.0, 0.0, 150, target);
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let weak_list = list.downgrade();
    adjustment.connect_value_changed(move |_| {
        let Some(list) = weak_list.upgrade() else {
            return;
        };
        if let Some(timer) = pending.borrow_mut().take() {
            timer.remove();
        }
        list.add_css_class("scrolling");
        animation.set_value_from(animation.value());
        animation.set_value_to(1.0);
        animation.set_duration(150);
        animation.play();
        let weak_list = list.downgrade();
        let animation = animation.clone();
        let pending2 = pending.clone();
        *pending.borrow_mut() = Some(glib::timeout_add_local_once(
            Duration::from_millis(900),
            move || {
                pending2.borrow_mut().take();
                if let Some(list) = weak_list.upgrade() {
                    list.remove_css_class("scrolling");
                    animation.set_value_from(animation.value());
                    animation.set_value_to(0.0);
                    animation.set_duration(450);
                    animation.play();
                }
            },
        ));
    });
}

pub fn fading(widget: &impl IsA<gtk::Widget>, list: &gtk::Box) {
    widget.add_css_class("stream-fade");
    widget.set_opacity(if list.has_css_class("scrolling") {
        1.0
    } else {
        0.0
    });
}

mod imp {
    use super::*;
    use gtk::subclass::prelude::*;
    use std::cell::{Cell, OnceCell};

    #[derive(Default)]
    pub struct CollapsibleText {
        pub container: OnceCell<gtk::Box>,
        pub label: OnceCell<gtk::Label>,
        pub toggle: OnceCell<gtk::Button>,
        pub expanded: Cell<bool>,
        pub full_text: RefCell<String>,
        pub pending_layout: Cell<bool>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for CollapsibleText {
        const NAME: &'static str = "MemoriousStreamText";
        type Type = super::CollapsibleText;
        type ParentType = gtk::Widget;
    }
    impl ObjectImpl for CollapsibleText {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            let container = gtk::Box::new(gtk::Orientation::Vertical, 2);
            container.set_parent(&*obj);
            obj.set_hexpand(true);
            let label = gtk::Label::builder()
                .wrap(true)
                .wrap_mode(pango::WrapMode::WordChar)
                .xalign(0.0)
                .yalign(0.0)
                .selectable(true)
                .lines(7)
                .ellipsize(pango::EllipsizeMode::End)
                .hexpand(true)
                .build();
            label.add_css_class("stream-text");
            let toggle = gtk::Button::with_label("more");
            toggle.add_css_class("flat");
            toggle.add_css_class("stream-more");
            toggle.set_halign(gtk::Align::Start);
            toggle.set_visible(false);
            let weak = obj.downgrade();
            toggle.connect_clicked(move |button| {
                let Some(obj) = weak.upgrade() else { return };
                let imp = obj.imp();
                let expanded = !imp.expanded.get();
                imp.expanded.set(expanded);
                let label = imp.label.get().unwrap();
                label.set_lines(if expanded { -1 } else { 7 });
                label.set_ellipsize(if expanded {
                    pango::EllipsizeMode::None
                } else {
                    pango::EllipsizeMode::End
                });
                button.set_label(if expanded { "less" } else { "more" });
                imp.update_text(label.width());
            });
            container.append(&label);
            container.append(&toggle);
            self.container.set(container).unwrap();
            self.label.set(label).unwrap();
            self.toggle.set(toggle).unwrap();
        }
        fn dispose(&self) {
            if let Some(container) = self.container.get() {
                container.unparent();
            }
        }
    }
    impl WidgetImpl for CollapsibleText {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.container.get().unwrap().measure(orientation, for_size)
        }
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.container
                .get()
                .unwrap()
                .allocate(width, height, baseline, None);
            // Changing the excerpt can change the requested height. Do that
            // after allocation, coalescing any resize passes into one update.
            if !self.pending_layout.replace(true) {
                let weak = self.obj().downgrade();
                glib::idle_add_local_once(move || {
                    if let Some(obj) = weak.upgrade() {
                        let imp = obj.imp();
                        imp.pending_layout.set(false);
                        imp.update_text(imp.label.get().unwrap().width());
                    }
                });
            }
        }
    }
    impl CollapsibleText {
        fn update_text(&self, width: i32) {
            let label = self.label.get().unwrap();
            let full_text = self.full_text.borrow();
            let layout = label.create_pango_layout(Some(&full_text));
            layout.set_width(width.max(1) * pango::SCALE);
            layout.set_wrap(pango::WrapMode::WordChar);
            let truncated = layout.line_count() > 7;
            let needed = self.expanded.get() || truncated;
            let shown = if !self.expanded.get() && truncated {
                // GtkLabel's line limit applies per paragraph. Derive a display
                // excerpt from Pango's seventh visual line so blank lines and
                // paragraph breaks count towards the same cap as wrapped text.
                let line = layout.line_readonly(6).unwrap();
                let start = line.start_index() as usize;
                let end = start + line.length() as usize;
                let mut last = full_text[start..end].trim_end().to_owned();
                let tail = label.create_pango_layout(None);
                loop {
                    tail.set_text(&format!("{last}…"));
                    if tail.pixel_size().0 <= width || last.is_empty() {
                        break;
                    }
                    last.pop();
                }
                format!("{}{last}…", &full_text[..start])
            } else {
                full_text.to_string()
            };
            let toggle = self.toggle.get().unwrap();
            if label.text().as_str() != shown {
                label.set_text(&shown);
            }
            if toggle.is_visible() != needed {
                toggle.set_visible(needed);
            }
        }
    }
}

glib::wrapper! {
    pub struct CollapsibleText(ObjectSubclass<imp::CollapsibleText>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}
impl CollapsibleText {
    pub fn new(text: &str, annotation: bool) -> Self {
        use gtk::subclass::prelude::*;
        let obj: Self = glib::Object::new();
        let label = obj.imp().label.get().unwrap();
        label.set_text(text);
        *obj.imp().full_text.borrow_mut() = text.to_owned();
        if annotation {
            label.add_css_class("stream-annotation");
        }
        obj
    }
}
