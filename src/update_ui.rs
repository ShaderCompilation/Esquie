//! Update UI: Settings status, the notice, and the updater thread's lifecycle.
//! Network checks run on the updater thread; this module only reacts to them.
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use slint::ComponentHandle;

use crate::{
    AppWindow,
    installation::{Inventory, Kind, Locations},
    settings::Settings,
    updater::{self, Outcome, Release, Report, Request},
};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Default)]
struct State {
    release: Option<Release>,
    /// "Later" hides the notice for this version until the next launch.
    dismissed: Option<semver::Version>,
}

/// Start the updater thread and wire the update controls. Dropping the
/// returned sender ends the thread once any in-flight request finishes; quit
/// never waits for it.
pub fn setup(ui: &AppWindow, settings: Arc<Mutex<Settings>>) -> Option<Sender<Request>> {
    ui.set_automatic_checks(lock(&settings).automatic_checks);
    let state = Rc::new(RefCell::new(State::default()));
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = {
        let settings = settings.clone();
        let weak = ui.as_weak();
        std::thread::Builder::new()
            .name("esquie-updater".into())
            .spawn(move || {
                let transport = updater::Http::new();
                updater::run(
                    rx,
                    settings,
                    &transport,
                    updater::STARTUP_DELAY,
                    Settings::save,
                    move |report| {
                        let _ = weak.upgrade_in_event_loop(move |ui| apply(&ui, report));
                    },
                )
            })
    };
    let tx = match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::warn!("Could not start the updater: {e}");
            ui.set_update_status(format!("Update checks are unavailable: {e}").into());
            None
        }
    };
    // Reports cross threads, so they find the UI-thread state through this.
    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    {
        let weak = ui.as_weak();
        let tx = tx.clone();
        ui.on_check_updates(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_update_checking() {
                return;
            }
            match tx.as_ref().map(|tx| tx.send(Request::CheckNow)) {
                Some(Ok(())) => {
                    ui.set_update_checking(true);
                    ui.set_update_error(false);
                    ui.set_update_status("Checking GitHub for updates…".into());
                }
                _ => {
                    ui.set_update_error(true);
                    ui.set_update_status("Update checks are unavailable in this session.".into());
                }
            }
        });
    }
    {
        let settings = settings.clone();
        let tx = tx.clone();
        let weak = ui.as_weak();
        ui.on_automatic_checks_changed(move |on| {
            let mut s = lock(&settings);
            s.automatic_checks = on;
            if let Err(e) = s.try_save()
                && let Some(ui) = weak.upgrade()
            {
                ui.set_update_error(true);
                ui.set_update_status(format!("Could not save this preference: {e}").into());
            }
            drop(s);
            if let Some(tx) = &tx {
                let _ = tx.send(Request::Reschedule);
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_open_release_notes(move || {
            let page = state
                .borrow()
                .release
                .as_ref()
                .map_or(updater::RELEASES_PAGE.to_string(), |r| r.page.clone());
            if let Err(e) = open_url(&page)
                && let Some(ui) = weak.upgrade()
            {
                ui.set_update_error(true);
                ui.set_update_status(
                    format!("Could not open your browser ({e}). Visit {page} instead.").into(),
                );
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.on_start_update(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(release) = state.borrow().release.clone() else {
                return;
            };
            ui.set_update_notice(false);
            ui.set_settings_open(true);
            // Esquie does not replace itself yet: the verified asset downloads
            // in the browser, and the user replaces the file.
            match open_url(&release.appimage.url) {
                Ok(()) => {
                    ui.set_update_error(false);
                    let target = lock(&settings)
                        .installed_path
                        .clone()
                        .or_else(|| crate::desktop_entry::executable().ok());
                    ui.set_update_status(manual_appimage_steps(&release, target).into());
                }
                Err(e) => {
                    ui.set_update_error(true);
                    ui.set_update_status(
                        format!(
                            "Could not open your browser ({e}). Download {} from {} instead.",
                            release.appimage.name, release.page
                        )
                        .into(),
                    );
                }
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_dismiss_update_notice(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_update_notice(false);
            }
            let mut state = state.borrow_mut();
            state.dismissed = state.release.as_ref().map(|r| r.version.clone());
        });
    }
    tx
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

fn apply(ui: &AppWindow, report: Report) {
    let Some(state) = STATE.with(|s| s.borrow().clone()) else {
        return;
    };
    if report.explicit {
        ui.set_update_checking(false);
    }
    match report.result {
        Ok(Outcome::UpToDate) => {
            state.borrow_mut().release = None;
            ui.set_update_version("".into());
            ui.set_update_notice(false);
            ui.set_update_error(false);
            ui.set_update_status(
                format!("Esquie {} is up to date.", env!("CARGO_PKG_VERSION")).into(),
            );
        }
        Ok(Outcome::Available(release)) => {
            let version = release.version.to_string();
            let (installable, instructions) = manual_path();
            ui.set_update_installable(installable);
            ui.set_update_instructions(instructions.into());
            ui.set_update_version(version.as_str().into());
            ui.set_update_error(false);
            ui.set_update_status(format!("Esquie {version} is available.").into());
            let dismissed = state.borrow().dismissed.as_ref() == Some(&release.version);
            ui.set_update_notice(!dismissed);
            state.borrow_mut().release = Some(release);
        }
        // Background failures stay quiet; the updater thread already logged them.
        Err(e) if report.explicit => {
            ui.set_update_error(true);
            ui.set_update_status(e.to_string().into());
        }
        Err(_) => {}
    }
}

/// Whether this copy is a replaceable AppImage, and instructions otherwise.
fn manual_path() -> (bool, String) {
    let kind = Locations::get()
        .and_then(|l| Inventory::current(&l, &Settings::default()))
        .map(|i| i.kind);
    match kind {
        Ok(Kind::AppImage) => (true, String::new()),
        Ok(Kind::Extracted) => (
            false,
            "This is an extracted AppImage. Download the new AppImage from the release page and extract it again to update."
                .into(),
        ),
        Ok(Kind::Binary) => (
            false,
            "This copy was built from source. Update the source to the new release, then rebuild or run install.sh."
                .into(),
        ),
        Err(_) => (
            false,
            "Download the new release from its release page.".into(),
        ),
    }
}

fn manual_appimage_steps(release: &Release, target: Option<PathBuf>) -> String {
    let target = target.map_or("your current Esquie file".to_string(), |t| {
        t.display().to_string()
    });
    format!(
        "{} is downloading in your browser. Its checksum file is published beside it on the release page. \
         To finish, quit Esquie, replace {target} with the downloaded file (keep the same name and make it executable), then start Esquie again.",
        release.appimage.name
    )
}

/// Open a GitHub address in the desktop's browser without inheriting the
/// AppImage's mount variables.
fn open_url(url: &str) -> std::io::Result<()> {
    if !url.starts_with("https://github.com/") {
        return Err(std::io::Error::other("unexpected address"));
    }
    let mut child = std::process::Command::new("xdg-open")
        .arg(url)
        .env_remove("APPIMAGE")
        .env_remove("APPDIR")
        .env_remove("ARGV0")
        .stdin(std::process::Stdio::null())
        .spawn()?;
    // Reap it off the UI thread.
    let _ = std::thread::Builder::new()
        .name("esquie-open-url".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}
