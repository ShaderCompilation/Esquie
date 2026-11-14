//! Update UI: Settings status, the notice, and the updater threads' lifecycle.
//! Network checks run on the updater thread; downloading and replacing the
//! selected AppImage run on their own thread. This module only starts,
//! cancels, and reacts to them, and hands a restart to `main`.
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
    replacement::{self, Installed, Target},
    settings::Settings,
    updater::{self, Outcome, Release, Report, Request},
};

/// Progress reaches the UI at most this often.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
/// How long quit waits for a cancelled download to clean up after itself.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// A replacement past its commit point is local renames; quit waits for it,
/// but never forever.
const COMMIT_LIMIT: Duration = Duration::from_secs(30);

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
    installed: Option<Update>,
}

/// The update job in progress: download, verification, then replacement. At
/// most one exists, and it holds the job slot it shares with installation.
struct Download {
    cancel: Arc<AtomicBool>,
    /// Set once replacement passed the point where it can be cancelled.
    committing: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    partial: PathBuf,
}

/// An update that replaced the selected copy and awaits a restart.
struct Update {
    version: semver::Version,
    /// The selected persistent path, launched by Restart now.
    selected: PathBuf,
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
    /// A replacement already committing is allowed to finish.
    pub fn shutdown(self) {
        drop(self.requests);
        let Some(download) = self.state.borrow_mut().download.take() else {
            return;
        };
        download.cancel.store(true, Ordering::Relaxed);
        let started = Instant::now();
        while !download.thread.is_finished() {
            let committing = download.committing.load(Ordering::Relaxed);
            let limit = if committing {
                COMMIT_LIMIT
            } else {
                SHUTDOWN_GRACE
            };
            if started.elapsed() >= limit {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if download.thread.is_finished() {
            return;
        }
        if download.committing.load(Ordering::Relaxed) {
            tracing::warn!(
                "update replacement still running at quit; the next start finishes cleanup"
            );
        } else {
            tracing::info!("update download still stopping at quit; removing its partial file");
            let _ = std::fs::remove_file(&download.partial);
        }
    }
}

/// Start the updater thread and wire the update controls. Restart now puts
/// the updated copy in `handoff` and ends the event loop; `main` then stops
/// audio, releases the instance lock, and launches it.
pub fn setup(
    ui: &AppWindow,
    settings: Arc<Mutex<Settings>>,
    jobs: Arc<Jobs>,
    handoff: Arc<Mutex<Option<PathBuf>>>,
) -> Updates {
    ui.set_automatic_checks(lock(&settings).automatic_checks);
    let state = Rc::new(RefCell::new(State::default()));
    if let Some(newer) = lock(&settings).written_by_newer() {
        tracing::warn!("settings were last saved by newer Esquie {newer}");
        let copy = Settings::newer_copy_path(&newer)
            .map_or_else(String::new, |p| format!(" at {}", p.display()));
        ui.set_update_status(
            format!(
                "Your settings were last saved by Esquie {newer}, which is newer than this copy ({}). \
                 Settings this version does not recognize are kept, and the original file is \
                 copied{copy} before this version first changes anything.",
                env!("CARGO_PKG_VERSION")
            )
            .into(),
        );
    }
    clean_abandoned_later(settings.clone(), jobs.clone());
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
            if ui.get_update_checking() || ui.get_update_ready() {
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
                if download.committing.load(Ordering::Relaxed) {
                    return;
                }
                download.cancel.store(true, Ordering::Relaxed);
                if let Some(ui) = weak.upgrade() {
                    ui.set_update_status("Cancelling the update…".into());
                }
            }
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_restart_update(move || {
            let Some(ui) = weak.upgrade() else { return };
            let st = state.borrow();
            let Some(update) = &st.installed else { return };
            if !update.selected.is_file() {
                ui.set_update_error(true);
                ui.set_update_status(
                    format!(
                        "{} is missing, so Esquie cannot restart into it. Start Esquie again from your menu.",
                        update.selected.display()
                    )
                    .into(),
                );
                return;
            }
            tracing::info!(
                "restarting into Esquie {} at {}",
                update.version,
                update.selected.display()
            );
            *lock(&handoff) = Some(update.selected.clone());
            let _ = slint::quit_event_loop();
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

/// Download the offered release beside the selected persistent copy, then
/// replace that copy. Ignored while an update runs; refused while an
/// installation runs.
fn start_download(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    settings: &Arc<Mutex<Settings>>,
    jobs: &Arc<Jobs>,
) -> Result<(), String> {
    let mut st = state.borrow_mut();
    if st.download.is_some() || st.installed.is_some() {
        return Ok(());
    }
    let Some(release) = st.release.clone() else {
        return Ok(());
    };
    let (installable, instructions) = manual_path();
    if !installable {
        return Err(instructions);
    }
    let selected = installation::selected_executable(&lock(settings))
        .map_err(|e| format!("Could not find the copy to update: {e}"))?;
    // Staging goes beside the file the selection resolves to, so the final
    // rename stays within one directory.
    let resolved = std::fs::canonicalize(&selected)
        .map_err(|e| format!("Could not find the copy to update: {e}"))?;
    let (partial, _) = updater::staging_paths(&resolved, &release.version)
        .ok_or_else(|| format!("{} has no directory to update in.", resolved.display()))?;
    let guard = jobs.begin(Job::Update).map_err(|_| {
        "Esquie is installing or repairing a copy. Update once that finishes.".to_string()
    })?;
    let cancel = Arc::new(AtomicBool::new(false));
    let committing = Arc::new(AtomicBool::new(false));
    let thread = {
        let cancel = cancel.clone();
        let committing = committing.clone();
        let weak = ui.as_weak();
        let release = release.clone();
        std::thread::Builder::new()
            .name("esquie-update-download".into())
            .spawn(move || {
                let result = run_update(&release, &selected, &cancel, &committing, &weak);
                match &result {
                    Ok((target, _)) => tracing::info!(
                        "installed Esquie {} at {}",
                        release.version,
                        target.file.display()
                    ),
                    Err(e) => tracing::info!("update stopped: {e}"),
                }
                drop(guard);
                let _ = weak.upgrade_in_event_loop(move |ui| finish_update(&ui, release, result));
            })
            .map_err(|e| format!("Could not start the download: {e}"))?
    };
    st.download = Some(Download {
        cancel,
        committing,
        thread,
        partial,
    });
    ui.set_update_downloading(true);
    ui.set_update_installing(false);
    ui.set_update_ready(false);
    ui.set_update_error(false);
    ui.set_update_download_failed(false);
    show_progress(ui, &release.version, 0, release.appimage.size);
    Ok(())
}

/// The update thread: record the target, download and verify beside it, then
/// replace it. The verified download is removed if replacement fails.
fn run_update(
    release: &Release,
    selected: &Path,
    cancel: &AtomicBool,
    committing: &AtomicBool,
    weak: &slint::Weak<AppWindow>,
) -> Result<(Target, Installed), updater::Error> {
    let target = replacement::inspect(selected, cancel)?;
    let transport = updater::Http::new();
    let total = release.appimage.size;
    let mut reported: Option<Instant> = None;
    let mut progress = |done: u64| {
        if done < total && reported.is_some_and(|t| t.elapsed() < PROGRESS_INTERVAL) {
            return;
        }
        reported = Some(Instant::now());
        let version = release.version.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| show_progress(&ui, &version, done, total));
    };
    tracing::info!(
        "downloading {} beside {}",
        release.appimage.name,
        target.file.display()
    );
    let staged = updater::stage(&transport, release, &target.file, cancel, &mut progress)?;
    let version = release.version.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if ui.get_update_status().starts_with("Cancelling") {
            return;
        }
        ui.set_update_installing(true);
        ui.set_update_progress(1.0);
        ui.set_update_status(format!("Installing Esquie {version}…").into());
    });
    let result = replacement::replace(&target, &staged, release, cancel, committing);
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result.map(|installed| (target, installed))
}

/// Remove what an interrupted update left beside the selected AppImage. Runs
/// once at startup, holding the job slot so no update starts meanwhile.
fn clean_abandoned_later(settings: Arc<Mutex<Settings>>, jobs: Arc<Jobs>) {
    let spawned = std::thread::Builder::new()
        .name("esquie-update-cleanup".into())
        .spawn(move || {
            let Ok(_guard) = jobs.begin(Job::Update) else {
                return;
            };
            if !manual_path().0 {
                return;
            }
            let selected = installation::selected_executable(&lock(&settings));
            let Ok(file) = selected.and_then(std::fs::canonicalize) else {
                return;
            };
            for path in replacement::clean_abandoned(&file) {
                tracing::info!("cleaned up after an interrupted update: {}", path.display());
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("could not start update cleanup: {e}");
    }
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
    if !ui.get_update_downloading()
        || ui.get_update_installing()
        || ui.get_update_status().starts_with("Cancelling")
    {
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

fn finish_update(
    ui: &AppWindow,
    release: Release,
    result: Result<(Target, Installed), updater::Error>,
) {
    let Some(state) = STATE.with(|s| s.borrow().clone()) else {
        return;
    };
    state.borrow_mut().download = None;
    ui.set_update_downloading(false);
    ui.set_update_installing(false);
    ui.set_update_progress(0.0);
    match result {
        Ok((target, installed)) => {
            ui.set_update_ready(true);
            ui.set_update_restart_deferred(false);
            ui.set_update_notice(false);
            ui.set_update_error(false);
            ui.set_update_download_failed(false);
            ui.set_update_status(installed_message(&release, &target, &installed).into());
            state.borrow_mut().installed = Some(Update {
                version: release.version,
                selected: target.selected,
            });
        }
        Err(updater::Error::Cancelled) => {
            ui.set_update_error(false);
            ui.set_update_download_failed(false);
            ui.set_update_status(
                format!(
                    "The update was cancelled. Your current copy is unchanged, and Esquie {} is still available.",
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

fn installed_message(release: &Release, target: &Target, installed: &Installed) -> String {
    let backup = installed.backup.as_ref().map_or_else(String::new, |b| {
        format!(" The previous version is kept at {}.", b.display())
    });
    format!(
        "Esquie {} is installed at {}. This session keeps running the current version until you \
         restart. Restarting briefly interrupts the virtual microphone; Later keeps it running, \
         and Esquie {} starts next time.{backup}",
        release.version,
        target.selected.display(),
        release.version,
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
    // An update in progress owns the status line; so does an installed update
    // awaiting restart, which no later check can undo.
    {
        let st = state.borrow();
        if st.download.is_some() || st.installed.is_some() {
            return;
        }
    }
    // A different outcome supersedes a failed download; a failed check does not.
    if report.result.is_ok() {
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
