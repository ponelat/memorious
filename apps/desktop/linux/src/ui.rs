use adw::prelude::*;
use anyhow::Context;
use gtk::{gio, glib};
use memorious_linux::{
    background::{self, Session},
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
        gtk::Window::set_default_icon_name("memorious");
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
            list: gtk::Box::new(gtk::Orientation::Vertical, 12),
            search: gtk::SearchEntry::new(),
            trash: gtk::ToggleButton::with_label("Trash"),
            composer: gtk::TextView::new(),
            limit: Cell::new(50),
            generation: Cell::new(0),
            loading: Cell::new(false),
            last_entries: RefCell::new(Value::Null),
            photo_cache: RefCell::new(HashMap::new()),
        });
        ui.build();
        let closing = Rc::new(Cell::new(false));
        let ui_close = ui.clone();
        ui.window.connect_close_request(move |window| {
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
        if std::env::var_os("MEMORIOUS_UI_SMOKE").is_some() {
            assert_eq!(ui.stack.visible_child_name().as_deref(), Some("setup"));
            if matches!(
                std::env::var("MEMORIOUS_UI_SMOKE").as_deref(),
                Ok("capture" | "verify" | "settings" | "peers")
            ) {
                smoke_capture(
                    ui.clone(),
                    std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("verify"),
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
    limit: Cell<usize>,
    generation: Cell<u64>,
    loading: Cell<bool>,
    last_entries: RefCell<Value>,
    photo_cache: RefCell<HashMap<String, gtk::gdk::Texture>>,
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
        let peers = gtk::Button::from_icon_name("network-workgroup-symbolic");
        peers.set_tooltip_text(Some("Peers and pairing"));
        header.pack_end(&peers);
        header.pack_end(&self.ptt);
        self.ptt
            .set_tooltip_text(Some("Start or stop push-to-talk recording"));
        let settings = gtk::Button::from_icon_name("preferences-system-symbolic");
        settings.set_tooltip_text(Some("Background and push-to-talk settings"));
        header.pack_end(&settings);
        let ui = self.clone();
        settings.connect_clicked(move |_| ui.background_settings());
        let ui = self.clone();
        self.ptt.connect_clicked(move |_| {
            let dir = data_dir();
            let ui2 = ui.clone();
            ui.controls.call(
                move |_, _| background::ptt_command(&dir, "toggle"),
                move |result| {
                    if let Err(error) = result {
                        ui2.error(&error);
                        ui2.background_settings();
                    }
                },
            );
        });
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
        padded(&self.list);
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
        editor.add_css_class("card");
        capture.append(&editor);
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
        let ui = self.clone();
        peers.connect_clicked(move |_| ui.peers());
    }
    fn refresh(self: &Rc<Self>) {
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
                                heading.add_css_class("heading");
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
        let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
        card.add_css_class("card");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        padded(&content);
        card.append(&content);
        let meta = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let timestamp = label(&time);
        timestamp.add_css_class("dim-label");
        timestamp.set_hexpand(true);
        meta.append(&timestamp);
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
        content.append(&meta);
        if let Some(text) = entry["text"].as_str() {
            content.append(&label(text));
        }
        if let Some(text) = entry["annotation"].as_str() {
            content.append(&label(text));
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
                let play = gtk::Button::with_label("Play");
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
        self.list.append(&card);
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
        let dir = data_dir();
        let ui = self.clone();
        self.toast
            .add_toast(adw::Toast::new("Voice captured. Transcribing…"));
        self.enrichment.call(
            move |_, _| background::transcribe_file(&dir, &path),
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
    fn background_settings(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder()
            .title("Background and push-to-talk")
            .content_width(500)
            .content_height(590)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        padded(&body);
        body.append(&adw::HeaderBar::new());
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
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&body)
            .build();
        dialog.set_child(Some(&scroll));
        dialog.present(Some(&self.window));
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
                Ok(value) => render_peer_status(&summary, &own_ticket, &peers, &value),
                Err(error) => ui.error(&error),
            },
        );
    }
    fn peers(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder()
            .title("Peers and pairing")
            .content_width(600)
            .content_height(720)
            .build();
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
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        body.append(&label("Network settings"));
        let relay = gtk::DropDown::from_strings(&[
            "Default public relays",
            "Custom relays",
            "Direct connections only",
        ]);
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
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&body)
            .build();
        let layout = adw::ToolbarView::new();
        layout.add_top_bar(&adw::HeaderBar::new());
        layout.set_content(Some(&scroll));
        dialog.set_child(Some(&layout));
        dialog.present(Some(&self.window));

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
                    render_peer_status(&summary2, &own2, &peers2, &value);
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
    summary: &gtk::Label,
    own_ticket: &gtk::Entry,
    peers: &gtk::Box,
    value: &Value,
) {
    summary.set_text(&format!(
        "{} entries · {} events\nPeer holdings reflect the last successful contact.",
        value["entries"], value["events_total"]
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
                "{}\nLast sync: {}\nEvents held: {} · missing: {}",
                name, last, peer["events_held"], peer["events_missing"]
            )));
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
                ui.peers();
                ui.window.visible_dialog().unwrap().set_content_height(360);
                let ui = ui.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
                    let widgets = descendants(ui.window.upcast_ref());
                    let relay = widgets
                        .iter()
                        .find_map(|widget| widget.downcast_ref::<gtk::DropDown>())
                        .expect("native network settings must expose relay mode");
                    assert_eq!(relay.selected(), 0, "fresh journals use default relays");
                    assert!(
                        widgets
                            .iter()
                            .filter_map(|widget| widget.downcast_ref::<gtk::Button>())
                            .any(|button| button.label().as_deref() == Some("Check connections")),
                        "peer diagnostics control is missing"
                    );
                    let dialog = ui.window.visible_dialog().unwrap();
                    let children = descendants(dialog.upcast_ref());
                    let header = children
                        .iter()
                        .find_map(|w| w.downcast_ref::<adw::HeaderBar>())
                        .unwrap()
                        .clone();
                    let scroll = children
                        .iter()
                        .filter_map(|w| w.downcast_ref::<gtk::ScrolledWindow>())
                        .filter(|scroll| relay.is_ancestor(*scroll))
                        .last()
                        .unwrap();
                    let initial_y = header.compute_bounds(&dialog).unwrap().y();
                    let adjustment = scroll.vadjustment();
                    assert!(
                        adjustment.upper() - adjustment.page_size() > 50.0,
                        "fixture must scroll"
                    );
                    adjustment.set_value(adjustment.upper() - adjustment.page_size());
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(250),
                        move || {
                            assert!(
                                (header.compute_bounds(&dialog).unwrap().y() - initial_y).abs()
                                    < 1.0,
                                "peer close controls moved when the panel scrolled"
                            );
                            let close = descendants(header.upcast_ref())
                                .into_iter()
                                .find_map(|widget| {
                                    widget
                                        .downcast::<gtk::Button>()
                                        .ok()
                                        .filter(|button| button.has_css_class("close"))
                                })
                                .expect("peer close button");
                            close.emit_clicked();
                            glib::timeout_add_local_once(
                                std::time::Duration::from_millis(300),
                                move || {
                                    assert!(
                                        ui.window.visible_dialog().is_none(),
                                        "close must work at the bottom of the panel"
                                    );
                                    std::fs::write(
                                        data_dir().join(".native-peers-tested"),
                                        b"passed",
                                    )
                                    .unwrap();
                                    ui.window.close();
                                },
                            );
                        },
                    );
                });
            } else if std::env::var("MEMORIOUS_UI_SMOKE").as_deref() == Ok("settings") {
                ui.background_settings();
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
                    std::fs::write(data_dir().join(".native-settings-tested"), b"passed").unwrap();
                    if let Some(dialog) = ui.window.visible_dialog() {
                        dialog.force_close();
                    }
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(300),
                        move || {
                            ui.window.close();
                        },
                    );
                });
            } else {
                ui.window.close();
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}
