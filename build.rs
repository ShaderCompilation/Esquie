fn main() {
    // Fixed style so the app looks the same on every desktop (follows light/dark).
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("compile Slint UI");
}
