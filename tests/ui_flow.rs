//! Exercise the production Slint UI with software rendering, without desktop
//! services, an audio device, or modifications to the user's preferences.
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};

slint::include_modules!();

struct SoftwarePlatform(Rc<MinimalSoftwareWindow>);

impl Platform for SoftwarePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn click(ui: &AppWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn key(ui: &AppWindow, key: Key) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

fn capture(ui: &AppWindow, name: &str) -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
    // A real event loop processes follow-up layout invalidations after a view
    // changes. Do the same before accepting a headless snapshot: wrapped text
    // needs the new widths before its containing cards can settle their heights.
    for _ in 0..3 {
        slint::platform::update_timers_and_animations();
        ui.window().take_snapshot().expect("layout pass");
    }
    let pixels = ui.window().take_snapshot().expect("software snapshot");
    assert!(
        pixels
            .as_slice()
            .iter()
            .any(|pixel| pixel != &pixels.as_slice()[0]),
        "blank UI: {name}"
    );
    // Content must stay inside both horizontal gutters, even with long status
    // text, installation paths, and expanded controls. The scrollbar is outside.
    let scale = ui.window().scale_factor();
    let width = pixels.width() as usize;
    let inset = (16. * scale) as usize;
    let background = pixels.as_slice()[0];
    for y in (90. * scale) as usize..pixels.height() as usize {
        for x in [inset, width - inset] {
            assert_eq!(
                pixels.as_slice()[y * width + x],
                background,
                "horizontal overflow: {name} at {x},{y}"
            );
        }
    }
    if let Some(directory) = std::env::var_os("ESQUIE_UI_OUTPUT_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let size = resvg::tiny_skia::IntSize::from_wh(pixels.width(), pixels.height()).unwrap();
        resvg::tiny_skia::Pixmap::from_vec(pixels.as_bytes().to_vec(), size)
            .unwrap()
            .save_png(directory.join(format!("{name}.png")))
            .unwrap();
    }
    pixels
}

#[test]
fn microphone_sound_check_and_settings_flow() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(SoftwarePlatform(window))).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.set_mics(ModelRc::new(VecModel::from(vec![
        "USB microphone".into(),
        "Built-in microphone".into(),
    ])));
    ui.set_mic_index(0);
    ui.set_audio_status(AudioStatus::Ready);
    ui.set_in_level(0.4);
    ui.set_out_level(0.2);
    ui.set_live_processing_active(true);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_install_directory("/home/user/.local/bin".into());
    ui.set_installation_info("AppImage: /home/user/Downloads/Esquie.AppImage\n0 Esquie launchers, 0 external/system launchers.".into());
    ui.set_can_copy(true);
    ui.global::<Palette>()
        .set_color_scheme(slint::language::ColorScheme::Dark);
    ui.show().unwrap();
    ui.set_window_visible(true);
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = polls.clone();
    ui.on_refresh_preview(move || {
        observed.fetch_add(1, Ordering::Relaxed);
    });
    let assert_polling = |active| {
        slint::platform::update_timers_and_animations();
        assert_eq!(ui.get_meters_active(), active);
        let before = polls.load(Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(45));
        slint::platform::update_timers_and_animations();
        assert_eq!(polls.load(Ordering::Relaxed) > before, active);
    };
    assert_polling(true); // The workspace always includes live meters.
    ui.set_setup_open(true);
    assert_polling(true); // The microphone-selection step also shows input.
    ui.set_setup_step(2);
    assert_polling(false); // The final setup instructions contain no meters.
    ui.set_test_open(true);
    assert_polling(true);
    ui.set_settings_open(true);
    assert_polling(false);
    ui.set_settings_open(false);
    assert_polling(true);
    ui.set_window_visible(false);
    assert_polling(false);
    ui.set_window_visible(true);
    assert_polling(true);
    ui.window().set_minimized(true);
    assert_polling(false);
    ui.window().set_minimized(false);
    assert_polling(true);
    ui.set_test_open(false);
    ui.set_setup_open(false);
    assert_polling(true);
    ui.on_refresh_preview(|| {});
    ui.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    ui.window().set_size(LogicalSize::new(880., 640.));
    capture(&ui, "01-workspace");
    // Both preview actions are directly reachable without navigating away.
    let records = Arc::new(AtomicUsize::new(0));
    let count = records.clone();
    ui.on_record(move || {
        count.fetch_add(1, Ordering::Relaxed);
    });
    let monitors = Arc::new(AtomicUsize::new(0));
    let count = monitors.clone();
    ui.on_monitor(move || {
        count.fetch_add(1, Ordering::Relaxed);
    });
    assert_eq!(records.load(Ordering::Relaxed), 0);
    assert_eq!(monitors.load(Ordering::Relaxed), 0);
    click(&ui, 550., 435.);
    click(&ui, 760., 435.);
    assert_eq!(records.load(Ordering::Relaxed), 1);
    assert_eq!(monitors.load(Ordering::Relaxed), 1);
    assert!(!ui.get_test_open());
    assert!(!ui.get_settings_open());
    ui.window().set_size(LogicalSize::new(540., 780.));
    capture(&ui, "01-microphone");

    // Header action works with a pointer, then keyboard activation returns.
    click(&ui, 460., 42.);
    assert!(ui.get_settings_open());
    capture(&ui, "02-settings-top");
    let before = capture(&ui, "02-settings-before-scroll");
    key(&ui, Key::End);
    let bottom = capture(&ui, "02-settings-bottom");
    assert_ne!(before.as_bytes(), bottom.as_bytes(), "Settings must scroll");
    // The last action is fully reachable, followed by the page's bottom padding.
    click(&ui, 240., 780. - 50.);
    assert!(
        ui.get_setup_open(),
        "last Settings action must be reachable"
    );
    assert!(!ui.get_settings_open());
    ui.set_setup_open(false);
    click(&ui, 460., 38.);
    key(&ui, Key::Return);
    assert!(!ui.get_settings_open());

    let strengths = Arc::new(Mutex::new(Vec::new()));
    let changed = strengths.clone();
    ui.on_strength_changed(move |value| {
        changed.lock().unwrap().push(value);
    });
    click(&ui, 260., 410.);
    assert_eq!(ui.get_strength(), 65.);
    assert_eq!(*strengths.lock().unwrap(), [65.]);
    click(&ui, 420., 410.);
    assert_eq!(ui.get_strength(), 100.);

    // A round trip through Settings preserves the microphone scroll position.
    ui.window().set_size(LogicalSize::new(480., 480.));
    ui.set_fine_tune_open(true);
    capture(&ui, "02-microphone-expanded");
    click(&ui, 400., 38.);
    capture(&ui, "02-settings-before-return");
    key(&ui, Key::Escape);
    capture(&ui, "02-microphone-before-scroll");
    key(&ui, Key::End);
    let scrolled = capture(&ui, "02-microphone-scrolled");
    click(&ui, 400., 38.);
    capture(&ui, "02-settings-scrolled-entry");
    key(&ui, Key::Escape);
    let restored = capture(&ui, "02-microphone-restored");
    // Ignore the scrollbar's hover animation; compare the actual page content.
    let width = scrolled.width() as usize;
    for y in 84..scrolled.height() as usize {
        let row = y * width;
        assert!(
            scrolled.as_slice()[row + 24..row + width - 24]
                == restored.as_slice()[row + 24..row + width - 24],
            "returning must preserve the scroll position (row {y})"
        );
    }
    ui.set_fine_tune_open(false);
    key(&ui, Key::Home);

    // Exercise the default size, narrow windows, and both sides of the breakpoint.
    for width in [480., 540., 759., 760., 880.] {
        ui.window().set_size(LogicalSize::new(width, 640.));
        ui.set_clip_length(0.);
        ui.set_test_mode(TestMode::Idle);
        capture(&ui, &format!("03-test-{width}"));
        ui.set_test_mode(TestMode::Recording);
        ui.set_clip_length(3.);
        key(&ui, Key::End);
        let recording = capture(&ui, &format!("04-recording-{width}"));
        // The recording progress must have room below it inside the card,
        // rather than touching or overflowing the card's lower edge.
        let stride = recording.width() as usize;
        let meter_x = if width >= 760. {
            (width * 0.6) as usize
        } else {
            50
        };
        let progress_end = (90..recording.height() as usize)
            .rev()
            .find(|y| {
                let pixel = recording.as_slice()[y * stride + meter_x];
                pixel.r > 200 && pixel.g < 160 && pixel.b < 160
            })
            .expect("recording progress is visible");
        let card = recording.as_slice()[(progress_end - 10) * stride + meter_x];
        let padding = (progress_end + 1..recording.height() as usize)
            .take_while(|y| recording.as_slice()[y * stride + meter_x] == card)
            .count();
        assert!(
            padding >= 12,
            "recording card must contain its progress and bottom padding"
        );
        key(&ui, Key::Home);
        ui.set_test_mode(TestMode::Idle);
        ui.set_clip_length(10.);
        capture(&ui, &format!("05-comparison-{width}"));
        ui.set_test_mode(TestMode::Playing);
        ui.set_clip_pos(4.);
        capture(&ui, &format!("06-playing-{width}"));
        ui.set_test_mode(TestMode::Monitoring);
        capture(&ui, &format!("07-listening-{width}"));
        ui.set_test_mode(TestMode::Idle);
        ui.set_fine_tune_open(true);
        capture(&ui, &format!("08-fine-tuning-{width}"));
        key(&ui, Key::End);
        let bottom = capture(&ui, &format!("08-fine-tuning-bottom-{width}"));
        let row = (bottom.height() as usize - 12) * bottom.width() as usize;
        let background = bottom.as_slice()[0];
        assert!(
            bottom.as_slice()[row + 24..row + width as usize - 24]
                .iter()
                .all(|pixel| *pixel == background),
            "bottom padding must be reachable"
        );
        key(&ui, Key::Home);
        ui.set_fine_tune_open(false);
        ui.set_test_open(false);
    }

    // Escape stops tests without leaving the unified workspace.
    let stops = Arc::new(AtomicUsize::new(0));
    let count = stops.clone();
    ui.on_stop_test(move || {
        count.fetch_add(1, Ordering::Relaxed);
    });
    ui.set_test_mode(TestMode::Recording);
    key(&ui, Key::Escape);
    assert!(!ui.get_settings_open());
    assert!(!ui.get_test_open());
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    // The meter timer may still report Idle immediately after Record was sent.
    ui.set_test_mode(TestMode::Idle);
    capture(&ui, "09-pending-record");
    key(&ui, Key::Escape);
    assert_eq!(
        stops.load(Ordering::Relaxed),
        2,
        "navigation cancels pending audio commands too"
    );
    ui.set_test_mode(TestMode::Monitoring);
    ui.set_settings_open(true);
    capture(&ui, "09-settings-from-test");
    assert!(stops.load(Ordering::Relaxed) >= 3);
    ui.set_settings_open(false);
    ui.set_test_open(false);
    ui.set_test_mode(TestMode::Idle);

    // Pointer and keyboard holds release on mouse-up, key-up, and navigation.
    ui.window().set_size(LogicalSize::new(880., 640.));
    ui.set_test_mode(TestMode::Monitoring);
    capture(&ui, "09-hold-ready");
    let holds = Arc::new(Mutex::new(Vec::new()));
    let events = holds.clone();
    ui.on_bypass_hold(move |held| {
        events.lock().unwrap().push(held);
    });
    click(&ui, 650., 435.);
    assert_eq!(*holds.lock().unwrap(), [true, false]);
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Space.into(),
    });
    assert_eq!(holds.lock().unwrap().last(), Some(&true));
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Space.into(),
    });
    assert_eq!(holds.lock().unwrap().last(), Some(&false));
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Space.into(),
    });
    ui.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(false));
    capture(&ui, "09-hold-window-inactive");
    assert_eq!(holds.lock().unwrap().last(), Some(&false));
    ui.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Space.into(),
    });
    click(&ui, 800., 38.);
    assert!(ui.get_settings_open());
    assert_eq!(holds.lock().unwrap().last(), Some(&false));
    ui.set_test_mode(TestMode::Idle);
    ui.set_settings_open(false);
    ui.set_test_mode(TestMode::Monitoring);
    capture(&ui, "09-hold-before-hide");
    click(&ui, 650., 435.);
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Space.into(),
    });
    let before = stops.load(Ordering::Relaxed);
    ui.set_window_visible(false);
    capture(&ui, "09-hidden");
    assert!(stops.load(Ordering::Relaxed) > before);
    assert_eq!(holds.lock().unwrap().last(), Some(&false));
    ui.set_window_visible(true);
    click(&ui, 650., 435.);
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Space.into(),
    });
    let before = stops.load(Ordering::Relaxed);
    ui.window().set_minimized(true);
    capture(&ui, "09-minimized");
    assert!(stops.load(Ordering::Relaxed) > before);
    assert_eq!(holds.lock().unwrap().last(), Some(&false));
    ui.window().set_minimized(false);
    ui.set_test_mode(TestMode::Idle);

    ui.set_settings_open(false);
    ui.window().set_size(LogicalSize::new(480., 600.));
    ui.set_setup_open(true);
    ui.set_setup_step(0);
    capture(&ui, "10-setup-microphone");
    click(&ui, 240., 558.);
    assert!(ui.get_test_open(), "setup opens optional sound check");
    capture(&ui, "10-setup-sound-check");
    let before = stops.load(Ordering::Relaxed);
    click(&ui, 240., 558.);
    assert!(stops.load(Ordering::Relaxed) > before);
    assert!(!ui.get_test_open());
    assert_eq!(ui.get_setup_step(), 2);
    capture(&ui, "11-setup-before-back");
    click(&ui, 210., 38.);
    assert_eq!(ui.get_setup_step(), 1);
    assert!(ui.get_test_open());
    key(&ui, Key::Escape);
    assert_eq!(ui.get_setup_step(), 0);
    assert!(!ui.get_test_open());
    capture(&ui, "10-setup-return-microphone");
    click(&ui, 240., 506.);
    assert_eq!(ui.get_setup_step(), 2);
    key(&ui, Key::Escape);
    assert_eq!(ui.get_setup_step(), 0);
    capture(&ui, "10-setup-skip-return");
    click(&ui, 240., 558.);
    capture(&ui, "10-setup-test-again");
    click(&ui, 350., 38.);
    assert!(ui.get_settings_open());
    key(&ui, Key::Escape);
    assert!(ui.get_test_open());
    assert_eq!(ui.get_setup_step(), 1);
    capture(&ui, "10-setup-settings-return");
    click(&ui, 240., 558.);
    capture(&ui, "11-setup-apps");
    for (width, height) in [(880., 640.), (480., 480.)] {
        ui.window().set_size(LogicalSize::new(width, height));
        for step in 0..3 {
            ui.set_setup_step(step);
            ui.set_test_open(step == 1);
            capture(&ui, &format!("setup-{width}-{height}-step-{step}"));
            if step == 1 {
                ui.set_fine_tune_open(true);
                capture(&ui, &format!("setup-{width}-{height}-expanded"));
                key(&ui, Key::End);
                capture(&ui, &format!("setup-{width}-{height}-expanded-bottom"));
                key(&ui, Key::Home);
                ui.set_fine_tune_open(false);
                ui.set_test_mode(TestMode::Recording);
                capture(&ui, &format!("setup-{width}-{height}-recording"));
                ui.set_test_mode(TestMode::Playing);
                capture(&ui, &format!("setup-{width}-{height}-playing"));
                ui.set_test_mode(TestMode::Idle);
            }
        }
    }
    ui.set_setup_step(0);
    ui.set_audio_status(AudioStatus::NoMicrophone);
    capture(&ui, "setup-small-no-microphone");
    let microphones = ui.get_mics();
    ui.set_mics(ModelRc::new(VecModel::from(vec![
        "A very long microphone name with manufacturer, device model, and audio profile".into(),
    ])));
    capture(&ui, "setup-small-long-microphone");
    ui.set_mics(microphones);
    ui.set_audio_status(AudioStatus::Ready);
    ui.set_setup_step(2);
    ui.window().set_size(LogicalSize::new(480., 600.));
    capture(&ui, "11-setup-finish");
    let finishes = Arc::new(AtomicUsize::new(0));
    let finished = finishes.clone();
    ui.on_finish_setup(move || {
        finished.fetch_add(1, Ordering::Relaxed);
    });
    click(&ui, 240., 558.);
    assert!(!ui.get_setup_open());
    assert_eq!(finishes.load(Ordering::Relaxed), 1);
    for (name, status) in [
        ("starting", AudioStatus::Connecting),
        ("reconnecting", AudioStatus::Disconnected),
        ("no-microphone", AudioStatus::NoMicrophone),
        ("no-route", AudioStatus::NoRoute),
        ("failed", AudioStatus::Failed),
    ] {
        ui.set_audio_status(status);
        capture(&ui, &format!("12-{name}"));
    }
    ui.set_audio_status(AudioStatus::Ready);
    ui.set_mics(ModelRc::new(VecModel::from(vec![
        "USB microphone with a very long device and manufacturer name (Analog Stereo)".into(),
    ])));
    ui.set_mic_index(0);
    ui.window().set_size(LogicalSize::new(880., 640.));
    capture(&ui, "13-long-microphone-wide");
    ui.window().set_size(LogicalSize::new(480., 600.));
    ui.set_users("A calling app with a long name, and another recording app".into());
    capture(&ui, "13-in-use");
    ui.set_enabled(false);
    capture(&ui, "14-noise-removal-off");

    ui.set_users("".into());
    ui.set_enabled(true);
    ui.set_install_prompt(true);
    ui.set_settings_open(true);
    ui.set_install_collision(true);
    ui.set_collision_path("/home/user/A very long installation directory with spaces/esquie".into());
    ui.set_can_switch(true);
    ui.set_crashed_last_run(true);
    capture(&ui, "15-settings-expanded");
    key(&ui, Key::End);
    let settings_bottom = capture(&ui, "15-settings-expanded-bottom");
    let stride = settings_bottom.width() as usize;
    let background = settings_bottom.as_slice()[0];
    let mut gaps = 0;
    let mut run = 0;
    for y in 100..settings_bottom.height() as usize - 50 {
        if settings_bottom.as_slice()[y * stride + stride / 2] == background {
            run += 1;
            if run == 10 {
                gaps += 1;
            }
        } else {
            run = 0;
        }
    }
    assert!(
        gaps >= 2,
        "expanded settings cards and their buttons must not overlap"
    );
    key(&ui, Key::Escape);
    key(&ui, Key::Home);
    assert!(!ui.get_settings_open());
    capture(&ui, "16-installation-suggestion");
    ui.set_install_prompt(false);
    // Switch to light through the in-app theme toggle (right of the header's
    // navigation button) rather than forcing the palette from Rust, so the
    // switch itself stays covered.
    click(&ui, 438., 38.);
    assert_eq!(
        ui.global::<Palette>().get_color_scheme(),
        slint::language::ColorScheme::Light
    );
    // Headless snapshots run back-to-back, so the 120ms button-background
    // animation would otherwise still show the old theme's colors. Give it
    // real time to finish before capturing the settled light UI.
    std::thread::sleep(std::time::Duration::from_millis(200));
    capture(&ui, "17-light-microphone");
    ui.window()
        .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.5 });
    ui.window().set_size(LogicalSize::new(480., 480.));
    ui.set_test_open(true);
    ui.set_fine_tune_open(true);
    assert!(!ui.get_settings_open());
    capture(&ui, "18-scaled-short-window");
    key(&ui, Key::End);
    capture(&ui, "18-scaled-short-bottom");

    // No app names are needed for live output (recording and monitoring also
    // supply demand). Silence is distinct from suspended processing.
    ui.window()
        .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
    ui.window().set_size(LogicalSize::new(880., 800.));
    ui.set_fine_tune_open(false);
    ui.set_users("".into());
    ui.set_out_level(0.);
    ui.set_test_mode(TestMode::Idle);
    key(&ui, Key::Home);
    let silent = capture(&ui, "19-output-active-silence");
    ui.set_live_processing_active(false);
    let idle = capture(&ui, "19-output-waiting");
    assert_ne!(silent.as_bytes(), idle.as_bytes());
    ui.set_test_mode(TestMode::Playing);
    capture(&ui, "19-output-playback-waiting");
    ui.set_test_mode(TestMode::Monitoring);
    ui.set_live_processing_active(true);
    ui.set_out_level(0.5);
    capture(&ui, "19-output-monitoring");
    ui.set_enabled(false);
    capture(&ui, "19-output-passthrough");
    ui.set_audio_status(AudioStatus::Disconnected);
    capture(&ui, "19-output-unavailable");
    ui.set_audio_status(AudioStatus::Ready);
    ui.set_live_processing_active(false);
    ui.set_test_mode(TestMode::Idle);
    ui.window().set_size(LogicalSize::new(480., 800.));
    key(&ui, Key::End);
    capture(&ui, "19-output-waiting-narrow");
}
