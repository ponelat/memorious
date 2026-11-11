//! StatusNotifierItem / DBusMenu integration, owned by the background process.
use crate::background;
use glib::variant::ToVariant;
use gtk::{gio, glib};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
};

const ITEM: &str = "org.kde.StatusNotifierItem";
const XML: &str = r#"<node>
<interface name="org.kde.StatusNotifierItem">
<method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
<property name="Category" type="s" access="read"/><property name="Id" type="s" access="read"/>
<property name="Title" type="s" access="read"/><property name="Status" type="s" access="read"/>
<property name="WindowId" type="u" access="read"/><property name="IconName" type="s" access="read"/>
<property name="IconThemePath" type="s" access="read"/><property name="Menu" type="o" access="read"/>
<property name="ItemIsMenu" type="b" access="read"/>
<property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
<signal name="NewIcon"/><signal name="NewToolTip"/>
</interface>
<interface name="com.canonical.dbusmenu">
<method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method>
<method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/></method>
<method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
<property name="Version" type="u" access="read"/><property name="TextDirection" type="s" access="read"/><property name="Status" type="s" access="read"/>
</interface></node>"#;

pub fn menu_layout() -> glib::Variant {
    let children: Vec<glib::Variant> = [
        (1, "Open Memorious"),
        (2, "Start / stop recording"),
        (3, "Stop background journal"),
    ]
    .into_iter()
    .map(|(id, title)| {
        let props: HashMap<String, glib::Variant> = [
            ("label".into(), title.to_variant()),
            ("enabled".into(), true.to_variant()),
            ("visible".into(), true.to_variant()),
        ]
        .into();
        (id, props, Vec::<glib::Variant>::new()).to_variant()
    })
    .collect();
    (
        1u32,
        (0i32, HashMap::<String, glib::Variant>::new(), children),
    )
        .to_variant()
}
fn action(id: i32, dir: PathBuf) {
    std::thread::spawn(move || match id {
        1 => {
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe)
                    .env("MEMORIOUS_DATA_DIR", &dir)
                    .env_remove("MEMORIOUS_UI_SMOKE")
                    .spawn();
            }
        }
        2 => {
            let _ = background::ptt_command(&dir, "toggle");
        }
        3 => {
            if let Ok(rt) = tokio::runtime::Runtime::new() {
                let _ = rt.block_on(memorious_core::local::call(
                    &background::socket(&dir),
                    "shutdown",
                    serde_json::json!({}),
                ));
            }
        }
        _ => {}
    });
}
pub struct Tray(mpsc::Sender<()>);
impl Drop for Tray {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
pub fn spawn(dir: &Path) -> Tray {
    let (tx, rx) = mpsc::channel();
    let dir = dir.to_owned();
    std::thread::spawn(move || {
        let context = glib::MainContext::new();
        let _ = context.with_thread_default(|| -> Result<(), glib::Error> {
            let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
            let info = gio::DBusNodeInfo::for_xml(XML)?;
            let item_dir = dir.clone();
            let props_dir = dir.clone();
            let _item = connection
                .register_object("/StatusNotifierItem", &info.lookup_interface(ITEM).unwrap())
                .method_call(move |_, _, _, _, method, _, invocation| {
                    action(
                        if method == "SecondaryActivate" { 2 } else { 1 },
                        item_dir.clone(),
                    );
                    invocation.return_value(Some(&().to_variant()));
                })
                .property(move |_, _, _, _, name| {
                    let status = background::ptt_status(&props_dir);
                    let state = status["state"].as_str().unwrap_or("stopped");
                    match name {
                        "Category" => "ApplicationStatus".to_variant(),
                        "Id" => "memorious".to_variant(),
                        "Title" => "Memorious".to_variant(),
                        "Status" => "Active".to_variant(),
                        "WindowId" => 0u32.to_variant(),
                        "IconThemePath" => "".to_variant(),
                        "IconName" => (if state == "recording" {
                            "media-record-symbolic"
                        } else {
                            "audio-input-microphone-symbolic"
                        })
                        .to_variant(),
                        "Menu" => glib::variant::ObjectPath::try_from("/Menu")
                            .unwrap()
                            .to_variant(),
                        "ItemIsMenu" => false.to_variant(),
                        "ToolTip" => (
                            "audio-input-microphone-symbolic",
                            Vec::<(i32, i32, Vec<u8>)>::new(),
                            "Memorious",
                            format!("Push-to-talk: {state}"),
                        )
                            .to_variant(),
                        _ => "".to_variant(),
                    }
                })
                .build()?;
            let menu_dir = dir.clone();
            let _menu = connection
                .register_object(
                    "/Menu",
                    &info.lookup_interface("com.canonical.dbusmenu").unwrap(),
                )
                .method_call(move |_, _, _, _, method, params, invocation| match method {
                    "GetLayout" => invocation.return_value(Some(&menu_layout())),
                    "AboutToShow" => invocation.return_value(Some(&(false,).to_variant())),
                    "Event" => {
                        if let Some((id, event, _, _)) =
                            params.get::<(i32, String, glib::Variant, u32)>()
                        {
                            if event == "clicked" {
                                action(id, menu_dir.clone());
                            }
                        }
                        invocation.return_value(Some(&().to_variant()));
                    }
                    _ => invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "Unknown menu method",
                    ),
                })
                .property(|_, _, _, _, name| match name {
                    "Version" => 3u32.to_variant(),
                    "TextDirection" => "ltr".to_variant(),
                    _ => "normal".to_variant(),
                })
                .build()?;
            let watch = gio::bus_watch_name_on_connection(
                &connection,
                "org.kde.StatusNotifierWatcher",
                gio::BusNameWatcherFlags::NONE,
                |connection, _, _| {
                    connection.call(
                        Some("org.kde.StatusNotifierWatcher"),
                        "/StatusNotifierWatcher",
                        "org.kde.StatusNotifierWatcher",
                        "RegisterStatusNotifierItem",
                        Some(&(connection.unique_name().unwrap().as_str(),).to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        3000,
                        gio::Cancellable::NONE,
                        |_| {},
                    );
                },
                |_, _| {},
            );
            let main_loop = glib::MainLoop::new(Some(&context), false);
            let main_loop2 = main_loop.clone();
            let source = glib::timeout_source_new(
                std::time::Duration::from_millis(700),
                Some("memorious-tray"),
                glib::Priority::DEFAULT,
                move || {
                    if rx.try_recv().is_ok() {
                        main_loop2.quit();
                        return glib::ControlFlow::Break;
                    }
                    let _ =
                        connection.emit_signal(None, "/StatusNotifierItem", ITEM, "NewIcon", None);
                    let _ = connection.emit_signal(
                        None,
                        "/StatusNotifierItem",
                        ITEM,
                        "NewToolTip",
                        None,
                    );
                    glib::ControlFlow::Continue
                },
            );
            source.attach(Some(&context));
            main_loop.run();
            gio::bus_unwatch_name(watch);
            Ok(())
        });
    });
    Tray(tx)
}
