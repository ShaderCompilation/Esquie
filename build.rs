fn main() {
    // Fixed style so the app looks the same on every desktop (follows light/dark).
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    // ElementHandle needs metadata to locate controls. Keep it in a separate
    // generated module used only by the UI integration test.
    slint_build::compile_with_config("ui/app.slint", config.clone().with_debug_info(true))
        .expect("compile test Slint UI");
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::rename(out_dir.join("app.rs"), out_dir.join("test_app.rs"))
        .expect("preserve test Slint UI");
    slint_build::compile_with_config("ui/app.slint", config).expect("compile Slint UI");
}
