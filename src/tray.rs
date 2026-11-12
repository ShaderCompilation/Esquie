//! System tray icon (StatusNotifierItem). Works on KDE, and on GNOME with the
//! AppIndicator extension.
//!
//! The tray host (StatusNotifierWatcher) may start after Esquie, restart, or
//! go away for good. The service stays alive throughout, re-registers whenever
//! a watcher appears, and reports availability through [`Action::Available`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem};

pub enum Action {
    ToggleWindow,
    ShowWindow,
    SetEnabled(bool),
    SetAutostart(bool),
    Quit,
    /// The icon became visible (`true`) or lost its tray host (`false`).
    Available(bool),
}

pub struct Tray {
    pub enabled: bool,
    pub autostart: bool,
    icon: Vec<Icon>,
    available: Arc<AtomicBool>,
    on_action: Box<dyn Fn(Action) + Send>,
}

pub type Handle = ksni::blocking::Handle<Tray>;

/// Starts the tray service and returns its handle with whether the icon is
/// visible right now. `None` means no tray is possible (e.g. no D-Bus session).
/// Later availability changes arrive as [`Action::Available`].
pub fn spawn(
    enabled: bool,
    autostart: bool,
    on_action: impl Fn(Action) + Send + 'static,
) -> Option<(Handle, bool)> {
    // Optimistic: registration failures during spawn call watcher_offline.
    let available = Arc::new(AtomicBool::new(true));
    let tray = Tray {
        enabled,
        autostart,
        icon: render_icon(),
        available: available.clone(),
        on_action: Box::new(on_action),
    };
    // Keep running without a watcher so a tray that starts later still gets
    // the icon (ksni re-registers when the watcher's bus name appears).
    match tray.assume_sni_available(true).spawn() {
        Ok(handle) => Some((handle, available.load(Ordering::SeqCst))),
        Err(e) => {
            tracing::warn!("System tray not available: {e}");
            None
        }
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "esquie".into()
    }

    fn title(&self) -> String {
        "Esquie".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Esquie".into(),
            description: if self.enabled {
                "Noise removal is on"
            } else {
                "Noise removal is off"
            }
            .into(),
            ..Default::default()
        }
    }

    fn watcher_online(&self) {
        // Registration follows; if it fails, watcher_offline reports that.
        self.available.store(true, Ordering::SeqCst);
        (self.on_action)(Action::Available(true));
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        tracing::warn!("System tray not available: {reason:?}");
        self.available.store(false, Ordering::SeqCst);
        (self.on_action)(Action::Available(false));
        true // keep waiting for a tray to (re)appear
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on_action)(Action::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Show Esquie".into(),
                activate: Box::new(|t: &mut Self| (t.on_action)(Action::ShowWindow)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Noise removal".into(),
                checked: self.enabled,
                activate: Box::new(|t: &mut Self| {
                    t.enabled = !t.enabled;
                    (t.on_action)(Action::SetEnabled(t.enabled));
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Start on login".into(),
                checked: self.autostart,
                activate: Box::new(|t: &mut Self| {
                    t.autostart = !t.autostart;
                    (t.on_action)(Action::SetAutostart(t.autostart));
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| (t.on_action)(Action::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Rasterize the app icon into the ARGB32 pixmaps the tray protocol wants.
fn render_icon() -> Vec<Icon> {
    let Ok(tree) =
        resvg::usvg::Tree::from_data(include_bytes!("../ui/icon.svg"), &Default::default())
    else {
        return Vec::new();
    };
    [22u32, 32, 48, 64]
        .into_iter()
        .filter_map(|size| {
            let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
            let scale = size as f32 / tree.size().width();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(scale, scale),
                &mut pixmap.as_mut(),
            );
            let data = pixmap
                .pixels()
                .iter()
                .flat_map(|p| {
                    let c = p.demultiply();
                    [c.alpha(), c.red(), c.green(), c.blue()]
                })
                .collect();
            Some(Icon {
                width: size as i32,
                height: size as i32,
                data,
            })
        })
        .collect()
}

/// Tray availability against a fake StatusNotifierWatcher that starts late,
/// disappears, and returns. Needs a private session bus, never the desktop's:
/// run with ESQUIE_PRIVATE_BUS=1 under dbus-run-session.
#[cfg(test)]
mod live {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Duration;

    struct Watcher(Arc<Mutex<Vec<String>>>);

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl Watcher {
        fn register_status_notifier_item(&self, service: &str) {
            self.0.lock().unwrap().push(service.to_owned());
        }

        fn register_status_notifier_host(&self, _service: &str) {}

        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn registered_status_notifier_items(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        #[zbus(property)]
        fn protocol_version(&self) -> i32 {
            0
        }
    }

    fn start_watcher(items: &Arc<Mutex<Vec<String>>>) -> zbus::blocking::Connection {
        zbus::blocking::connection::Builder::session()
            .unwrap()
            .name("org.kde.StatusNotifierWatcher")
            .unwrap()
            .serve_at("/StatusNotifierWatcher", Watcher(items.clone()))
            .unwrap()
            .build()
            .unwrap()
    }

    fn next(rx: &Receiver<bool>) -> bool {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("availability change")
    }

    fn assert_quiet(rx: &Receiver<bool>) {
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(300)).ok(),
            None,
            "unexpected availability change"
        );
    }

    fn spawn_reporting() -> (Handle, bool, Receiver<bool>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let (handle, available) = spawn(true, false, move |action| {
            if let Action::Available(on) = action {
                tx.lock().unwrap().send(on).unwrap();
            }
        })
        .expect("tray service on the private bus");
        (handle, available, rx)
    }

    #[test]
    #[ignore = "needs ESQUIE_PRIVATE_BUS=1 and a private D-Bus session"]
    fn tray_availability() {
        assert_eq!(
            std::env::var("ESQUIE_PRIVATE_BUS").as_deref(),
            Ok("1"),
            "refusing to fake a tray watcher on the desktop's session bus"
        );
        let items = Arc::new(Mutex::new(Vec::new()));

        // No tray host yet: the service still starts and reports unavailable.
        let (handle, available, rx) = spawn_reporting();
        assert!(!available);
        assert!(!next(&rx));
        assert_quiet(&rx);

        // The tray starts late: the item registers and becomes available.
        let watcher = start_watcher(&items);
        assert!(next(&rx));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(items.lock().unwrap().len(), 1);
        assert_quiet(&rx);

        // The tray disappears, then returns.
        drop(watcher);
        assert!(!next(&rx));
        let watcher = start_watcher(&items);
        assert!(next(&rx));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(items.lock().unwrap().len(), 2);

        // With a tray present at startup, it is available immediately.
        let (second, available, rx2) = spawn_reporting();
        assert!(available);
        assert_quiet(&rx2);
        assert_eq!(items.lock().unwrap().len(), 3);

        second.shutdown().wait();
        handle.shutdown().wait();
        drop(watcher);
    }
}
