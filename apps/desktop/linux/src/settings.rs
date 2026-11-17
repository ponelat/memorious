//! Native settings navigation. Each pane keeps its header outside scrolling content.
use adw::prelude::*;
use gtk::glib;

const SECTIONS: [(&str, &str, &str); 3] = [
    ("appearance", "Appearance", "applications-graphics-symbolic"),
    (
        "background",
        "Background & PTT",
        "audio-input-microphone-symbolic",
    ),
    ("peers", "Peers & sync", "network-workgroup-symbolic"),
];

pub struct Panel {
    pub dialog: adw::Dialog,
    pub sidebar: gtk::ListBox,
    pub split: adw::NavigationSplitView,
}
pub struct WeakPanel {
    dialog: glib::WeakRef<adw::Dialog>,
    sidebar: glib::WeakRef<gtk::ListBox>,
    split: glib::WeakRef<adw::NavigationSplitView>,
}
impl WeakPanel {
    pub fn upgrade(&self) -> Option<Panel> {
        Some(Panel {
            dialog: self.dialog.upgrade()?,
            sidebar: self.sidebar.upgrade()?,
            split: self.split.upgrade()?,
        })
    }
}
impl Panel {
    pub fn new(dialog: adw::Dialog, sections: [gtk::Widget; 3]) -> Self {
        let pages = gtk::Stack::new();
        pages.set_widget_name("settings-sections");
        pages.set_hhomogeneous(false);
        pages.set_vhomogeneous(false);
        pages.set_vexpand(true);
        let sidebar = gtk::ListBox::new();
        sidebar.add_css_class("navigation-sidebar");
        sidebar.set_selection_mode(gtk::SelectionMode::Single);
        for ((name, title, icon), page) in SECTIONS.iter().zip(sections.iter()) {
            pages.add_titled(page, Some(name), title);
            let row = gtk::ListBoxRow::new();
            row.set_widget_name(name);
            let item = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            item.append(&gtk::Image::from_icon_name(icon));
            item.append(&gtk::Label::new(Some(title)));
            row.set_child(Some(&item));
            sidebar.append(&row);
        }
        let sidebar_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&sidebar)
            .build();
        let sidebar_layout = adw::ToolbarView::new();
        sidebar_layout.add_top_bar(&adw::HeaderBar::new());
        sidebar_layout.set_content(Some(&sidebar_scroll));
        let content_layout = adw::ToolbarView::new();
        content_layout.add_top_bar(&adw::HeaderBar::new());
        content_layout.set_content(Some(&pages));
        let content = adw::NavigationPage::new(&content_layout, "Appearance");
        content.set_tag(Some("content"));
        let split = adw::NavigationSplitView::new();
        split.set_min_sidebar_width(190.0);
        split.set_max_sidebar_width(220.0);
        split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar_layout, "Settings")));
        split.set_content(Some(&content));
        // Native split-view navigation supplies the back button at narrow widths.
        // https://gnome.pages.gitlab.gnome.org/libadwaita/doc/1.5/class.NavigationSplitView.html
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            600.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        dialog.add_breakpoint(breakpoint);
        dialog.set_size_request(360, 300);
        dialog.set_child(Some(&split));
        let weak_pages = pages.downgrade();
        let weak_content = content.downgrade();
        let weak_split = split.downgrade();
        sidebar.connect_row_selected(move |_, row| {
            let (Some(row), Some(split), Some(pages), Some(content)) = (
                row,
                weak_split.upgrade(),
                weak_pages.upgrade(),
                weak_content.upgrade(),
            ) else {
                return;
            };
            if !split.is_collapsed() {
                let (name, title, _) = SECTIONS[row.index() as usize];
                pages.set_visible_child_name(name);
                content.set_title(title);
            }
        });
        let weak_pages = pages.downgrade();
        let weak_content = content.downgrade();
        let weak_split = split.downgrade();
        sidebar.connect_row_activated(move |_, row| {
            let (Some(split), Some(pages), Some(content)) = (
                weak_split.upgrade(),
                weak_pages.upgrade(),
                weak_content.upgrade(),
            ) else {
                return;
            };
            let (name, title, _) = SECTIONS[row.index() as usize];
            pages.set_visible_child_name(name);
            content.set_title(title);
            split.set_show_content(true);
        });
        Self {
            dialog,
            sidebar,
            split,
        }
    }
    pub fn show_section(&self, section: &str) {
        let index = SECTIONS
            .iter()
            .position(|(name, _, _)| *name == section)
            .unwrap_or(0);
        let row = self.sidebar.row_at_index(index as i32).unwrap();
        self.sidebar.select_row(Some(&row));
        self.sidebar.emit_by_name::<()>("row-activated", &[&row]);
    }
    pub fn downgrade(&self) -> WeakPanel {
        WeakPanel {
            dialog: self.dialog.downgrade(),
            sidebar: self.sidebar.downgrade(),
            split: self.split.downgrade(),
        }
    }
}

pub fn page(body: &gtk::Box) -> gtk::Widget {
    let clamp = adw::Clamp::builder().maximum_size(600).child(body).build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .vexpand(true)
        .build()
        .upcast()
}
