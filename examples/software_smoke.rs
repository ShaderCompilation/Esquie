//! Render the production UI without starting the audio engine or tray.
//! Run through ci/check.sh for a virtual X11 display with GLX disabled.
use slint::ComponentHandle;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    // Explicit selection prevents a successful GPU renderer from hiding a
    // broken software renderer on development machines with working graphics.
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()?;
    let ui = AppWindow::new()?;
    ui.show()?;
    let weak = ui.as_weak();
    let stage = std::cell::Cell::new(0);
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(250),
        move || {
            let ui = weak.upgrade().expect("window remains alive");
            check_snapshot(&ui, stage.get());
            show_next_view(&ui, stage.get());
            stage.set(stage.get() + 1);
        },
    );
    slint::run_event_loop()?;
    println!(
        "Microphone, installation prompt, and Settings rendered with Winit software rendering"
    );
    Ok(())
}

fn check_snapshot(ui: &AppWindow, stage: i32) {
    let pixels = ui
        .window()
        .take_snapshot()
        .expect("software-rendered snapshot");
    assert!(pixels.width() > 0 && pixels.height() > 0);
    let first = &pixels.as_bytes()[..4];
    assert!(
        pixels
            .as_bytes()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel != first),
        "blank UI"
    );
    if let Some(directory) =
        std::env::var_os("ESQUIE_SMOKE_OUTPUT_DIR").or_else(|| std::env::args_os().nth(1))
    {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let size = resvg::tiny_skia::IntSize::from_wh(pixels.width(), pixels.height()).unwrap();
        resvg::tiny_skia::Pixmap::from_vec(pixels.as_bytes().to_vec(), size)
            .unwrap()
            .save_png(directory.join(format!("ui-{stage}.png")))
            .unwrap();
    }
}

fn show_next_view(ui: &AppWindow, stage: i32) {
    match stage {
        0 => ui.set_install_prompt(true),
        1 => {
            ui.set_install_prompt(false);
            ui.set_settings_open(true);
            ui.set_can_copy(true);
            ui.set_can_switch(true);
            ui.set_installation_info("AppImage: /home/user/Downloads/Esquie.AppImage\n0 Esquie launchers, 0 external launchers.".into());
            ui.set_install_directory("/home/user/.local/bin".into());
            ui.set_app_version(env!("CARGO_PKG_VERSION").into());
            ui.set_automatic_checks(true);
        }
        2 => slint::quit_event_loop().expect("quit smoke test"),
        _ => unreachable!(),
    }
}
