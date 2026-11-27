#[path = "canvas.rs"]
mod canvas;
#[path = "settings.rs"]
mod settings;
#[path = "stream.rs"]
mod stream;
use adw::prelude::*;
use anyhow::Context;
use gtk::{gio, glib};
use memorious_linux::{
    background::{self, Session},
    recording::{self, Recording},
    DesktopJournal,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    os::fd::AsRawFd,
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
};
use zeroize::Zeroizing;

type Job = Box<dyn FnOnce(&tokio::runtime::Runtime, &mut Option<Session>) + Send>;

#[derive(Clone)]
struct Worker(mpsc::Sender<Job>);
impl Worker {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Rust runtime");
            let mut journal = None;
            for job in rx {
                let _guard = rt.enter();
                job(&rt, &mut journal);
            }
            if let Some(journal) = journal {
                rt.block_on(journal.close());
            }
        });
        Self(tx)
    }
    fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&tokio::runtime::Runtime, &mut Option<Session>) -> anyhow::Result<T>
            + Send
            + 'static,
        done: impl FnOnce(Result<T, String>) + 'static,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sent = self.0.send(Box::new(move |rt, journal| {
            let _ = tx.send(job(rt, journal).map_err(|e| format!("{e:#}")));
        }));
        glib::MainContext::default().spawn_local(async move {
            done(if sent.is_err() {
                Err("Journal worker stopped".into())
            } else {
                rx.await
                    .unwrap_or_else(|_| Err("Journal worker stopped".into()))
            });
        });
    }
}

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("app.memorious")
        .build();
    app.connect_activate(|app| {
        let manager = adw::StyleManager::default();
        match std::fs::read_to_string(appearance_path()).as_deref() {
            Ok("dark") => manager.set_color_scheme(adw::ColorScheme::ForceDark),
            Ok("light") => manager.set_color_scheme(adw::ColorScheme::ForceLight),
            _ => {}
        }
        gtk::Window::set_default_icon_name("memorious");
        stream::install_style(&gtk::gdk::Display::default().expect("display"));
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Memorious")
            .default_width(820)
            .default_height(760)
            .build();
        let ui = Rc::new(Ui {
            worker: Worker::new(),
            controls: Worker::new(),
            enrichment: Worker::new(),
            ptt: gtk::Button::with_label("PTT"),
            window,
            stack: gtk::Stack::new(),
            toast: adw::ToastOverlay::new(),
            list: gtk::Box::new(gtk::Orientation::Vertical, 0),
            search: gtk::SearchEntry::new(),
            trash: gtk::ToggleButton::with_label("Trash"),
            composer: gtk::TextView::new(),
            record: gtk::Button::from_icon_name("audio-input-microphone-symbolic"),
            cancel_recording: gtk::Button::with_label("Cancel recording"),
            recording_status: gtk::Label::new(None),
            recording: RefCell::new(None),
            pending_audio: RefCell::new(None),
            recording_busy: Cell::new(false),
            recording_started: Cell::new(None),
            limit: Cell::new(50),
            generation: Cell::new(0),
            loading: Cell::new(false),
            last_entries: RefCell::new(Value::Null),
            photo_cache: RefCell::new(HashMap::new()),
            settings_panel: RefCell::new(None),
            canvas: RefCell::new(None),
        });
        ui.build();
        let closing = Rc::new(Cell::new(false));
        let ui_close = ui.clone();
        ui.window.connect_close_request(move |window| {
            if ui_close
                .canvas
                .borrow()
                .as_ref()
                .is_some_and(|c| c.has_pending())
            {
                ui_close.error("Wait for canvas ink to save before closing.");
                return glib::Propagation::Stop;
            }
            if let Some(canvas) = ui_close.canvas.borrow_mut().take() {
                canvas.window.destroy();
            }
            if ui_close.recording_busy.get()
                || ui_close.recording.borrow().is_some()
                || ui_close.pending_audio.borrow().is_some()
            {
                ui_close.error("Save or cancel the recording before closing.");
                return glib::Propagation::Stop;
            }
            if closing.replace(true) {
                return glib::Propagation::Stop;
            }
            window.set_sensitive(false);
            let window = window.clone();
            ui_close.worker.call(
                move |rt, state| {
                    if let Some(journal) = state.take() {
                        rt.block_on(journal.close());
                    }
                    Ok(())
                },
                move |_| window.destroy(),
            );
            glib::Propagation::Stop
        });
        ui.window.present();
        if matches!(
            std::env::var("MEMORIOUS_UI_SMOKE").as_deref(),
            Ok("composer" | "appearance")
        ) {
            smoke_composer(ui.clone());
        } else if std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("stream") {
            smoke_stream(ui.clone());
        } else if std::env::var_os("MEMORIOUS_UI_SMOKE").is_some() {
            assert_eq!(ui.stack.visible_child_name().as_deref(), Some("setup"));
            if matches!(
                std::env::var("MEMORIOUS_UI_SMOKE").as_deref(),
                Ok("capture"
                    | "verify"
                    | "settings"
                    | "peers"
                    | "recording"
                    | "audio-verify"
                    | "canvas"
                    | "canvas-verify")
            ) {
                smoke_capture(
                    ui.clone(),
                    matches!(
                        std::env::var("MEMORIOUS_UI_SMOKE").as_deref(),
                        Ok("verify" | "audio-verify" | "canvas-verify")
                    ),
                );
            } else {
                let app = app.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                    app.quit()
                });
            }
        }
    });
    app.run()
}

fn appearance_path() -> PathBuf {
    glib::user_config_dir().join("memorious").join("appearance")
}
fn data_dir() -> PathBuf {
    std::env::var_os("MEMORIOUS_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| glib::user_data_dir().join("app.memorious").join("journal"))
}
fn keyring() -> Option<keyring::Entry> {
    if std::env::var_os("MEMORIOUS_NO_KEYRING").is_some()
        || std::env::var_os("MEMORIOUS_DATA_DIR").is_some()
    {
        return None;
    }
    keyring::Entry::new("app.memorious", "master-password").ok()
}
fn padded(b: &gtk::Box) {
    b.set_margin_top(18);
    b.set_margin_bottom(18);
    b.set_margin_start(18);
    b.set_margin_end(18);
}
fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .build()
}
struct Ui {
    worker: Worker,
    controls: Worker,
    enrichment: Worker,
    ptt: gtk::Button,
    window: adw::ApplicationWindow,
    stack: gtk::Stack,
    toast: adw::ToastOverlay,
    list: gtk::Box,
    search: gtk::SearchEntry,
    trash: gtk::ToggleButton,
    composer: gtk::TextView,
    record: gtk::Button,
    cancel_recording: gtk::Button,
    recording_status: gtk::Label,
    recording: RefCell<Option<Recording>>,
    pending_audio: RefCell<Option<std::fs::File>>,
    recording_busy: Cell<bool>,
    recording_started: Cell<Option<std::time::Instant>>,
    limit: Cell<usize>,
    generation: Cell<u64>,
    loading: Cell<bool>,
    last_entries: RefCell<Value>,
    photo_cache: RefCell<HashMap<String, gtk::gdk::Texture>>,
    settings_panel: RefCell<Option<settings::WeakPanel>>,
    canvas: RefCell<Option<Rc<canvas::Canvas>>>,
}
impl Ui {
    fn error(&self, error: &str) {
        self.toast.add_toast(adw::Toast::new(error));
    }
    fn build(self: &Rc<Self>) {
        self.toast.set_child(Some(&self.stack));
        self.window.set_content(Some(&self.toast));
        let setup = gtk::Box::new(gtk::Orientation::Vertical, 18);
        padded(&setup);
        let header = adw::HeaderBar::new();
        setup.append(&header);
        let page = adw::StatusPage::builder()
            .title("Memorious")
            .description("A quiet place to capture what’s happening.")
            .icon_name("accessories-text-editor-symbolic")
            .vexpand(true)
            .build();
        let form = gtk::Box::new(gtk::Orientation::Vertical, 12);
        form.set_halign(gtk::Align::Center);
        form.set_size_request(320, -1);
        let exists = data_dir().join("db.sqlite").exists();
        let password = gtk::PasswordEntry::builder()
            .placeholder_text("Master password")
            .show_peek_icon(true)
            .build();
        let confirm = gtk::PasswordEntry::builder()
            .placeholder_text("Confirm password")
            .show_peek_icon(true)
            .visible(!exists)
            .build();
        let ticket = gtk::Entry::builder()
            .placeholder_text("Pairing ticket (to join a journal)")
            .visible(!exists)
            .build();
        form.append(&password);
        form.append(&confirm);
        form.append(&ticket);
        let write_only =
            gtk::CheckButton::with_label("Write-only peer (join without pulling history)");
        write_only.set_visible(!exists);
        write_only.set_tooltip_text(Some("Push new captures only. This mode cannot verify the network's master password; enter the same password as the other peers."));
        form.append(&write_only);
        let open = gtk::Button::with_label(if exists {
            "Unlock"
        } else {
            "Create or join journal"
        });
        open.add_css_class("suggested-action");
        form.append(&open);
        let error = label("");
        error.add_css_class("error");
        form.append(&error);
        page.set_child(Some(&form));
        setup.append(&page);
        self.stack.add_named(&setup, Some("setup"));
        let ui = self.clone();
        let pass = password.clone();
        open.connect_clicked(move |button| {
            let pw = Zeroizing::new(pass.text().to_string());
            if pw.is_empty() {
                error.set_text("Enter the journal’s master password.");
                return;
            }
            if !exists && pw.as_str() != confirm.text().as_str() {
                error.set_text("Passwords do not match.");
                return;
            }
            button.set_sensitive(false);
            error.set_text("Opening journal…");
            let join = ticket.text().to_string();
            let capture_only = write_only.is_active();
            if capture_only && join.trim().is_empty() {
                error.set_text("Enter a ticket to join as a write-only peer.");
                button.set_sensitive(true);
                return;
            }

            let dir = data_dir();
            let ui2 = ui.clone();
            let button = button.clone();
            let error = error.clone();
            let pass = pass.clone();
            let confirm = confirm.clone();
            let ticket = ticket.clone();
            ui.worker.call(
                move |rt, state| {
                    let journal = rt.block_on(async {
                        if exists {
                            DesktopJournal::open(&dir, &pw).await
                        } else if join.trim().is_empty() {
                            DesktopJournal::create(&dir, &pw).await
                        } else if capture_only {
                            DesktopJournal::join_write_only(&dir, join.trim(), &pw).await
                        } else {
                            DesktopJournal::join(&dir, join.trim(), &pw).await
                        }
                    })?;
                    if let Some(entry) = keyring() {
                        let _ = entry.set_password(&pw);
                    }
                    *state = Some(Session::embedded(dir, pw, journal));
                    Ok(())
                },
                move |result| {
                    button.set_sensitive(true);
                    match result {
                        Ok(()) => {
                            pass.set_text("");
                            confirm.set_text("");
                            ticket.set_text("");
                            ui2.ready();
                        }
                        Err(e) => error.set_text(&e),
                    }
                },
            );
        });
        let button = open.clone();
        password.connect_activate(move |_| button.emit_clicked());
        self.build_journal();
        self.stack.set_visible_child_name("setup");
        if exists && std::env::var_os("MEMORIOUS_UI_SMOKE").is_none() {
            let ui = self.clone();
            let dir = data_dir();
            self.worker.call(
                move |rt, state| {
                    if let Ok(session) = rt.block_on(Session::attach(&dir)) {
                        *state = Some(session);
                        return Ok(true);
                    }
                    let Some(password) = keyring()
                        .and_then(|k| k.get_password().ok())
                        .map(Zeroizing::new)
                    else {
                        return Ok(false);
                    };
                    let journal = rt.block_on(DesktopJournal::open(&dir, &password))?;
                    *state = Some(Session::embedded(dir, password, journal));
                    Ok(true)
                },
                move |result| {
                    if let Ok(true) = result {
                        ui.ready();
                    }
                },
            );
        }
    }
    fn ready(self: &Rc<Self>) {
        self.stack.set_visible_child_name("journal");
        self.refresh();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_millis(700), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !ui.window.is_sensitive() {
                return glib::ControlFlow::Continue;
            }
            let dir = data_dir();
            let weak = Rc::downgrade(&ui);
            ui.controls.call(move |_, _| Ok(background::ptt_status(&dir)), move |result| {
                let Some(ui) = weak.upgrade() else { return; };
                if let Ok(status) = result {
                    let state = status["state"].as_str().unwrap_or("stopped");
                    ui.ptt.set_label(match state { "recording" => "● Recording", "transcribing" => "Transcribing…", "waiting" | "idle" => "PTT ready", "failed" => "PTT error", _ => "PTT off" });
                    if state == "recording" { ui.ptt.add_css_class("destructive-action"); } else { ui.ptt.remove_css_class("destructive-action"); }
                    ui.ptt.set_tooltip_text(Some(status["error"].as_str().unwrap_or("Click to start or stop recording; the configured global key records while held")));
                }
            });
            glib::ControlFlow::Continue
        });
        let ui = Rc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_secs(3), move || {
            let Some(ui) = ui.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if ui.window.is_sensitive() && !ui.loading.get() {
                ui.refresh();
            }
            glib::ControlFlow::Continue
        });
    }
    fn build_journal(self: &Rc<Self>) {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new("Memorious", "")));
        header.pack_end(&self.ptt);
        self.ptt
            .set_tooltip_text(Some("Start or stop push-to-talk recording"));
        let settings = gtk::Button::from_icon_name("preferences-system-symbolic");
        settings.set_tooltip_text(Some("Settings"));
        header.pack_end(&settings);
        let ui = self.clone();
        settings.connect_clicked(move |_| ui.settings("appearance"));
        let ui = self.clone();
        self.ptt.connect_clicked(move |_| {
            let dir = data_dir();
            let ui2 = ui.clone();
            ui.controls.call(
                move |_, _| background::ptt_command(&dir, "toggle"),
                move |result| {
                    if let Err(error) = result {
                        ui2.error(&error);
                        ui2.settings("background");
                    }
                },
            );
        });
        let canvas = gtk::Button::with_label("Canvas");
        canvas.set_tooltip_text(Some("Open the canvas"));
        header.pack_start(&canvas);
        let ui = self.clone();
        canvas.connect_clicked(move |_| ui.open_canvas());
        header.pack_start(&self.trash);
        body.append(&header);
        self.search
            .set_placeholder_text(Some("Search your journal"));
        self.search.set_margin_start(18);
        self.search.set_margin_end(18);
        body.append(&self.search);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        scroll.add_css_class("stream-surface");
        padded(&self.list);
        self.list.add_css_class("journal-stream");
        stream::watch_scroll(&self.list, &scroll.vadjustment());
        let clamp = adw::Clamp::builder()
            .maximum_size(720)
            .child(&self.list)
            .build();
        scroll.set_child(Some(&clamp));
        let ui = self.clone();
        scroll.connect_edge_reached(move |_, edge| {
            if edge == gtk::PositionType::Bottom
                && !ui.loading.get()
                && ui.last_entries.borrow()["entries"]
                    .as_array()
                    .map(|e| e.len())
                    == Some(ui.limit.get())
            {
                ui.limit.set(ui.limit.get() + 50);
                ui.refresh();
            }
        });
        body.append(&scroll);
        let capture = gtk::Box::new(gtk::Orientation::Vertical, 8);
        padded(&capture);
        self.composer.set_wrap_mode(gtk::WrapMode::WordChar);
        self.composer.set_top_margin(10);
        self.composer.set_bottom_margin(10);
        self.composer.set_left_margin(10);
        self.composer.set_right_margin(10);
        let editor = gtk::ScrolledWindow::builder()
            .min_content_height(72)
            .max_content_height(180)
            .propagate_natural_height(true)
            .child(&self.composer)
            .build();
        editor.set_hexpand(true);
        let hint = gtk::Label::builder()
            .label("What’s happening?")
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_start(14)
            .margin_top(12)
            .can_target(false)
            .build();
        hint.add_css_class("composer-hint");
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&editor));
        overlay.add_overlay(&hint);
        overlay.set_hexpand(true);
        self.composer
            .buffer()
            .connect_changed(move |buffer| hint.set_visible(buffer.char_count() == 0));
        self.composer.add_css_class("composer-text");
        let field = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        field.add_css_class("capture-field");
        field.append(&overlay);
        self.record.set_tooltip_text(Some("Record audio"));
        self.record.add_css_class("flat");
        self.record.add_css_class("circular");
        self.record.set_valign(gtk::Align::Center);
        self.record.set_margin_end(8);
        self.record.set_size_request(44, 44);
        field.append(&self.record);
        capture.append(&field);
        let ui = self.clone();
        self.record.connect_clicked(move |_| ui.toggle_recording());
        let ui = self.clone();
        self.cancel_recording.connect_clicked(move |_| {
            let recording = ui.recording.borrow_mut().take();
            ui.pending_audio.borrow_mut().take();
            ui.controls.call(
                move |_, _| {
                    drop(recording);
                    Ok(())
                },
                |_| {},
            );
            ui.reset_recording();
        });
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let photo = gtk::Button::with_label("Photo");
        let voice = gtk::Button::with_label("Voice file");
        let music = gtk::Button::with_label("Music file");
        for button in [&photo, &voice, &music] {
            actions.append(button);
        }
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&spacer);
        let send = gtk::Button::with_label("Capture");
        send.add_css_class("suggested-action");
        actions.append(&send);
        capture.append(&actions);
        let feedback = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        self.recording_status.set_hexpand(true);
        self.recording_status.set_xalign(0.0);
        self.recording_status.add_css_class("dim-label");
        self.cancel_recording.add_css_class("flat");
        self.cancel_recording.set_visible(false);
        feedback.append(&self.recording_status);
        feedback.append(&self.cancel_recording);
        capture.append(&feedback);
        body.append(&capture);
        self.stack.add_named(&body, Some("journal"));
        let keys = gtk::EventControllerKey::new();
        let capture_button = send.clone();
        let search = self.search.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                if key == gtk::gdk::Key::Return {
                    capture_button.emit_clicked();
                    return glib::Propagation::Stop;
                }
                if key == gtk::gdk::Key::f {
                    search.grab_focus();
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        self.window.add_controller(keys);
        let ui = self.clone();
        send.connect_clicked(move |button| {
            let buffer = ui.composer.buffer();
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            if text.trim().is_empty() {
                return;
            }
            button.set_sensitive(false);
            ui.composer.set_editable(false);
            let ui2 = ui.clone();
            let button = button.clone();
            ui.worker.call(
                move |rt, state| rt.block_on(state.as_ref().unwrap().capture_text(&text)),
                move |result| {
                    button.set_sensitive(true);
                    ui2.composer.set_editable(true);
                    match result {
                        Ok(_) => {
                            ui2.composer.buffer().set_text("");
                            ui2.refresh();
                        }
                        Err(e) => ui2.error(&e),
                    }
                },
            );
        });
        for (button, kind) in [(photo, "photo"), (voice, "voice"), (music, "music")] {
            let ui = self.clone();
            button.connect_clicked(move |_| ui.import(kind));
        }
        let ui = self.clone();
        self.search.connect_search_changed(move |_| {
            ui.limit.set(50);
            ui.refresh();
        });
        let ui = self.clone();
        self.trash.connect_toggled(move |_| {
            ui.limit.set(50);
            ui.refresh();
        });
    }
    fn refresh(self: &Rc<Self>) {
        // The presentation fixture deliberately has no journal. Search signals
        // can arrive after it is mounted or resized.
        if matches!(
            std::env::var("MEMORIOUS_UI_SMOKE").as_deref(),
            Ok("stream" | "composer" | "appearance")
        ) {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.loading.set(true);
        let query = self.search.text().to_string();
        let trash = self.trash.is_active();
        let limit = self.limit.get();
        let ui = self.clone();
        self.worker.call(
            move |rt, state| rt.block_on(state.as_ref().unwrap().entries(&query, trash, limit)),
            move |result| {
                if generation != ui.generation.get() {
                    return;
                }
                ui.loading.set(false);
                match result {
                    Ok(entries) => {
                        let value = json!({"trash": trash, "entries": entries, "limit": limit});
                        if *ui.last_entries.borrow() == value {
                            return;
                        }
                        *ui.last_entries.borrow_mut() = value;
                        while let Some(child) = ui.list.first_child() {
                            ui.list.remove(&child);
                        }
                        if entries.is_empty() {
                            ui.list.append(
                                &adw::StatusPage::builder()
                                    .title("No entries")
                                    .description(if trash {
                                        "Redacted captures appear here."
                                    } else {
                                        "Capture a thought, a photo, or a recording."
                                    })
                                    .icon_name("document-new-symbolic")
                                    .build(),
                            );
                        }
                        let mut day = String::new();
                        for entry in &entries {
                            let time = glib::DateTime::from_unix_local(
                                entry["recorded_at"].as_i64().unwrap_or(0) / 1000,
                            )
                            .ok();
                            let date = time
                                .as_ref()
                                .and_then(|t| t.format("%A, %e %B %Y").ok())
                                .map(|s| s.to_string())
                                .unwrap_or_default();
                            if day != date {
                                let heading = label(&date);
                                heading.add_css_class("stream-day");
                                ui.list.append(&heading);
                                day = date;
                            }
                            ui.entry(
                                entry,
                                trash,
                                time.and_then(|t| t.format("%H:%M").ok())
                                    .map(|s| s.to_string())
                                    .unwrap_or_default(),
                            );
                        }
                        if entries.len() == limit {
                            let more = gtk::Button::with_label("Load older entries");
                            let ui2 = ui.clone();
                            more.connect_clicked(move |_| {
                                ui2.limit.set(ui2.limit.get() + 50);
                                ui2.refresh();
                            });
                            ui.list.append(&more);
                        }
                    }
                    Err(e) => ui.error(&e),
                }
            },
        );
    }
    fn entry(self: &Rc<Self>, entry: &Value, trash: bool, time: String) {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.add_css_class("stream-row");
        row.append(&line);
        let timestamp = label(&time);
        timestamp.add_css_class("stream-time");
        timestamp.set_width_chars(5);
        timestamp.set_valign(gtk::Align::Start);
        timestamp.set_margin_top(2);
        stream::fading(&timestamp, &self.list);
        line.append(&timestamp);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        content.set_hexpand(true);
        line.append(&content);
        let meta = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        meta.add_css_class("stream-actions");
        meta.set_valign(gtk::Align::Start);
        let copyable = entry["text"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| entry["annotation"].as_str().filter(|s| !s.is_empty()));
        if let Some(text) = copyable {
            let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
            copy.add_css_class("flat");
            copy.set_tooltip_text(Some("Copy text"));
            let text = text.to_owned();
            copy.connect_clicked(move |button| button.clipboard().set_text(&text));
            meta.append(&copy);
        }
        if !trash {
            let redact = gtk::Button::from_icon_name("user-trash-symbolic");
            redact.add_css_class("flat");
            redact.set_tooltip_text(Some("Redact entry"));
            let id = entry["event_id"].as_str().unwrap_or("").to_owned();
            let ui = self.clone();
            redact.connect_clicked(move |_| {
                let dialog = adw::AlertDialog::builder().heading("Redact this entry?").body("It will be hidden from the stream on every peer. The event history is preserved.").build();
                dialog.add_responses(&[("cancel", "Cancel"), ("redact", "Redact")]); dialog.set_response_appearance("redact", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel")); dialog.set_close_response("cancel");
                let ui2 = ui.clone(); let id = id.clone();
                dialog.connect_response(None, move |_, response| { if response == "redact" {
                    let ui3 = ui2.clone(); let id = id.clone(); ui2.worker.call(move |rt, state| rt.block_on(state.as_ref().unwrap().redact(&id)), move |result| {
                        if let Err(e) = result { ui3.error(&e); } ui3.refresh();
                    });
                }}); dialog.present(Some(&ui.window));
            });
            meta.append(&redact);
        }
        line.append(&meta);
        if let Some(text) = entry["text"].as_str().filter(|s| !s.is_empty()) {
            content.append(&stream::CollapsibleText::new(text, false));
        }
        if let Some(hash) = entry["media"]["hash"].as_str() {
            let kind = entry["kind"].as_str().unwrap_or("audio");
            if kind == "photo" {
                let picture = gtk::Picture::new();
                picture.set_can_shrink(true);
                picture.set_content_fit(gtk::ContentFit::Contain);
                picture.set_size_request(-1, 320);
                picture.add_css_class("card");
                content.append(&picture);
                if let Some(texture) = self.photo_cache.borrow().get(hash).cloned() {
                    picture.set_paintable(Some(&texture));
                } else {
                    let hash = hash.to_owned();
                    let ui = self.clone();
                    let picture2 = picture.clone();
                    let load_hash = hash.clone();
                    let worker = ui.worker.clone();
                    worker.call(
                        move |rt, state| rt.block_on(state.as_ref().unwrap().media(&load_hash)),
                        move |result| match result {
                            Ok(bytes) => {
                                let bytes = glib::Bytes::from_owned(bytes);
                                match gtk::gdk::Texture::from_bytes(&bytes) {
                                    Ok(texture) => {
                                        ui.photo_cache.borrow_mut().insert(hash, texture.clone());
                                        picture2.set_paintable(Some(&texture));
                                    }
                                    Err(error) => {
                                        ui.error(&format!("Could not display photo: {error}"))
                                    }
                                }
                            }
                            Err(error) => ui.error(&error),
                        },
                    );
                }
            } else if kind == "audio" {
                let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let play = gtk::Button::with_label(if entry["audio_kind"] == "music" {
                    "play music"
                } else {
                    "play audio"
                });
                play.add_css_class("stream-audio");
                play.add_css_class("flat");
                let pause = gtk::Button::with_label("Pause");
                pause.add_css_class("flat");
                pause.set_sensitive(false);
                controls.append(&play);
                controls.append(&pause);
                let playback: Rc<RefCell<Option<(gtk::MediaFile, std::fs::File)>>> =
                    Rc::new(RefCell::new(None));
                let saved = playback.clone();
                pause.connect_clicked(move |_| {
                    if let Some((media, _fd)) = saved.borrow().as_ref() {
                        media.pause();
                    }
                });
                let saved = playback.clone();
                let ui = self.clone();
                let hash = hash.to_owned();
                play.connect_clicked(move |button| {
                    if let Some((media, _fd)) = saved.borrow().as_ref() {
                        if media.is_ended() {
                            media.seek(0);
                        }
                        media.play();
                        return;
                    }
                    button.set_sensitive(false);
                    let button = button.clone();
                    let pause = pause.clone();
                    let saved = saved.clone();
                    let ui2 = ui.clone();
                    let hash = hash.clone();
                    ui.worker.call(
                        move |rt, state| rt.block_on(state.as_ref().unwrap().media(&hash)),
                        move |result| match result {
                            Ok(bytes) => {
                                let bytes = glib::Bytes::from_owned(bytes);
                                match anonymous_media_file(&bytes) {
                                    Ok(media_fd) => {
                                        let file = gio::File::for_path(format!(
                                            "/proc/self/fd/{}",
                                            media_fd.as_raw_fd()
                                        ));
                                        let media = gtk::MediaFile::for_file(&file);
                                        let weak_ui = Rc::downgrade(&ui2);
                                        media.connect_error_notify(move |media| {
                                            if let (Some(ui), Some(error)) =
                                                (weak_ui.upgrade(), media.error())
                                            {
                                                ui.error(&format!(
                                                    "Could not play recording: {error}"
                                                ));
                                            }
                                        });
                                        saved.replace(Some((media.clone(), media_fd)));
                                        button.set_sensitive(true);
                                        pause.set_sensitive(true);
                                        media.play();
                                    }
                                    Err(error) => {
                                        button.set_sensitive(true);
                                        ui2.error(&format!("Could not open recording: {error}"));
                                    }
                                }
                            }
                            Err(error) => {
                                button.set_sensitive(true);
                                ui2.error(&error);
                            }
                        },
                    );
                });
                content.append(&controls);
            } else {
                let open = gtk::Button::with_label("Play video");
                let hash = hash.to_owned();
                let kind = kind.to_owned();
                let ui = self.clone();
                open.connect_clicked(move |button| {
                    button.set_sensitive(false);
                    let ui2 = ui.clone();
                    let button = button.clone();
                    let hash = hash.clone();
                    let kind = kind.clone();
                    ui.worker.call(
                        move |rt, state| rt.block_on(state.as_ref().unwrap().media(&hash)),
                        move |result| {
                            button.set_sensitive(true);
                            match result {
                                Ok(bytes) => ui2.show_media(&kind, bytes),
                                Err(error) => ui2.error(&error),
                            }
                        },
                    );
                });
                content.append(&open);
            }
        }
        if entry["audio_kind"] != "music" {
            if let Some(text) = entry["annotation"].as_str().filter(|s| !s.is_empty()) {
                content.append(&stream::CollapsibleText::new(text, true));
            }
        }
        let rule = gtk::Separator::new(gtk::Orientation::Horizontal);
        rule.add_css_class("stream-rule");
        stream::fading(&rule, &self.list);
        row.append(&rule);
        self.list.append(&row);
    }
    fn reset_recording(&self) {
        self.recording_started.set(None);
        self.recording_busy.set(false);
        self.record.set_sensitive(true);
        self.record.set_icon_name("audio-input-microphone-symbolic");
        self.record.set_tooltip_text(Some("Record audio"));
        self.record.remove_css_class("destructive-action");
        self.recording_status.set_text("");
        self.cancel_recording.set_visible(false);
        self.cancel_recording.set_sensitive(true);
    }
    fn recording_work(&self, message: &str) {
        self.recording_busy.set(true);
        self.record.set_sensitive(false);
        self.cancel_recording.set_sensitive(false);
        self.recording_status.set_text(message);
    }
    fn toggle_recording(self: &Rc<Self>) {
        if self.recording_busy.get() {
            return;
        }
        if self.pending_audio.borrow().is_some() {
            self.save_recording();
            return;
        }
        let current = self.recording.borrow_mut().take();
        if let Some(recording) = current {
            self.recording_work("Saving audio…");
            let ui = self.clone();
            self.controls.call(
                move |_, _| recording.finish(),
                move |result| match result {
                    Ok(file) => {
                        *ui.pending_audio.borrow_mut() = Some(file);
                        ui.save_recording();
                    }
                    Err(error) => {
                        ui.reset_recording();
                        ui.error(&error);
                    }
                },
            );
            return;
        }
        self.recording_work("Starting microphone…");
        let ui = self.clone();
        self.controls.call(
            move |_, _| Recording::start(),
            move |result| match result {
                Ok(recording) => {
                    *ui.recording.borrow_mut() = Some(recording);
                    ui.recording_started.set(Some(std::time::Instant::now()));
                    ui.recording_busy.set(false);
                    ui.record.set_sensitive(true);
                    ui.record.set_icon_name("media-playback-stop-symbolic");
                    ui.record.set_tooltip_text(Some("Stop and save audio"));
                    ui.record.add_css_class("destructive-action");
                    ui.cancel_recording.set_visible(true);
                    ui.cancel_recording.set_sensitive(true);
                    ui.recording_status
                        .set_text("● Recording 0:00 — click stop to save");
                    let weak = Rc::downgrade(&ui);
                    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                        let Some(ui) = weak.upgrade() else {
                            return glib::ControlFlow::Break;
                        };
                        let Some(started) = ui.recording_started.get() else {
                            return glib::ControlFlow::Break;
                        };
                        if ui.recording_busy.get() {
                            return glib::ControlFlow::Break;
                        }
                        let running = ui
                            .recording
                            .borrow_mut()
                            .as_mut()
                            .is_some_and(|r| r.is_running().unwrap_or(false));
                        if !running {
                            ui.recording_status
                                .set_text("Microphone stopped — click stop to save recorded audio");
                            return glib::ControlFlow::Break;
                        }
                        let elapsed = started.elapsed().as_secs();
                        ui.recording_status.set_text(&format!(
                            "● Recording {}:{:02} — click stop to save",
                            elapsed / 60,
                            elapsed % 60
                        ));
                        glib::ControlFlow::Continue
                    });
                }
                Err(error) => {
                    ui.reset_recording();
                    ui.error(&error);
                }
            },
        );
    }
    fn save_recording(self: &Rc<Self>) {
        let file = match self.pending_audio.borrow().as_ref().unwrap().try_clone() {
            Ok(file) => file,
            Err(error) => {
                self.recording_busy.set(false);
                self.record.set_sensitive(true);
                self.cancel_recording.set_sensitive(true);
                self.error(&error.to_string());
                return;
            }
        };
        self.recording_work("Saving audio…");
        let ui = self.clone();
        self.worker.call(
            move |rt, state| {
                let bytes = std::fs::read(recording::file_path(&file))?;
                rt.block_on(
                    state
                        .as_ref()
                        .context("Journal is locked")?
                        .capture_media("voice", bytes),
                )
            },
            move |result| match result {
                Ok(entry) => {
                    let file = ui.pending_audio.borrow_mut().take().unwrap();
                    let path = PathBuf::from(recording::file_path(&file));
                    ui.reset_recording();
                    ui.refresh();
                    ui.transcribe_voice_source(
                        entry["event_id"].as_str().unwrap_or("").to_owned(),
                        path,
                        Some(file),
                    );
                }
                Err(error) => {
                    ui.recording_busy.set(false);
                    ui.record.set_sensitive(true);
                    ui.record.set_icon_name("document-save-symbolic");
                    ui.record.set_tooltip_text(Some("Retry saving audio"));
                    ui.cancel_recording.set_sensitive(true);
                    ui.recording_status
                        .set_text("Audio is still held in memory — click save to retry");
                    ui.error(&error);
                }
            },
        );
    }
    fn import(self: &Rc<Self>, kind: &'static str) {
        let dialog = gtk::FileDialog::builder().title("Capture a file").build();
        let ui = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else {
                return;
            };
            let Some(path) = file.path() else {
                ui.error("Choose a local file.");
                return;
            };
            let ui2 = ui.clone();
            ui.worker.call(
                move |rt, state| {
                    let bytes = std::fs::read(&path)?;
                    let entry = rt.block_on(state.as_ref().unwrap().capture_media(kind, bytes))?;
                    Ok((entry, path))
                },
                move |result| match result {
                    Ok((entry, path)) if kind == "voice" => {
                        ui2.refresh();
                        ui2.transcribe_voice(
                            entry["event_id"].as_str().unwrap_or("").to_owned(),
                            path,
                        );
                    }
                    Ok(_) => ui2.refresh(),
                    Err(error) => ui2.error(&error),
                },
            );
        });
    }
    fn transcribe_voice(self: &Rc<Self>, event_id: String, path: PathBuf) {
        self.transcribe_voice_source(event_id, path, None);
    }
    fn transcribe_voice_source(
        self: &Rc<Self>,
        event_id: String,
        path: PathBuf,
        keep_alive: Option<std::fs::File>,
    ) {
        let dir = data_dir();
        let ui = self.clone();
        self.toast
            .add_toast(adw::Toast::new("Voice captured. Transcribing…"));
        self.enrichment.call(
            move |_, _| {
                let result = background::transcribe_file(&dir, &path);
                drop(keep_alive);
                result
            },
            move |result| match result {
                Ok(text) => {
                    let ui2 = ui.clone();
                    ui.worker.call(
                        move |rt, state| {
                            rt.block_on(state.as_ref().context("Journal is locked")?.request(
                                "annotate_voice",
                                json!({"event_id":event_id,"text":text}),
                            ))
                        },
                        move |result| {
                            if let Err(error) = result {
                                ui2.error(&error);
                            }
                            ui2.refresh();
                        },
                    );
                }
                Err(error) => ui.error(&format!("Voice is saved; transcription failed: {error}")),
            },
        );
    }
    fn show_media(&self, kind: &str, bytes: Vec<u8>) {
        let window = adw::Window::builder()
            .title("Memorious — media")
            .transient_for(&self.window)
            .default_width(800)
            .default_height(600)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&adw::HeaderBar::new());
        let bytes = glib::Bytes::from_owned(bytes);
        if kind == "photo" {
            match gtk::gdk::Texture::from_bytes(&bytes) {
                Ok(texture) => {
                    let picture = gtk::Picture::for_paintable(&texture);
                    picture.set_can_shrink(true);
                    picture.set_vexpand(true);
                    body.append(&picture);
                }
                Err(e) => {
                    self.error(&e.to_string());
                    return;
                }
            }
        } else {
            // GTK's GStreamer backend seeks while inspecting MP4/M4A files.
            // Keep the seekable source alive through playback without writing
            // decrypted audio to a normal filesystem path.
            let media_fd = match anonymous_media_file(&bytes) {
                Ok(file) => file,
                Err(error) => {
                    self.error(&format!("Could not open recording: {error}"));
                    return;
                }
            };
            let file = gio::File::for_path(format!("/proc/self/fd/{}", media_fd.as_raw_fd()));
            let media = gtk::MediaFile::for_file(&file);
            let video = gtk::Video::for_media_stream(Some(&media));
            video.set_vexpand(true);
            body.append(&video);
            window.connect_close_request(move |_| {
                media.pause();
                let _keep_fd_open = media_fd.as_raw_fd();
                glib::Propagation::Proceed
            });
        }
        window.set_content(Some(&body));
        window.present();
    }
    fn open_canvas(self: &Rc<Self>) {
        if let Some(canvas) = self
            .canvas
            .borrow()
            .as_ref()
            .filter(|c| c.window.is_visible())
        {
            canvas.window.present();
            return;
        }
        *self.canvas.borrow_mut() = Some(canvas::Canvas::new(&self.window, self.worker.clone()));
    }
    fn settings(self: &Rc<Self>, section: &str) {
        if let Some(panel) = self
            .settings_panel
            .borrow()
            .as_ref()
            .and_then(settings::WeakPanel::upgrade)
        {
            if panel.dialog.is_visible() {
                panel.show_section(section);
                panel.dialog.present(Some(&self.window));
                return;
            }
        }
        let dialog = adw::Dialog::builder()
            .title("Settings")
            .content_width(820)
            .content_height(620)
            .build();
        let appearance = self.appearance_page();
        let background = self.background_settings_page(&dialog);
        let peers = self.peers_page();
        let panel = settings::Panel::new(dialog, [appearance, background, peers]);
        panel.show_section(section);
        *self.settings_panel.borrow_mut() = Some(panel.downgrade());
        panel.dialog.present(Some(&self.window));
    }
    fn appearance_page(self: &Rc<Self>) -> gtk::Widget {
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
            .title("Appearance")
            .description("Choose how Memorious looks on this device.")
            .build();
        let theme = adw::SwitchRow::builder()
            .title("Dark mode")
            .subtitle("Use a dark appearance")
            .active(adw::StyleManager::default().is_dark())
            .build();
        let ui = self.clone();
        theme.connect_active_notify(move |row| {
            let dark = row.is_active();
            adw::StyleManager::default().set_color_scheme(if dark {
                adw::ColorScheme::ForceDark
            } else {
                adw::ColorScheme::ForceLight
            });
            let path = appearance_path();
            let save = || -> anyhow::Result<()> {
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(
                    path.with_extension("tmp"),
                    if dark { "dark" } else { "light" },
                )?;
                std::fs::rename(path.with_extension("tmp"), &path)?;
                Ok(())
            };
            if let Err(error) = save() {
                ui.error(&format!("Could not save appearance: {error}"));
            }
        });
        group.add(&theme);
        page.add(&group);
        page.upcast()
    }
    fn background_settings_page(self: &Rc<Self>, dialog: &adw::Dialog) -> gtk::Widget {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        padded(&body);
        body.append(&label("Background mode keeps this journal syncing and push-to-talk available when the window closes."));
        let background_toggle = gtk::CheckButton::with_label("Run in the background");
        body.append(&background_toggle);
        let enabled = gtk::CheckButton::with_label("Enable push-to-talk");
        body.append(&enabled);
        let key = gtk::Entry::builder()
            .placeholder_text("Global key, e.g. KEY_RIGHTCTRL")
            .build();
        body.append(&key);
        let provider = gtk::DropDown::from_strings(&[
            "Local — Whisper Small (multilingual, 488 MB)",
            "Remote — API key",
        ]);
        body.append(&provider);
        let language = gtk::Entry::builder()
            .text("auto")
            .placeholder_text("Language: auto, af, en, es…")
            .build();
        body.append(&label(
            "Language (auto-detect or a code such as af for Afrikaans)",
        ));
        body.append(&language);
        let model_status = label("Checking local model…");
        body.append(&model_status);
        let model_download = gtk::Button::with_label("Download local model");
        body.append(&model_download);
        let local_note = label("Local transcription runs on this device without API calls. Remote mode uses your API key for transcription and cleanup. Your journal still syncs with paired peers.");
        body.append(&local_note);
        let api_key = gtk::PasswordEntry::builder()
            .placeholder_text("Remote API key (blank keeps saved key)")
            .show_peek_icon(true)
            .build();
        body.append(&api_key);
        body.append(&label("Failed captures stay queued for retry. The toolbar button works without global-key permissions."));
        let api_entry = api_key.clone();
        provider.connect_selected_notify(move |provider| {
            api_entry.set_sensitive(provider.selected() == 1)
        });
        api_key.set_sensitive(false);
        let ui = self.clone();
        model_download.connect_clicked(move |button| {
            button.set_sensitive(false);
            let ui2 = ui.clone();
            let button = button.clone();
            ui.controls.call(
                move |_, _| background::download_local_model(),
                move |result| {
                    if let Err(error) = result {
                        button.set_sensitive(true);
                        ui2.error(&error);
                    }
                },
            );
        });
        let weak_dialog = dialog.downgrade();
        let weak_ui = Rc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_millis(900), move || {
            let Some(dialog) = weak_dialog.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !dialog.is_visible() {
                return glib::ControlFlow::Break;
            }
            let Some(ui) = weak_ui.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let text = model_status.clone();
            let button = model_download.clone();
            ui.controls.call(
                move |_, _| background::local_model_status(),
                move |result| match result {
                    Ok(info) => {
                        let ready = info["ready"] == true;
                        let downloading = info["state"] == "downloading";
                        text.set_text(&if ready {
                            "Whisper Small is installed — ready offline".into()
                        } else if downloading {
                            format!(
                                "Downloading local model… {:.0}%",
                                info["fraction"].as_f64().unwrap_or(0.0) * 100.0
                            )
                        } else {
                            info["error"]
                                .as_str()
                                .unwrap_or("488 MB download; runs locally without an API key")
                                .into()
                        });
                        button.set_sensitive(!ready && !downloading);
                        button.set_label(if ready {
                            "Model installed"
                        } else if downloading {
                            "Downloading…"
                        } else {
                            "Download local model"
                        });
                    }
                    Err(error) => text.set_text(&error),
                },
            );
            glib::ControlFlow::Continue
        });
        let password = gtk::PasswordEntry::builder()
            .placeholder_text("Master password, if returning from background")
            .show_peek_icon(true)
            .build();
        body.append(&password);
        let status = label("Loading…");
        body.append(&status);
        let save = gtk::Button::with_label("Apply");
        save.add_css_class("suggested-action");
        body.append(&save);
        let page = settings::page(&body);
        let dir = data_dir();
        let toggle = background_toggle.clone();
        let enabled2 = enabled.clone();
        let key2 = key.clone();
        let status2 = status.clone();
        let provider2 = provider.clone();
        let language2 = language.clone();
        self.worker.call(
            move |_, state| {
                let config = background::load_ptt(&dir)?;
                Ok((
                    state.as_ref().context("Journal is locked")?.is_background(),
                    config.enabled,
                    config.key,
                    config.provider,
                    config.language,
                ))
            },
            move |result| match result {
                Ok((background, enabled, key, selected_provider, selected_language)) => {
                    language2.set_text(&selected_language);
                    provider2.set_selected(if selected_provider == "remote" { 1 } else { 0 });
                    toggle.set_active(background);
                    enabled2.set_active(enabled);
                    key2.set_text(&key);
                    status2.set_text(
                        "Changes apply immediately. The worker runs with the background journal.",
                    );
                }
                Err(error) => status2.set_text(&error),
            },
        );
        let ui = self.clone();
        save.connect_clicked(move |button| {
            let run_background = background_toggle.is_active();
            let enable_ptt = enabled.is_active();
            if enable_ptt && !run_background {
                status.set_text("Enable background mode to use push-to-talk.");
                return;
            }
            let key = key.text().to_string();
            let selected_provider = if provider.selected() == 1 {
                "remote"
            } else {
                "local"
            };
            let selected_language = language.text().to_string();
            let api = Zeroizing::new(api_key.text().to_string());
            let pw = Zeroizing::new(password.text().to_string());
            let dir = data_dir();
            let button = button.clone();
            button.set_sensitive(false);
            status.set_text("Applying…");
            let status = status.clone();
            let api_entry = api_key.clone();
            let password_entry = password.clone();
            ui.worker.call(
                move |rt, state| {
                    let session = state.as_mut().context("Journal is locked")?;
                    background::save_ptt_options(
                        &dir,
                        enable_ptt,
                        &key,
                        Some(&api),
                        selected_provider,
                        &selected_language,
                    )?;
                    rt.block_on(async {
                        if session.is_background() && !run_background {
                            session.disable_background(&pw).await?;
                        }
                        if run_background && !session.is_background() {
                            session.enable_background(&std::env::current_exe()?).await?;
                        }
                        Ok(())
                    })
                },
                move |result| {
                    button.set_sensitive(true);
                    api_entry.set_text("");
                    password_entry.set_text("");
                    match result {
                        Ok(()) => status.set_text(
                            "Applied. Close this window to keep the background journal running.",
                        ),
                        Err(error) => status.set_text(&error),
                    }
                },
            );
        });
        page
    }
    fn refresh_peer_status(
        self: &Rc<Self>,
        summary: gtk::Label,
        own_ticket: gtk::Entry,
        peers: gtk::Box,
    ) {
        let ui = self.clone();
        self.worker.call(
            move |rt, state| rt.block_on(state.as_ref().context("Journal is locked")?.status()),
            move |result| match result {
                Ok(value) => render_peer_status(&ui.worker, &summary, &own_ticket, &peers, &value),
                Err(error) => ui.error(&error),
            },
        );
    }
    fn history_controls(self: &Rc<Self>, body: &gtk::Box) {
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&label("History on this device"));
        let policy =
            gtk::DropDown::from_strings(&["Keep everything", "Phone retention", "Custom policy"]);
        policy.set_widget_name("history-retention-policy");
        policy.set_sensitive(false);
        body.append(&policy);
        let note = label("Loading retention policy…");
        body.append(&note);
        body.append(&label("Keep everything retains canvas revisions and media. Phone retention collects superseded canvas history after 30 days, transcribed voice after 7 days, and redacted media after 30 days. Canvas history collection waits for a peer that keeps revisions."));
        let save = gtk::Button::with_label("Save retention policy");
        save.set_sensitive(false);
        body.append(&save);
        let select = policy.clone();
        let note2 = note.clone();
        let save2 = save.clone();
        self.worker.call(
            move |rt, state| {
                rt.block_on(
                    state
                        .as_ref()
                        .context("Journal is locked")?
                        .request("get_retention", json!({})),
                )
            },
            move |result| match result {
                Ok(v) => {
                    select.set_selected(if v["policy"] == v["presets"]["keep_everything"] {
                        0
                    } else if v["policy"] == v["presets"]["phone"] {
                        1
                    } else {
                        2
                    });
                    select.set_sensitive(true);
                    save2.set_sensitive(select.selected() < 2);
                    note2.set_text("");
                }
                Err(e) => note2.set_text(&e),
            },
        );
        let save2 = save.clone();
        policy.connect_selected_notify(move |p| {
            save2.set_sensitive(p.is_sensitive() && p.selected() < 2)
        });
        let ui = self.clone();
        save.connect_clicked(move |b| {
            let preset = if policy.selected() == 0 {
                "keep_everything"
            } else {
                "phone"
            };
            let b = b.clone();
            b.set_sensitive(false);
            let note = note.clone();
            ui.worker.call(
                move |rt, state| {
                    rt.block_on(
                        state
                            .as_ref()
                            .context("Journal is locked")?
                            .request("set_retention", json!({"preset":preset})),
                    )
                },
                move |r| {
                    b.set_sensitive(true);
                    note.set_text(&match r {
                        Ok(_) => "Retention policy saved".into(),
                        Err(e) => e,
                    });
                },
            );
        });
        let export = gtk::Button::with_label("Export lossless archive…");
        body.append(&export);
        body.append(&label("The archive contains plain event JSON, the current schema and decrypted media. Keep it somewhere private."));
        let ui = self.clone();
        export.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Export journal archive")
                .initial_name("memorious.zip")
                .build();
            let ui = ui.clone();
            let parent = ui.window.clone();
            dialog.save(Some(&parent), gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else {
                    return;
                };
                let Some(path) = file.path() else {
                    ui.error("Choose a local destination");
                    return;
                };
                let ui2 = ui.clone();
                ui.toast.add_toast(adw::Toast::new("Exporting archive…"));
                ui.worker.call(
                    move |rt, state| {
                        rt.block_on(
                            state
                                .as_ref()
                                .context("Journal is locked")?
                                .request("export_archive", json!({"out":path})),
                        )
                    },
                    move |result| match result {
                        Ok(report) => ui2.error(&format!(
                            "Archive saved: {} events, {} media files, {} unavailable",
                            report["events"], report["media"], report["media_unavailable"]
                        )),
                        Err(error) => ui2.error(&error),
                    },
                );
            });
        });
        let isolate = gtk::Button::with_label("Isolate this device");
        isolate.add_css_class("destructive-action");
        body.append(&isolate);
        let end = gtk::Button::with_label("End isolation");
        body.append(&end);
        let ui = self.clone();
        isolate.connect_clicked(move |_| {
            let dialog=adw::AlertDialog::builder()
                .heading("Isolate this device?")
                .body("Sync stops and remembered peers are forgotten. Your entries stay here. Restart this journal for its new address to take effect; pair again after ending isolation.")
                .build();
            dialog.add_responses(&[("cancel","Cancel"),("isolate","Isolate")]);
            dialog.set_close_response("cancel");
            dialog.set_response_appearance("isolate",adw::ResponseAppearance::Destructive);
            let parent=ui.window.clone();
            let ui=ui.clone();
            dialog.connect_response(None,move |_,response| {
                if response!="isolate" {return;}
                let ui2=ui.clone();
                ui.worker.call(
                    move |rt,state| rt.block_on(state.as_ref().context("Journal is locked")?.request("isolate",json!({}))),
                    move |result| match result {
                        Ok(_)=>ui2.error("Isolated. Restart this journal before ending isolation. In background mode, stop it from the tray first."),
                        Err(error)=>ui2.error(&error),
                    },
                );
            });
            dialog.present(Some(&parent));
        });
        let ui = self.clone();
        end.connect_clicked(move |_| {
            let ui2 = ui.clone();
            ui.worker.call(
                move |rt, state| {
                    rt.block_on(
                        state
                            .as_ref()
                            .context("Journal is locked")?
                            .request("end_isolation", json!({})),
                    )
                },
                move |r| match r {
                    Ok(_) => ui2.error("Isolation ended. Pair with another peer to reconnect."),
                    Err(e) => ui2.error(&e),
                },
            );
        });
    }
    fn peers_page(self: &Rc<Self>) -> gtk::Widget {
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        padded(&body);
        let summary = label("Loading peer status…");
        body.append(&summary);
        let own_ticket = gtk::Entry::builder().editable(false).build();
        body.append(&label(
            "This device’s ticket — copy it to another device to pair:",
        ));
        body.append(&own_ticket);
        let copy = gtk::Button::with_label("Copy this device’s ticket");
        body.append(&copy);
        let own = own_ticket.clone();
        copy.connect_clicked(move |button| button.clipboard().set_text(&own.text()));
        let ticket = gtk::Entry::builder()
            .placeholder_text("Ticket from another device (optional)")
            .build();
        body.append(&label("Leave the ticket blank to sync every known peer. To reach a new address, paste a fresh ticket from that peer."));
        body.append(&ticket);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let sync = gtk::Button::with_label("Sync now");
        sync.add_css_class("suggested-action");
        actions.append(&sync);
        let check = gtk::Button::with_label("Check connections");
        actions.append(&check);
        body.append(&actions);
        let result_status = label("");
        body.append(&result_status);
        let peer_list = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.append(&peer_list);
        self.history_controls(&body);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&label("Network settings"));
        let relay = gtk::DropDown::from_strings(&[
            "Default public relays",
            "Custom relays",
            "Direct connections only",
        ]);
        relay.set_widget_name("peer-relay-mode");
        let relay_urls = gtk::TextView::builder()
            .height_request(70)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        body.append(&relay);
        body.append(&label("Custom relay URLs (one per line)"));
        body.append(&relay_urls);
        relay_urls.set_sensitive(false);
        let urls = relay_urls.clone();
        relay.connect_selected_notify(move |relay| urls.set_sensitive(relay.selected() == 1));
        let lookup =
            gtk::CheckButton::with_label("Publish and resolve addresses with public lookup");
        body.append(&lookup);
        let network_note = label("Network changes apply when this journal restarts. In background mode, stop the background journal from the tray, then reopen Memorious.");
        body.append(&network_note);
        let save = gtk::Button::with_label("Save network settings");
        save.set_sensitive(false);
        body.append(&save);
        let page = settings::page(&body);

        let ui = self.clone();
        let summary2 = summary.clone();
        let own2 = own_ticket.clone();
        let peers2 = peer_list.clone();
        let relay2 = relay.clone();
        let urls2 = relay_urls.clone();
        let lookup2 = lookup.clone();
        let save2 = save.clone();
        let note2 = network_note.clone();
        self.worker.call(
            move |rt, state| rt.block_on(state.as_ref().context("Journal is locked")?.status()),
            move |result| match result {
                Ok(value) => {
                    render_peer_status(&ui.worker, &summary2, &own2, &peers2, &value);
                    relay2.set_selected(match value["net"]["relay_mode"].as_str() {
                        Some("disabled") => 2, Some("custom") => 1, _ => 0,
                    });
                    let urls = value["net"]["relay_urls"].as_array().map(|urls| urls.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                    urls2.buffer().set_text(&urls);
                    lookup2.set_active(value["net"]["public_lookup"].as_bool().unwrap_or(true));
                    if relay2.selected() == 2 {
                        note2.set_text("Relays are disabled. Peers on other networks may be unreachable. To use relays, select them, save, then restart this journal. In background mode, stop the background journal from the tray and reopen Memorious.");
                    }
                    save2.set_sensitive(true);
                }
                Err(error) => ui.error(&error),
            },
        );
        let ui = self.clone();
        save.connect_clicked(move |button| {
            let mode = match relay.selected() { 1 => "custom", 2 => "disabled", _ => "default" };
            let buffer = relay_urls.buffer();
            let urls = if mode == "custom" { buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).lines().map(str::trim).filter(|url| !url.is_empty()).map(str::to_owned).collect::<Vec<_>>() } else { vec![] };
            let net = json!({"relay_mode":mode, "relay_urls":urls, "public_lookup":lookup.is_active()});
            let button = button.clone();
            button.set_sensitive(false);
            let note = network_note.clone();
            ui.worker.call(
                move |rt, state| rt.block_on(state.as_ref().context("Journal is locked")?.request("set_net_config", json!({"net":net}))),
                move |result| {
                    button.set_sensitive(true);
                    note.set_text(&match result {
                        Ok(_) => "Saved network settings. Restart this journal to apply them. In background mode, stop the background journal from the tray, then reopen Memorious.".into(),
                        Err(error) => error,
                    });
                },
            );
        });
        let ui = self.clone();
        let summary2 = summary.clone();
        let own2 = own_ticket.clone();
        let peers2 = peer_list.clone();
        let result2 = result_status.clone();
        sync.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_label("Syncing…");
            result2.set_text("Connecting to peers…");
            let ticket = ticket.text().to_string();
            let ui2 = ui.clone();
            let button = button.clone();
            let result_status = result2.clone();
            let summary = summary2.clone();
            let own = own2.clone();
            let peers = peers2.clone();
            ui.worker.call(
                move |rt, state| {
                    rt.block_on(state.as_ref().context("Journal is locked")?.sync(&ticket))
                },
                move |result| {
                    button.set_sensitive(true);
                    button.set_label("Sync now");
                    match result {
                        Ok(value) => {
                            let mut text = format!(
                                "Sent {} · received {} · media fetched {}",
                                value["sent"], value["received"], value["blobs"]
                            );
                            if let Some(peers) = value["peers"].as_array() {
                                text.push_str(&format!(
                                    "\nSynced {} of {} peers.",
                                    peers.iter().filter(|peer| peer["ok"] == true).count(),
                                    peers.len()
                                ));
                                for peer in peers.iter().filter(|peer| peer["ok"] != true) {
                                    let id = peer["endpoint_id"].as_str().unwrap_or("Peer");
                                    text.push_str(&format!(
                                        "\n{}: {}",
                                        &id[..10.min(id.len())],
                                        peer["error"].as_str().unwrap_or("unreachable")
                                    ));
                                }
                            }
                            result_status.set_text(&text);
                            ui2.refresh();
                        }
                        Err(error) => result_status.set_text(&error),
                    }
                    ui2.refresh_peer_status(summary, own, peers);
                },
            );
        });
        let ui = self.clone();
        check.connect_clicked(move |button| {
            button.set_sensitive(false);
            let button = button.clone();
            let result = result_status.clone();
            result.set_text("Checking connections…");
            let ui2 = ui.clone();
            let summary = summary.clone();
            let own = own_ticket.clone();
            let peers = peer_list.clone();
            ui.worker.call(
                move |rt, state| rt.block_on(state.as_ref().context("Journal is locked")?.request("ping_peers", json!({}))),
                move |outcome| {
                    button.set_sensitive(true);
                    result.set_text(&match outcome {
                        Ok(value) => {
                            let pings = value["pings"].as_array().cloned().unwrap_or_default();
                            if pings.is_empty() { "No known peers. Paste a ticket from another device in this journal.".into() }
                            else { pings.iter().map(|ping| {
                                let id = ping["endpoint_id"].as_str().unwrap_or("Peer");
                                let state = if ping["ok"] == true { format!("reachable ({} ms)", ping["rtt_ms"]) }
                                    else { ping["error"].as_str().unwrap_or("unreachable").into() };
                                format!("{}: {}", &id[..10.min(id.len())], state)
                            }).collect::<Vec<_>>().join("\n") }
                        }
                        Err(error) => error,
                    });
                    ui2.refresh_peer_status(summary, own, peers);
                    ui2.refresh();
                },
            );
        });
        page
    }
}

fn anonymous_media_file(bytes: &glib::Bytes) -> anyhow::Result<std::fs::File> {
    use std::{io::Write, os::fd::FromRawFd};
    let name = std::ffi::CString::new("memorious-playback")?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes.as_ref())?;
    Ok(file)
}

fn render_peer_status(
    worker: &Worker,
    summary: &gtk::Label,
    own_ticket: &gtk::Entry,
    peers: &gtk::Box,
    value: &Value,
) {
    summary.set_text(&format!(
        "{} entries · {} events · {} collected\n{}\nPeer holdings reflect the last successful contact.",
        value["entries"], value["events_total"], value["stubs"]["total"], if value["isolated"] == true {"Isolated: sync is disabled."} else if value["write_only"] == true {"Write-only peer: captures push; other entries are not pulled."} else {"Required peers determine sync health."}
    ));
    own_ticket.set_text(value["ticket"].as_str().unwrap_or(""));
    while let Some(child) = peers.first_child() {
        peers.remove(&child);
    }
    if let Some(known) = value["peers"].as_array() {
        if known.is_empty() {
            peers.append(&label("No known peers yet."));
        }
        for peer in known {
            let id = peer["device_id"].as_str().unwrap_or("");
            let name = value["names"][id]
                .as_str()
                .unwrap_or_else(|| peer["endpoint_id"].as_str().unwrap_or("Peer"));
            let last = peer["last_ok_ms"]
                .as_i64()
                .and_then(|t| glib::DateTime::from_unix_local(t / 1000).ok())
                .and_then(|t| t.format("%e %b %Y, %H:%M").ok())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "No successful sync".into());
            peers.append(&label(&format!(
                "{}\nLast sync: {}\nEvents held: {} · missing: {} · media missing: {}",
                name, last, peer["events_held"], peer["events_missing"], peer["media_missing"]
            )));
            let required = gtk::CheckButton::with_label("Required for sync health");
            required.set_active(peer["required"].as_bool().unwrap_or(true));
            let endpoint = peer["endpoint_id"].as_str().unwrap_or("").to_owned();
            let worker = worker.clone();
            let summary = summary.clone();
            required.connect_toggled(move |button| {
                button.set_sensitive(false);
                let enabled = button.is_active();
                let endpoint = endpoint.clone();
                let button = button.clone();
                let summary = summary.clone();
                worker.call(
                    move |rt, state| {
                        rt.block_on(state.as_ref().context("Journal is locked")?.request(
                            "set_peer_required",
                            json!({"endpoint_id":endpoint,"required":enabled}),
                        ))
                    },
                    move |result| {
                        button.set_sensitive(true);
                        button.set_inconsistent(result.is_err());
                        if let Err(error) = result {
                            summary.set_text(&error);
                        }
                    },
                );
            });
            peers.append(&required);
        }
    }
}

// Exercise the same widgets and signal handlers as a user, in an isolated test journal.
fn smoke_capture(ui: Rc<Ui>, verify: bool) {
    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut out = vec![widget.clone()];
        let mut child = widget.first_child();
        while let Some(widget) = child {
            out.extend(descendants(&widget));
            child = widget.next_sibling();
        }
        out
    }
    let setup = ui.stack.child_by_name("setup").unwrap();
    for widget in descendants(&setup) {
        if let Some(password) = widget.downcast_ref::<gtk::PasswordEntry>() {
            password.set_text("native-test-password");
        }
    }
    let create = descendants(&setup)
        .into_iter()
        .find_map(|widget| {
            widget.downcast::<gtk::Button>().ok().filter(|button| {
                button.label().as_deref()
                    == Some(if verify {
                        "Unlock"
                    } else {
                        "Create or join journal"
                    })
            })
        })
        .unwrap();
    create.emit_clicked();
    let started = std::time::Instant::now();
    let mut captured = verify;
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        assert!(started.elapsed().as_secs() < 20, "native capture timed out");
        if ui.stack.visible_child_name().as_deref() != Some("journal") {
            return glib::ControlFlow::Continue;
        }
        if matches!(
            std::env::var("MEMORIOUS_UI_SMOKE").as_deref(),
            Ok("canvas" | "canvas-verify")
        ) {
            ui.open_canvas();
            let verify = std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("canvas-verify");
            let canvas = ui.canvas.borrow().as_ref().unwrap().clone();
            let ui2 = ui.clone();
            canvas.smoke(verify, move || ui2.window.close());
            return glib::ControlFlow::Break;
        }
        if std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("recording") {
            smoke_recording(ui.clone());
            return glib::ControlFlow::Break;
        }
        if std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("audio-verify") {
            let entries = ui.last_entries.borrow();
            let Some(entries) = entries["entries"].as_array() else {
                return glib::ControlFlow::Continue;
            };
            assert_eq!(entries.len(), 1, "cancel must not append an audio entry");
            assert_eq!(entries[0]["kind"], "audio");
            assert_eq!(entries[0]["audio_kind"], "voice");
            ui.window.close();
            return glib::ControlFlow::Break;
        }
        if !captured {
            captured = true;
            ui.composer.buffer().set_text("Captured through GTK");
            let journal = ui.stack.child_by_name("journal").unwrap();
            descendants(&journal)
                .into_iter()
                .find_map(|widget| {
                    widget
                        .downcast::<gtk::Button>()
                        .ok()
                        .filter(|button| button.label().as_deref() == Some("Capture"))
                })
                .unwrap()
                .emit_clicked();
        } else if ui.last_entries.borrow()["entries"][0]["text"] == "Captured through GTK" {
            if std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("peers") {
                ui.settings("peers");
                ui.window.visible_dialog().unwrap().set_content_height(360);
                let ui = ui.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
                    let widgets = descendants(ui.window.upcast_ref());
                    let relay = widgets
                        .iter()
                        .find_map(|widget| {
                            widget
                                .downcast_ref::<gtk::DropDown>()
                                .filter(|d| d.widget_name() == "peer-relay-mode")
                        })
                        .expect("native network settings must expose relay mode");
                    assert_eq!(relay.selected(), 0, "fresh journals use default relays");
                    assert!(
                        widgets
                            .iter()
                            .filter_map(|widget| widget.downcast_ref::<gtk::Button>())
                            .any(|button| button.label().as_deref() == Some("Check connections")),
                        "peer diagnostics control is missing"
                    );
                    for title in [
                        "Save retention policy",
                        "Export lossless archive…",
                        "Isolate this device",
                        "End isolation",
                    ] {
                        assert!(
                            widgets
                                .iter()
                                .filter_map(|w| w.downcast_ref::<gtk::Button>())
                                .any(|b| b.label().as_deref() == Some(title)),
                            "missing native control: {title}"
                        );
                    }
                    let policy = widgets
                        .iter()
                        .find_map(|w| {
                            w.downcast_ref::<gtk::DropDown>()
                                .filter(|d| d.widget_name() == "history-retention-policy")
                        })
                        .unwrap();
                    policy.set_selected(1);
                    widgets
                        .iter()
                        .find_map(|w| {
                            w.downcast_ref::<gtk::Button>()
                                .filter(|b| b.label().as_deref() == Some("Save retention policy"))
                        })
                        .unwrap()
                        .emit_clicked();
                    let ui2 = ui.clone();
                    ui.worker.call(
                        move |rt, state| {
                            rt.block_on(state.as_ref().unwrap().request("get_retention", json!({})))
                        },
                        move |r| {
                            assert_eq!(r.unwrap()["policy"]["voice_audio"]["days"], 7);
                            glib::timeout_add_local_once(
                                std::time::Duration::from_millis(250),
                                move || smoke_close_settings(ui2, ".native-peers-tested"),
                            );
                        },
                    );
                });
            } else if std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("settings") {
                ui.settings("background");
                ui.window.visible_dialog().unwrap().set_content_height(360);
                let ui = ui.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
                    let widgets = descendants(ui.window.upcast_ref());
                    assert!(
                        widgets.iter().any(|w| w.is::<gtk::DropDown>()),
                        "provider selector is missing"
                    );
                    assert!(
                        widgets
                            .iter()
                            .filter_map(|w| w.downcast_ref::<gtk::Entry>())
                            .any(|entry| entry.text() == "auto"),
                        "language setting is missing"
                    );
                    let api = widgets
                        .iter()
                        .filter_map(|w| w.downcast_ref::<gtk::PasswordEntry>())
                        .find(|entry| {
                            entry.placeholder_text().as_deref()
                                == Some("Remote API key (blank keeps saved key)")
                        })
                        .expect("API key control");
                    assert!(
                        !api.is_sensitive(),
                        "local mode must not request an API key"
                    );
                    if std::env::var_os("MEMORIOUS_UI_EXPECT_MODEL").is_some() {
                        assert!(
                            widgets
                                .iter()
                                .filter_map(|w| w.downcast_ref::<gtk::Label>())
                                .any(|label| label.text().contains("ready offline")),
                            "installed model readiness did not reach the UI"
                        );
                    }
                    smoke_close_settings(ui, ".native-settings-tested");
                });
            } else {
                ui.window.close();
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

// A rendered feed fixture: no owner data, media fetching, or journal mutation.
fn smoke_stream(ui: Rc<Ui>) {
    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut out = vec![widget.clone()];
        let mut child = widget.first_child();
        while let Some(widget) = child {
            out.extend(descendants(&widget));
            child = widget.next_sibling();
        }
        out
    }
    fn resize_fixture(ui2: Rc<Ui>, wrapped: gtk::Label) {
        let wide_width = wrapped.width();
        ui2.window.set_default_size(520, 760);
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            assert!(
                wrapped.width() < wide_width,
                "fixture must exercise window resizing"
            );
            assert!(
                wrapped.layout().line_count() <= 7,
                "narrow text must stay capped"
            );
            ui2.window.set_default_size(1000, 760);
            glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                assert!(wrapped.width() >= wide_width);
                assert!(wrapped.layout().line_count() <= 7);
                ui2.window.application().unwrap().quit();
            });
        });
    }
    ui.stack.set_visible_child_name("journal");
    ui.loading.set(true); // This fixture has no journal worker to refresh.
    ui.entry(
        &json!({"text": "A short thought", "event_id": "fixture"}),
        false,
        "15:58".into(),
    );
    let row = ui.list.first_child().unwrap();
    assert!(
        !row.has_css_class("card"),
        "entries must be bare text, without card borders or shadows"
    );
    ui.entry(
        &json!({"text": "A long thought\n".repeat(15), "event_id": "long"}),
        false,
        "15:57".into(),
    );
    ui.entry(&json!({"kind": "audio", "media": {"hash": "fixture"}, "annotation": "A quiet transcript", "event_id": "audio"}), false, "15:56".into());
    ui.entry(&json!({"text": "A wrapped paragraph with Unicode café, 日本語, and https://example.org/a-long-link ".repeat(8), "event_id": "wrapped"}), false, "15:55".into());
    for _ in 0..12 {
        ui.entry(
            &json!({"text": "Another thought", "event_id": "filler"}),
            false,
            "15:55".into(),
        );
    }
    glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
        smoke_snapshot(&ui);
        let widgets = descendants(ui.list.upcast_ref());
        let labels: Vec<gtk::Label> = widgets
            .iter()
            .filter_map(|w| w.clone().downcast().ok())
            .collect();
        let text = labels
            .iter()
            .find(|l| l.text() == "A short thought")
            .unwrap();
        assert!(text.is_selectable());
        assert!(
            !text.next_sibling().unwrap().is_visible(),
            "short entries need no more control"
        );
        assert!(text
            .pango_context()
            .font_description()
            .unwrap()
            .family()
            .unwrap()
            .to_lowercase()
            .contains("mono"));
        let long = labels
            .iter()
            .find(|l| l.text().starts_with("A long thought"))
            .unwrap();
        assert_eq!(long.lines(), 7);
        assert!(
            long.layout().line_count() <= 7,
            "the rendered text must cap paragraphs as well as wrapped lines"
        );
        let more = widgets
            .iter()
            .filter_map(|w| w.clone().downcast::<gtk::Button>().ok())
            .find(|b| b.is_visible() && b.label().as_deref() == Some("more"))
            .expect("long entries need more/less");
        more.emit_clicked();
        assert_eq!(long.lines(), -1);
        assert_eq!(long.text(), "A long thought\n".repeat(15));
        assert_eq!(more.label().as_deref(), Some("less"));
        more.emit_clicked();
        assert_eq!(long.lines(), 7);
        assert!(long.layout().line_count() <= 7);
        let transcript = labels
            .iter()
            .find(|l| l.text() == "A quiet transcript")
            .unwrap();
        assert!(transcript.has_css_class("stream-annotation"));
        let wrapped = labels
            .iter()
            .find(|l| l.text().starts_with("A wrapped paragraph"))
            .unwrap()
            .clone();
        assert!(wrapped.layout().line_count() <= 7);
        let timestamp = labels.iter().find(|l| l.text() == "15:58").unwrap();
        assert!(
            timestamp.opacity() < 0.01,
            "timestamps are hidden while idle"
        );
        let rule = widgets
            .iter()
            .find(|w| w.has_css_class("stream-rule"))
            .expect("entry rule");
        assert!(rule.opacity() < 0.01, "rules are hidden while idle");
        let scroll = descendants(ui.window.upcast_ref())
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::ScrolledWindow>().ok())
            .find(|w| ui.list.is_ancestor(w))
            .unwrap();
        let adjustment = scroll.vadjustment();
        assert!(adjustment.upper() > adjustment.page_size());
        adjustment.set_value(80.0);
        let ui2 = ui.clone();
        let timestamp = timestamp.clone();
        let rule = rule.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            assert!(timestamp.opacity() > 0.99, "scrolling reveals timestamps");
            assert!(rule.opacity() > 0.99, "scrolling reveals rules");
            // More motion restarts the idle delay (including scrollbar/keyboard scrolling).
            adjustment.set_value(120.0);
            glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
                assert!(
                    ui2.list.has_css_class("scrolling"),
                    "scroll idle timer must reset"
                );
                glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                    assert!(!ui2.list.has_css_class("scrolling"));
                    let started = std::time::Instant::now();
                    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                        if timestamp.opacity() >= 0.01 || rule.opacity() >= 0.01 {
                            assert!(
                                started.elapsed() < std::time::Duration::from_secs(3),
                                "scroll fade must finish"
                            );
                            return glib::ControlFlow::Continue;
                        }
                        resize_fixture(ui2.clone(), wrapped.clone());
                        glib::ControlFlow::Break
                    });
                });
            });
        });
    });
}

fn smoke_composer(ui: Rc<Ui>) {
    fn descendants(w: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut out = vec![w.clone()];
        let mut child = w.first_child();
        while let Some(w) = child {
            out.extend(descendants(&w));
            child = w.next_sibling();
        }
        out
    }
    ui.stack.set_visible_child_name("journal");
    let widgets = descendants(ui.window.upcast_ref());
    let record = widgets
        .iter()
        .find_map(|w| {
            w.downcast_ref::<gtk::Button>()
                .filter(|b| b.tooltip_text().as_deref() == Some("Record audio"))
        })
        .expect("composer needs a microphone button independent of PTT");
    assert!(record.is_sensitive());
    let hint = widgets
        .iter()
        .find_map(|w| {
            w.downcast_ref::<gtk::Label>()
                .filter(|l| l.text() == "What’s happening?")
        })
        .expect("empty composer needs a visible prompt")
        .clone();
    assert!(hint.is_visible());
    ui.composer.buffer().set_text("A thought");
    assert!(!hint.is_visible());
    ui.composer.buffer().set_text("");
    assert!(hint.is_visible());
    let settings = widgets
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Button>())
        .filter(|b| b.tooltip_text().as_deref() == Some("Settings"))
        .collect::<Vec<_>>();
    assert_eq!(settings.len(), 1, "journal needs one settings entry point");
    assert!(
        !widgets.iter().any(|w| matches!(
            w.tooltip_text().as_deref(),
            Some("Dark mode" | "Peers and pairing" | "Background and push-to-talk settings")
        )),
        "settings must be consolidated behind the settings icon"
    );
    settings[0].emit_clicked();
    let dialog = ui.window.visible_dialog().expect("settings panel");
    assert_eq!(dialog.title(), "Settings");
    let widgets = descendants(dialog.upcast_ref());
    let split = widgets
        .iter()
        .find_map(|w| w.downcast_ref::<adw::NavigationSplitView>())
        .expect("settings needs an adaptive Adwaita sidebar")
        .clone();
    let sidebar = descendants(split.sidebar().unwrap().upcast_ref())
        .into_iter()
        .find_map(|w| w.downcast::<gtk::ListBox>().ok())
        .unwrap();
    let pages = widgets
        .iter()
        .find_map(|w| {
            w.downcast_ref::<gtk::Stack>()
                .filter(|s| s.widget_name() == "settings-sections")
        })
        .expect("settings sections")
        .clone();
    for (i, name) in ["appearance", "background", "peers"]
        .into_iter()
        .enumerate()
    {
        let row = sidebar.row_at_index(i as i32).expect("settings section");
        sidebar.select_row(Some(&row));
        sidebar.emit_by_name::<()>("row-activated", &[&row]);
        assert_eq!(pages.visible_child_name().as_deref(), Some(name));
        assert_eq!(
            ui.window.visible_dialog().as_ref(),
            Some(&dialog),
            "sections must share a single panel"
        );
    }
    assert!(sidebar.row_at_index(3).is_none());
    let row = sidebar.row_at_index(0).unwrap();
    sidebar.select_row(Some(&row));
    sidebar.emit_by_name::<()>("row-activated", &[&row]);
    let theme = widgets
        .iter()
        .find_map(|w| w.downcast_ref::<adw::SwitchRow>())
        .expect("appearance dark-mode switch");
    let manager = adw::StyleManager::default();
    if std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("appearance") {
        assert_eq!(
            manager.color_scheme(),
            adw::ColorScheme::ForceLight,
            "appearance must survive restart"
        );
        assert!(!theme.is_active());
    } else {
        theme.set_active(true);
        assert_eq!(manager.color_scheme(), adw::ColorScheme::ForceDark);
        theme.set_active(false);
        assert_eq!(manager.color_scheme(), adw::ColorScheme::ForceLight);
    }
    ui.window.set_default_size(480, 760);
    dialog.set_content_width(420);
    glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
        assert!(
            split.is_collapsed(),
            "settings sidebar must adapt at narrow widths"
        );
        split
            .content()
            .unwrap()
            .child()
            .unwrap()
            .activate_action("navigation.pop", None)
            .unwrap();
        assert!(
            !split.shows_content(),
            "back must return to the settings sidebar"
        );
        let row = sidebar.row_at_index(1).unwrap();
        sidebar.select_row(Some(&row));
        sidebar.emit_by_name::<()>("row-activated", &[&row]);
        assert!(
            split.shows_content(),
            "selecting a section must open it at narrow widths"
        );
        assert_eq!(pages.visible_child_name().as_deref(), Some("background"));
        assert!(descendants(split.content().unwrap().upcast_ref())
            .iter()
            .any(|w| w.is::<adw::HeaderBar>()));
        ui.window.application().unwrap().quit();
    });
}

fn smoke_recording(ui: Rc<Ui>) {
    let started = std::time::Instant::now();
    let mut stage = 0;
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        assert!(
            started.elapsed().as_secs() < 20,
            "recording controls timed out at stage {stage}"
        );
        match stage {
            0 => {
                ui.record.emit_clicked();
                stage = 1;
            }
            1 if ui.recording.borrow().is_some() => {
                assert!(ui.cancel_recording.is_visible());
                assert_eq!(
                    ui.record.tooltip_text().as_deref(),
                    Some("Stop and save audio")
                );
                assert!(ui.recording_status.text().contains("Recording"));
                ui.window.close();
                assert!(
                    ui.window.is_sensitive(),
                    "closing must not silently discard audio"
                );
                ui.cancel_recording.emit_clicked();
                assert!(ui.recording.borrow().is_none());
                ui.record.emit_clicked();
                stage = 2;
            }
            2 if ui.recording.borrow().is_some() => {
                ui.record.emit_clicked();
                stage = 3;
            }
            3 if ui.last_entries.borrow()["entries"][0]["kind"] == "audio" => {
                assert!(!ui.recording_busy.get());
                assert!(ui.pending_audio.borrow().is_none());
                assert!(!ui.cancel_recording.is_visible());
                ui.window.close();
                return glib::ControlFlow::Break;
            }
            _ => {}
        }
        glib::ControlFlow::Continue
    });
}

// Check the rendered header position and use its actual close button after scrolling.
fn smoke_close_settings(ui: Rc<Ui>, marker: &'static str) {
    fn descendants(w: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut out = vec![w.clone()];
        let mut child = w.first_child();
        while let Some(w) = child {
            out.extend(descendants(&w));
            child = w.next_sibling();
        }
        out
    }
    let dialog = ui.window.visible_dialog().unwrap();
    let widgets = descendants(dialog.upcast_ref());
    let pane = widgets
        .iter()
        .find_map(|w| w.downcast_ref::<adw::NavigationSplitView>())
        .and_then(|split| split.content())
        .and_then(|content| content.child())
        .unwrap_or_else(|| dialog.clone().upcast());
    let pane_widgets = descendants(&pane);
    let header = pane_widgets
        .iter()
        .find_map(|w| w.downcast_ref::<adw::HeaderBar>())
        .unwrap()
        .clone();
    let page = pane_widgets
        .iter()
        .find_map(|w| {
            w.downcast_ref::<gtk::Stack>()
                .filter(|s| s.widget_name() == "settings-sections")
        })
        .and_then(|pages| pages.visible_child())
        .unwrap_or(pane);
    let page_widgets = descendants(&page);
    let scroll = page_widgets
        .iter()
        .find_map(|w| w.downcast_ref::<gtk::ScrolledWindow>())
        .unwrap();
    assert!(
        !widgets
            .iter()
            .filter_map(|w| w.downcast_ref::<adw::HeaderBar>())
            .any(|h| widgets
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk::ScrolledWindow>())
                .any(|s| h.is_ancestor(s))),
        "settings header must be outside the scrolling content"
    );
    smoke_snapshot(&ui);
    let initial_y = header.compute_bounds(&dialog).unwrap().y();
    let adjustment = scroll.vadjustment();
    assert!(
        adjustment.upper() - adjustment.page_size() > 50.0,
        "settings fixture must scroll"
    );
    adjustment.set_value(adjustment.upper() - adjustment.page_size());
    glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
        assert!(
            (header.compute_bounds(&dialog).unwrap().y() - initial_y).abs() < 1.0,
            "settings close controls moved when the panel scrolled"
        );
        let close = descendants(dialog.upcast_ref())
            .into_iter()
            .find_map(|w| {
                w.downcast::<gtk::Button>()
                    .ok()
                    .filter(|b| b.has_css_class("close") && b.is_mapped())
            })
            .expect("settings close button");
        let bounds = close.compute_bounds(&dialog).unwrap();
        assert!(
            bounds.y() >= 0.0 && bounds.y() + bounds.height() <= dialog.height() as f32,
            "close button must stay on screen"
        );
        close.emit_clicked();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            assert!(
                ui.window.visible_dialog().is_none(),
                "close must work at the bottom of settings"
            );
            std::fs::write(data_dir().join(marker), b"passed").unwrap();
            ui.window.close();
        });
    });
}

fn smoke_snapshot(ui: &Ui) {
    if let Some(path) = std::env::var_os("MEMORIOUS_UI_SCREENSHOT") {
        let snapshot = gtk::Snapshot::new();
        gtk::WidgetPaintable::new(Some(&ui.window)).snapshot(
            &snapshot,
            ui.window.width() as f64,
            ui.window.height() as f64,
        );
        let node = snapshot.to_node().unwrap();
        ui.window
            .renderer()
            .unwrap()
            .render_texture(&node, None)
            .save_to_png(path)
            .unwrap();
    }
}
