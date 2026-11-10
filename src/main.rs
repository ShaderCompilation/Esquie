mod audio;
mod settings;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use audio::{Command, Event, Shared};
use settings::{strength_to_db, Settings};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

const READY: &str = "Pick \"Esquie (noise removed)\" as the microphone in your apps.";

fn main() -> anyhow::Result<()> {
    let settings = Arc::new(Mutex::new(Settings::load()));
    let initial = settings.lock().unwrap().clone();
    let shared = Arc::new(Shared::new(initial.enabled, strength_to_db(initial.strength)));

    let ui = AppWindow::new()?;
    ui.set_enabled(initial.enabled);
    ui.set_strength(initial.strength);

    // Audio thread (PipeWire + DeepFilterNet).
    let (cmd_tx, cmd_rx) = pipewire::channel::channel::<Command>();
    let audio_thread = {
        let shared = shared.clone();
        let ui_weak = ui.as_weak();
        let mic = initial.mic.clone();
        std::thread::Builder::new().name("esquie-audio".into()).spawn(move || {
            let notify_weak = ui_weak.clone();
            let notify = move |event: Event| {
                let _ = notify_weak.upgrade_in_event_loop(move |ui| apply_event(&ui, event));
            };
            if let Err(e) = audio::run(shared, mic, cmd_rx, notify) {
                let msg = format!("Audio engine failed: {e:#}");
                eprintln!("{msg}");
                let _ = ui_weak.upgrade_in_event_loop(move |ui| ui.set_status(msg.into()));
            }
        })?
    };

    {
        let tx = cmd_tx.clone();
        let settings = settings.clone();
        let ui_weak = ui.as_weak();
        ui.on_mic_selected(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            if let Some(name) = ui.get_mic_names().row_data(i as usize).map(|n| n.to_string()) {
                settings.lock().unwrap().mic = Some(name.clone());
                settings.lock().unwrap().save();
                let _ = tx.send(Command::SelectMic(name));
            }
        });
    }
    {
        let shared = shared.clone();
        let settings = settings.clone();
        ui.on_enabled_changed(move |on| {
            shared.enabled.store(on, Ordering::Relaxed);
            let mut s = settings.lock().unwrap();
            s.enabled = on;
            s.save();
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

    // Level meters, ~30 fps with a gentle fall-off.
    let meter_timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        let shared = shared.clone();
        meter_timer.start(slint::TimerMode::Repeated, Duration::from_millis(33), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (i, o) = shared.take_levels();
            ui.set_in_level(to_meter(i).max(ui.get_in_level() * 0.85));
            ui.set_out_level(to_meter(o).max(ui.get_out_level() * 0.85));
        });
    }

    ui.run()?;

    let _ = cmd_tx.send(Command::Quit);
    let _ = audio_thread.join();
    Ok(())
}

fn apply_event(ui: &AppWindow, event: Event) {
    match event {
        Event::Mics { list, active } => {
            let labels: Vec<SharedString> = list.iter().map(|m| m.description.as_str().into()).collect();
            let index = active
                .as_ref()
                .and_then(|a| list.iter().position(|m| &m.name == a))
                .map_or(-1, |i| i as i32);
            let names: Vec<SharedString> = list.iter().map(|m| m.name.as_str().into()).collect();
            ui.set_mic_names(ModelRc::new(VecModel::from(names)));
            ui.set_mics(ModelRc::new(VecModel::from(labels)));
            ui.set_mic_index(index);
            ui.set_status(if list.is_empty() { "No microphone found." } else { READY }.into());
        }
        Event::Error(msg) => ui.set_status(msg.into()),
    }
}

/// Peak amplitude to a 0..1 meter value on a 60 dB scale.
fn to_meter(peak: f32) -> f32 {
    if peak <= 1e-6 {
        return 0.0;
    }
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}
