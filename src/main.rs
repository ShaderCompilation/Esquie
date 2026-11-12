mod audio;
mod autostart;
mod desktop_entry;
mod dsp;
mod settings;
mod tray;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audio::{Command, Event, PreviewMode, Shared, Status};
use settings::{Settings, strength_to_db};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

/// How long a hidden window waits for a tray to (re)appear before showing
/// itself, so login races and tray restarts don't pop the window up.
const TRAY_GRACE: Duration = Duration::from_secs(3);

fn main() -> anyhow::Result<()> {
    // Handle these before settings, desktop services, or the audio engine exist.
    let has_arg = |flag: &str| std::env::args_os().skip(1).any(|arg| arg == flag);
    if has_arg("--version") {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // Used by install.sh, so the launcher shares the autostart entry's quoting.
    if has_arg("--print-launcher-entry") {
        print!(
            "{}",
            desktop_entry::launcher(&desktop_entry::executable()?)?
        );
        return Ok(());
    }
    if has_arg("--refresh-autostart") {
        autostart::refresh()?;
        return Ok(());
    }

    // Follow a moved binary or AppImage. Skipped in debug builds so `cargo run`
    // doesn't point login startup at the build directory.
    if !cfg!(debug_assertions)
        && let Err(e) = autostart::refresh()
    {
        eprintln!("Could not update autostart entry: {e}");
    }

    let settings = Arc::new(Mutex::new(Settings::load()));
    let initial = settings.lock().unwrap().clone();
    let shared = Arc::new(Shared::new(
        initial.enabled,
        strength_to_db(initial.strength),
        initial.extra_cleanup,
        initial.gate,
    ));

    let ui = AppWindow::new()?;
    // Must match the .desktop file name so GNOME can link the window to the
    // launcher; otherwise it shows a busy cursor until its startup timeout.
    // Has to run after the platform exists (AppWindow::new) but before show().
    slint::set_xdg_app_id("esquie")?;
    ui.set_enabled(initial.enabled);
    ui.set_strength(initial.strength);
    ui.set_extra_cleanup(initial.extra_cleanup);
    ui.set_gate(initial.gate);
    ui.set_clip_max(audio::MAX_CLIP_SECS as f32);
    ui.set_autostart(autostart::is_enabled());

    // Tray icon. Its callbacks run on the tray thread, so hop to the UI thread.
    // `has-tray` tracks whether the icon is visible right now.
    let spawned = {
        let ui_weak = ui.as_weak();
        tray::spawn(initial.enabled, ui.get_autostart(), move |action| {
            let _ = ui_weak.upgrade_in_event_loop(move |ui| match action {
                tray::Action::ToggleWindow if ui.window().is_visible() => {
                    let _ = ui.hide();
                }
                tray::Action::ToggleWindow | tray::Action::ShowWindow => {
                    let _ = ui.show();
                }
                tray::Action::SetEnabled(on) => {
                    ui.set_enabled(on);
                    ui.invoke_enabled_changed(on);
                }
                tray::Action::SetAutostart(on) => {
                    ui.set_autostart(on);
                    ui.invoke_autostart_changed(on);
                }
                tray::Action::Quit => {
                    let _ = slint::quit_event_loop();
                }
                tray::Action::Available(available) => {
                    ui.set_has_tray(available);
                    if !available {
                        reveal_without_tray_later(&ui);
                    }
                }
            });
        })
    };
    let (tray, tray_available) = match spawned {
        Some((handle, available)) => (Some(handle), available),
        None => (None, false),
    };
    ui.set_has_tray(tray_available);

    // Audio thread (PipeWire + DeepFilterNet).
    let (cmd_tx, cmd_rx) = pipewire::channel::channel::<Command>();
    let audio_thread = {
        let shared = shared.clone();
        let ui_weak = ui.as_weak();
        let mic = initial.mic.clone();
        std::thread::Builder::new()
            .name("esquie-audio".into())
            .spawn(move || {
                let notify_weak = ui_weak.clone();
                let notify = move |event: Event| {
                    let _ = notify_weak.upgrade_in_event_loop(move |ui| apply_event(&ui, event));
                };
                if let Err(e) = audio::run(shared, mic, cmd_rx, notify) {
                    let msg = format!("Audio engine failed: {e:#}");
                    eprintln!("{msg}");
                    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                        apply_event(&ui, Event::Status(Status::Failed(msg)))
                    });
                }
            })?
    };

    {
        let tx = cmd_tx.clone();
        let settings = settings.clone();
        let ui_weak = ui.as_weak();
        ui.on_mic_selected(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            if let Some(name) = ui
                .get_mic_names()
                .row_data(i as usize)
                .map(|n| n.to_string())
            {
                let mut s = settings.lock().unwrap();
                s.mic = Some(name.clone());
                s.save();
                let _ = tx.send(Command::SelectMic(name));
            }
        });
    }
    {
        let shared = shared.clone();
        let settings = settings.clone();
        let tray = tray.clone();
        ui.on_enabled_changed(move |on| {
            shared.enabled.store(on, Ordering::Relaxed);
            let mut s = settings.lock().unwrap();
            s.enabled = on;
            s.save();
            if let Some(t) = &tray {
                t.update(|t| t.enabled = on);
            }
        });
    }
    {
        let tray = tray.clone();
        let ui_weak = ui.as_weak();
        ui.on_autostart_changed(move |on| {
            if let Err(e) = autostart::set_enabled(on) {
                eprintln!("Could not update autostart: {e}");
            }
            // Re-read so the toggle shows what actually happened.
            let actual = autostart::is_enabled();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_autostart(actual);
            }
            if let Some(t) = &tray {
                t.update(|t| t.autostart = actual);
            }
        });
    }
    {
        let shared = shared.clone();
        let settings = settings.clone();
        ui.on_strength_changed(move |v| {
            shared.set_atten_db(strength_to_db(v));
            let mut s = settings.lock().unwrap();
            s.strength = v;
            s.save();
        });
    }
    {
        let shared = shared.clone();
        let settings = settings.clone();
        ui.on_extra_cleanup_changed(move |on| {
            shared.extra_cleanup.store(on, Ordering::Relaxed);
            let mut s = settings.lock().unwrap();
            s.extra_cleanup = on;
            s.save();
        });
    }
    {
        let shared = shared.clone();
        let settings = settings.clone();
        ui.on_gate_changed(move |on| {
            shared.gate.store(on, Ordering::Relaxed);
            let mut s = settings.lock().unwrap();
            s.gate = on;
            s.save();
        });
    }
    {
        let shared = shared.clone();
        ui.on_play_cleaned_changed(move |on| shared.play_cleaned.store(on, Ordering::Relaxed));
    }
    let send = |cmd: fn() -> Command| {
        let tx = cmd_tx.clone();
        move || {
            let _ = tx.send(cmd());
        }
    };
    ui.on_record(send(|| Command::Record));
    ui.on_play(send(|| Command::Play));
    ui.on_monitor(send(|| Command::Monitor));
    ui.on_stop_test(send(|| Command::Stop));

    // Meters and test progress, ~30 fps. Meters fall off gently.
    let meter_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let shared = shared.clone();
        meter_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(33),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let (i, o) = shared.take_levels();
                ui.set_in_level(to_meter(i).max(ui.get_in_level() * 0.85));
                ui.set_out_level(to_meter(o).max(ui.get_out_level() * 0.85));
                let (mode, clip, pos) = shared.preview();
                ui.set_test_mode(match mode {
                    PreviewMode::Idle => TestMode::Idle,
                    PreviewMode::Recording => TestMode::Recording,
                    PreviewMode::Playing => TestMode::Playing,
                    PreviewMode::Monitoring => TestMode::Monitoring,
                });
                ui.set_clip_length(clip);
                ui.set_clip_pos(pos);
            },
        );
    }

    // With a tray, closing the window just hides it; quit from the tray menu.
    // Without one, closing the window quits as usual. Decided per close, as
    // the tray can come and go.
    {
        let ui_weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            if !ui_weak.upgrade().is_some_and(|ui| ui.get_has_tray()) {
                let _ = slint::quit_event_loop();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    // `--hidden` only makes sense with a tray to get the window back from.
    // A tray that may still be starting (e.g. at login) gets a grace period.
    if !has_arg(desktop_entry::HIDDEN_FLAG) || tray.is_none() {
        ui.show()?;
    } else if !tray_available {
        reveal_without_tray_later(&ui);
    }
    slint::run_event_loop_until_quit()?;

    if let Some(t) = tray {
        t.shutdown().wait();
    }

    let _ = cmd_tx.send(Command::Quit);
    let _ = audio_thread.join();
    Ok(())
}

/// Show the window if it is still hidden without a tray after [`TRAY_GRACE`],
/// so Esquie never ends up running with no way to reach it.
fn reveal_without_tray_later(ui: &AppWindow) {
    let ui_weak = ui.as_weak();
    slint::Timer::single_shot(TRAY_GRACE, move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        if !ui.get_has_tray() && !ui.window().is_visible() {
            let _ = ui.show();
        }
    });
}

fn apply_event(ui: &AppWindow, event: Event) {
    match event {
        Event::Mics { list, active } => {
            let labels: Vec<SharedString> =
                list.iter().map(|m| m.description.as_str().into()).collect();
            let names: Vec<SharedString> = list.iter().map(|m| m.name.as_str().into()).collect();
            let index = active
                .as_ref()
                .and_then(|a| list.iter().position(|m| &m.name == a))
                .map_or(-1, |i| i as i32);
            ui.set_mic_names(ModelRc::new(VecModel::from(names)));
            ui.set_mics(ModelRc::new(VecModel::from(labels)));
            ui.set_mic_index(index);
        }
        Event::Users(users) => ui.set_users(users.join(", ").into()),
        Event::Status(status) => {
            let (kind, message) = match status {
                Status::Connecting => (AudioStatus::Connecting, String::new()),
                Status::Disconnected(_) => (
                    AudioStatus::Disconnected,
                    "Waiting for the audio server (PipeWire). Esquie reconnects automatically."
                        .to_string(),
                ),
                Status::NoMicrophone => (AudioStatus::NoMicrophone, String::new()),
                Status::NoRoute(msg) => (
                    AudioStatus::NoRoute,
                    format!("{msg} Retrying automatically."),
                ),
                Status::Ready => (AudioStatus::Ready, String::new()),
                Status::Failed(msg) => (AudioStatus::Failed, msg),
            };
            ui.set_audio_status(kind);
            ui.set_status_message(message.into());
        }
        Event::PreviewError(msg) => ui.set_test_error(msg.unwrap_or_default().into()),
    }
}

/// Peak amplitude to a 0..1 meter value on a 60 dB scale.
fn to_meter(peak: f32) -> f32 {
    if peak <= 1e-6 {
        return 0.0;
    }
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}
