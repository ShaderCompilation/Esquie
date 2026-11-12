use std::process::Command;

#[test]
fn version_without_desktop_or_audio() {
    for args in [vec!["--version"], vec!["--hidden", "--version"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_esquie"))
            .args(args)
            .env_clear()
            // Any accidental settings or service access must not reach the host.
            .env("HOME", "/nonexistent/esquie-version-test")
            .env("XDG_CONFIG_HOME", "/nonexistent/esquie-version-test")
            .env("XDG_RUNTIME_DIR", "/nonexistent/esquie-version-test")
            .env("PIPEWIRE_REMOTE", "esquie-version-test-missing")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/esquie-version-test",
            )
            .env("SLINT_BACKEND", "invalid-backend-for-version-test")
            .output()
            .expect("run esquie --version");
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::str::from_utf8(&output.stdout).unwrap(),
            format!("{} {}\n", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        );
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}
