//! UI orchestration; copying and filesystem integration run off the audio/UI threads.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::{
    AppWindow,
    installation::{self, Inventory, Kind, Locations, Outcome},
    settings::{InstallPrompt, Settings},
};
use slint::ComponentHandle;

/// Lock a mutex, recovering from poisoning: a panic in another thread must
/// not take the UI down too, and the guarded data is still readable.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn setup(ui: &AppWindow, settings: Arc<Mutex<Settings>>) -> Arc<Mutex<Option<PathBuf>>> {
    let handoff = Arc::new(Mutex::new(None));
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    let initial = lock(&settings).clone();
    ui.set_automatic_checks(initial.automatic_checks);
    if let Ok(locations) = Locations::get() {
        ui.set_install_directory(
            initial
                .install_directory
                .clone()
                .unwrap_or(locations.default_bin)
                .to_string_lossy()
                .as_ref()
                .into(),
        );
    }
    refresh(ui, &initial);
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.on_consider_install_prompt(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_prompt_considered() {
                return;
            }
            ui.set_prompt_considered(true);
            let settings = lock(&settings);
            let eligible = Locations::get()
                .and_then(|l| Inventory::current(&l, &settings))
                .is_ok_and(|i| {
                    i.should_prompt(settings.install_prompt, true, cfg!(debug_assertions))
                });
            ui.set_install_prompt(eligible);
        });
    }
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.on_dismiss_install_prompt(move |never| {
            if let Some(ui) = weak.upgrade() {
                ui.set_install_prompt(false);
            }
            let mut s = lock(&settings);
            s.install_prompt = if never {
                InstallPrompt::Never
            } else {
                InstallPrompt::Later
            };
            if let Err(e) = s.try_save()
                && let Some(ui) = weak.upgrade()
            {
                ui.set_settings_open(true);
                ui.set_installation_message(
                    format!("Could not save your prompt preference: {e}").into(),
                );
            }
        });
    }
    {
        let settings = settings.clone();
        ui.on_directory_changed(move |value| {
            let mut s = lock(&settings);
            s.install_directory = Some(PathBuf::from(value.as_str()));
            s.save();
        });
    }
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.on_installation_requested(move |action| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_installation_busy() {
                return;
            }
            ui.set_installation_busy(true);
            ui.set_install_collision(false);
            let directory = PathBuf::from(ui.get_install_directory().as_str());
            let settings = settings.clone();
            let weak = ui.as_weak();
            let error_weak = ui.as_weak();
            let spawn = std::thread::Builder::new()
                .name("esquie-install".into())
                .spawn(move || run_operation(action, directory, settings, weak));
            if let Err(e) = spawn
                && let Some(ui) = error_weak.upgrade()
            {
                ui.set_installation_busy(false);
                ui.set_installation_message(format!("Could not start installation: {e}").into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let handoff = handoff.clone();
        ui.on_switch_installation(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_installation_busy() {
                return;
            }
            if let Some(target) = lock(&settings).installed_path.clone() {
                if target.is_file() {
                    *lock(&handoff) = Some(target);
                    let _ = slint::quit_event_loop();
                } else {
                    ui.set_installation_message(
                        "The installed copy is missing. Install or repair it first.".into(),
                    );
                }
            }
        });
    }
    handoff
}

fn run_operation(
    action: i32,
    directory: PathBuf,
    settings: Arc<Mutex<Settings>>,
    weak: slint::Weak<AppWindow>,
) {
    let initial = lock(&settings).clone();
    let result = operate(action, &directory, &initial);
    // Merge into the latest settings so audio edits during the copy survive.
    let mut save_error = None;
    if let Ok(Outcome::Complete { target, .. }) = &result {
        let mut s = lock(&settings);
        if action != 4 {
            s.installed_path = Some(target.clone());
        }
        s.install_directory = Some(directory);
        s.install_prompt = InstallPrompt::Never;
        save_error = s.try_save().err();
    }
    let snapshot = lock(&settings).clone();
    let _ =
        weak.upgrade_in_event_loop(move |ui| finish(&ui, action, result, &snapshot, save_error));
}

fn finish(
    ui: &AppWindow,
    action: i32,
    result: std::io::Result<Outcome>,
    snapshot: &Settings,
    save_error: Option<std::io::Error>,
) {
    ui.set_installation_busy(false);
    match result {
        Ok(Outcome::Collision(path)) => {
            ui.set_collision_path(path.to_string_lossy().as_ref().into());
            ui.set_install_collision(true);
            ui.set_installation_message("Choose another directory or confirm replacement.".into());
        }
        Ok(Outcome::Complete { target, external }) => {
            ui.set_install_prompt(false);
            ui.set_installation_options_open(false);
            let message = if action == 4 {
                "Esquie-owned launchers removed. The executable and startup preference were kept."
                    .to_string()
            } else if external {
                format!(
                    "Ready at {}. External launchers were preserved; use their manager to change their paths.",
                    target.display()
                )
            } else {
                format!(
                    "Ready at {}. Launchers and enabled Esquie startup use this path. Current audio keeps running.",
                    target.display()
                )
            };
            let message = if let Some(e) = save_error {
                format!(
                    "{message}\nCould not save the selected path: {e}. Save it again before quitting."
                )
            } else {
                message
            };
            ui.set_installation_message(message.into());
        }
        Err(e) => {
            ui.set_installation_message(format!("Could not complete the operation: {e}").into())
        }
    }
    refresh(ui, snapshot);
}

fn operate(
    action: i32,
    directory: &std::path::Path,
    initial: &Settings,
) -> std::io::Result<Outcome> {
    let locations = Locations::get()?;
    let inventory = Inventory::current(&locations, initial)?;
    match action {
        0 | 1 => installation::install(&locations, &inventory, directory, action == 1),
        2 | 3 => {
            let target = if action == 2 {
                inventory.executable.clone()
            } else {
                initial
                    .installed_path
                    .clone()
                    .unwrap_or(inventory.executable.clone())
            };
            let external = installation::integrate(&locations, &inventory, &target)?;
            Ok(Outcome::Complete { target, external })
        }
        4 => {
            installation::remove_launchers(&locations, &inventory)?;
            Ok(Outcome::Complete {
                target: initial
                    .installed_path
                    .clone()
                    .unwrap_or(inventory.executable),
                external: false,
            })
        }
        _ => Err(std::io::Error::other("Unknown installation operation")),
    }
}

fn refresh(ui: &AppWindow, settings: &Settings) {
    ui.set_installed_path(
        settings
            .installed_path
            .as_ref()
            .filter(|path| path.is_file())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into(),
    );
    match Locations::get().and_then(|l| Inventory::current(&l, settings)) {
        Ok(inventory) => {
            let mut info = inventory.description();
            if inventory.kind == Kind::Extracted {
                info.push_str("\nKeep this directory intact. Use a new extraction to update it; launcher-only setup is available here.");
            } else if inventory.kind == Kind::Binary {
                info.push_str("\nUpdate source binaries by rebuilding or using install.sh.");
            }
            if let Some(target) = &settings.installed_path {
                info.push_str(&format!("\nSelected persistent copy: {}", target.display()));
            }
            ui.set_installation_info(info.into());
            ui.set_can_copy(inventory.kind != Kind::Extracted);
            ui.set_can_switch(settings.installed_path.as_ref().is_some_and(|p| {
                p.is_file() && !installation::same_path(p, &inventory.executable)
            }));
        }
        Err(e) => {
            ui.set_installation_info(format!("Could not inspect installation: {e}").into());
            ui.set_can_copy(false);
            ui.set_can_switch(false);
        }
    }
}
