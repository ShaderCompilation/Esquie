//! System tray icon (StatusNotifierItem). Works on KDE, and on GNOME with the
//! AppIndicator extension.

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem};

pub enum Action {
    ToggleWindow,
    ShowWindow,
    SetEnabled(bool),
    SetAutostart(bool),
    Quit,
}

pub struct Tray {
    pub enabled: bool,
    pub autostart: bool,
    icon: Vec<Icon>,
    on_action: Box<dyn Fn(Action) + Send>,
}

pub type Handle = ksni::blocking::Handle<Tray>;

/// Returns `None` when the desktop has no tray to show the icon in.
pub fn spawn(enabled: bool, autostart: bool, on_action: impl Fn(Action) + Send + 'static) -> Option<Handle> {
    let tray = Tray { enabled, autostart, icon: render_icon(), on_action: Box::new(on_action) };
    match tray.spawn() {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("System tray not available: {e}");
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
            description: if self.enabled { "Noise removal is on" } else { "Noise removal is off" }.into(),
            ..Default::default()
        }
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
    let Ok(tree) = resvg::usvg::Tree::from_data(include_bytes!("../ui/icon.svg"), &Default::default()) else {
        return Vec::new();
    };
    [22u32, 32, 48, 64]
        .into_iter()
        .filter_map(|size| {
            let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
            let scale = size as f32 / tree.size().width();
            resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
            let data = pixmap
                .pixels()
                .iter()
                .flat_map(|p| {
                    let c = p.demultiply();
                    [c.alpha(), c.red(), c.green(), c.blue()]
                })
                .collect();
            Some(Icon { width: size as i32, height: size as i32, data })
        })
        .collect()
}
