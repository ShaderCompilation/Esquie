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
    slint::Timer::single_shot(std::time::Duration::from_millis(250), move || {
        let ui = weak.upgrade().expect("window remains alive");
        let pixels = ui
            .window()
            .take_snapshot()
            .expect("software-rendered snapshot");
        assert!(pixels.width() > 0 && pixels.height() > 0);
        let first = &pixels.as_bytes()[..4];
        assert!(
            pixels
                .as_bytes()
                .chunks_exact(4)
                .any(|pixel| pixel != first),
            "blank UI"
        );
        slint::quit_event_loop().expect("quit smoke test");
    });
    slint::run_event_loop()?;
    println!("Production UI rendered with Winit software rendering");
    Ok(())
}
