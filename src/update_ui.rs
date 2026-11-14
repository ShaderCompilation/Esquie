//! Update UI: Settings status, the notice, and the updater threads' lifecycle.
//! Network checks run on the updater thread and downloads on their own thread;
//! this module only starts, cancels, and reacts to them.
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use crate::{
    AppWindow,
    installation::{self, Inventory, Kind, Locations},
    jobs::{Job, Jobs},
    settings::Settings,
    updater::{self, Outcome, Release, Report, Request},
};

/// Progress reaches the UI at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// How long quit waits for a cancelled download to clean up after itself.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

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
    download: Option<Download>,
    staged: Option<Staged>,
}

/// The download in progress. At most one exists, and it holds the job slot it
/// shares with installation.
struct Download {
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    partial: PathBuf,
}

/// A downloaded AppImage whose digest matched the signed checksum, waiting
/// beside the copy it replaces.
#[allow(dead_code)] // `path` and `target` are read by replacement.
struct Staged {
    version: semver::Version,
    path: PathBuf,
    target: PathBuf,
}

/// Keeps the update machinery alive until quit; see [`Updates::shutdown`].
pub struct Updates {
    requests: Option<Sender<Request>>,
    state: Rc<RefCell<State>>,
}

impl Updates {
    /// Stop update work for quit without letting it delay exit. The checker
    /// thread ends once any in-flight request finishes and is never joined. A
    /// download is cancelled and gets a short grace period to remove its
    /// partial file; a download stuck in a slow read has the file removed
    /// for it (it writes through its open handle until the process exits).
    pub fn shutdown(self) {
        drop(self.requests);
        let Some(download) = self.state.borrow_mut().download.take() else {
            return;
        };
        download.cancel.store(true, Ordering::Relaxed);
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while !download.thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !download.thread.is_finished() {
            tracing::info!("update download still stopping at quit; removing its partial file");
            let _ = std::fs::remove_file(&download.partial);
        }
    }
}

/// Start the updater thread and wire the update controls.
pub fn setup(ui: &AppWindow, settings: Arc<Mutex<Settings>>, jobs: Arc<Jobs>) -> Updates {
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
            ui.set_update_notice(false);
            ui.set_settings_open(true);
            if let Err(message) = start_download(&ui, &state, &settings, &jobs) {
                ui.set_update_error(true);
                ui.set_update_download_failed(true);
                ui.set_update_status(message.into());
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_cancel_update(move || {
            if let Some(download) = &state.borrow().download {
                download.cancel.store(true, Ordering::Relaxed);
                if let Some(ui) = weak.upgrade() {
                    ui.set_update_status("Cancelling the download…".into());
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
    Updates {
        requests: tx,
        state,
    }
}

/// Start downloading the offered release beside the selected persistent copy.
/// Ignored while a download runs; refused while an installation runs.
fn start_download(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    settings: &Arc<Mutex<Settings>>,
    jobs: &Arc<Jobs>,
) -> Result<(), String> {
    let mut st = state.borrow_mut();
    if st.download.is_some() {
        return Ok(());
    }
    let Some(release) = st.release.clone() else {
        return Ok(());
    };
    let (installable, instructions) = manual_path();
    if !installable {
        return Err(instructions);
    }
    let target = installation::selected_executable(&lock(settings))
        .map_err(|e| format!("Could not find the copy to update: {e}"))?;
    let (partial, _) = updater::staging_paths(&target, &release.version)
        .ok_or_else(|| format!("{} has no directory to update in.", target.display()))?;
    let guard = jobs.begin(Job::Update).map_err(|_| {
        "Esquie is installing or repairing a copy. Update once that finishes.".to_string()
    })?;
    let cancel = Arc::new(AtomicBool::new(false));
    let thread = {
        let cancel = cancel.clone();
        let weak = ui.as_weak();
        let release = release.clone();
        let target = target.clone();
        std::thread::Builder::new()
            .name("esquie-update-download".into())
            .spawn(move || {
                let transport = updater::Http::new();
                let total = release.appimage.size;
                let mut reported: Option<Instant> = None;
                let mut progress = |done: u64| {
                    if done < total && reported.is_some_and(|t| t.elapsed() < PROGRESS_INTERVAL) {
                        return;
                    }
                    reported = Some(Instant::now());
                    let version = release.version.clone();
                    let _ = weak
                        .upgrade_in_event_loop(move |ui| show_progress(&ui, &version, done, total));
                };
                tracing::info!(
                    "downloading {} beside {}",
                    release.appimage.name,
                    target.display()
                );
                let result = updater::stage(&transport, &release, &target, &cancel, &mut progress);
                match &result {
                    Ok(path) => tracing::info!("update verified at {}", path.display()),
                    Err(e) => tracing::info!("update download stopped: {e}"),
                }
                drop(guard);
                let _ = weak
                    .upgrade_in_event_loop(move |ui| finish_download(&ui, release, target, result));
            })
            .map_err(|e| format!("Could not start the download: {e}"))?
    };
    st.download = Some(Download {
        cancel,
        thread,
        partial,
    });
    st.staged = None;
    ui.set_update_downloading(true);
    ui.set_update_ready(false);
    ui.set_update_error(false);
    ui.set_update_download_failed(false);
    show_progress(ui, &release.version, 0, release.appimage.size);
    Ok(())
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1_000_000.0)
}

fn show_progress(ui: &AppWindow, version: &semver::Version, done: u64, total: u64) {
    let fraction = if total == 0 {
        0.0
    } else {
        done as f64 / total as f64
    };
    // A cancel request keeps its status until the thread reports back.
    if !ui.get_update_downloading() || ui.get_update_status().starts_with("Cancelling") {
        return;
    }
    ui.set_update_progress(fraction as f32);
    ui.set_update_status(
        format!(
            "Downloading Esquie {version}: {} of {} MB ({:.0}%). Esquie and your microphone keep working meanwhile.",
            megabytes(done),
            megabytes(total),
            fraction * 100.0
        )
        .into(),
    );
}

fn finish_download(
    ui: &AppWindow,
    release: Release,
    target: PathBuf,
    result: Result<PathBuf, updater::Error>,
) {
    let Some(state) = STATE.with(|s| s.borrow().clone()) else {
        return;
    };
    state.borrow_mut().download = None;
    ui.set_update_downloading(false);
    ui.set_update_progress(0.0);
    match result {
        Ok(path) => {
            ui.set_update_ready(true);
            ui.set_update_error(false);
            ui.set_update_download_failed(false);
            ui.set_update_status(ready_steps(&release, &path, &target).into());
            state.borrow_mut().staged = Some(Staged {
                version: release.version,
                path,
                target,
            });
        }
        Err(updater::Error::Cancelled) => {
            ui.set_update_error(false);
            ui.set_update_download_failed(false);
            ui.set_update_status(
                format!(
                    "The download was cancelled. Esquie {} is still available.",
                    release.version
                )
                .into(),
            );
        }
        Err(e) => {
            ui.set_update_error(true);
            ui.set_update_download_failed(true);
            ui.set_update_status(format!("{e} Your current copy is unchanged.").into());
        }
    }
}

/// Until Esquie replaces itself, the verified file waits beside the target.
fn ready_steps(release: &Release, path: &Path, target: &Path) -> String {
    format!(
        "Esquie {} was downloaded and matches its signed checksum. Current audio keeps running. \
         To finish, quit Esquie, replace {} with {} (keep the same name), then start Esquie again.",
        release.version,
        target.display(),
        path.display()
    )
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
    // A download in progress owns the status line; so does a staged update
    // while the check still offers that same version.
    {
        let st = state.borrow();
        let same_staged = matches!(
            (&report.result, &st.staged),
            (Ok(Outcome::Available(r)), Some(s)) if r.version == s.version
        );
        if st.download.is_some() || same_staged {
            return;
        }
    }
    // A different outcome supersedes a staged or failed download; a failed
    // check does not.
    if report.result.is_ok() {
        if state.borrow_mut().staged.take().is_some() {
            ui.set_update_ready(false);
        }
        ui.set_update_download_failed(false);
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
