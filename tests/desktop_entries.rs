use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EXE: &str = env!("CARGO_BIN_EXE_esquie");

/// A scratch directory standing in for the user's config and an AppImage location.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("esquie-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn autostart(&self) -> PathBuf {
        self.0.join("config/autostart/esquie.desktop")
    }

    /// Runs without a display, D-Bus, or PipeWire, so these options must not touch them.
    fn run(&self, arg: &str, env: &[(&str, &Path)]) -> Output {
        let mut cmd = Command::new(EXE);
        cmd.arg(arg)
            .env_clear()
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_RUNTIME_DIR", "/nonexistent/esquie-desktop-test")
            .env("PIPEWIRE_REMOTE", "esquie-desktop-test-missing")
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/esquie-desktop-test")
            .env("SLINT_BACKEND", "invalid-backend-for-desktop-test");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let output = cmd.output().expect("run esquie");
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        output
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn exec_of(entry: &str) -> &str {
    entry.lines().find_map(|l| l.strip_prefix("Exec=")).expect("Exec line")
}

#[test]
fn launcher_entry_targets_this_executable() {
    let scratch = Scratch::new("launcher");
    let output = scratch.run("--print-launcher-entry", &[]);
    let entry = String::from_utf8(output.stdout).unwrap();
    let exe = std::fs::canonicalize(EXE).unwrap();
    assert!(entry.starts_with("[Desktop Entry]\n"), "{entry}");
    assert_eq!(exec_of(&entry), exe.to_str().unwrap(), "{entry}");
    assert!(!scratch.autostart().exists());
}

#[test]
fn refresh_follows_moved_appimage() {
    let scratch = Scratch::new("refresh");
    let exe = std::fs::canonicalize(EXE).unwrap();
    // Pretend the test binary's directory is the AppImage mount.
    let appdir = exe.parent().unwrap();
    let image = scratch.0.join("Moved Apps/Esquie 100%.AppImage");
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    std::fs::write(&image, b"").unwrap();
    let appimage_env = [("APPIMAGE", image.as_path()), ("APPDIR", appdir)];

    // Nothing to refresh while autostart is off.
    scratch.run("--refresh-autostart", &appimage_env);
    assert!(!scratch.autostart().exists());

    let stale = "[Desktop Entry]\nType=Application\nName=Esquie\n\
                 Exec=/tmp/.mount_EsquieOld/usr/bin/esquie --hidden\nX-GNOME-Autostart-Delay=2\n";
    std::fs::create_dir_all(scratch.autostart().parent().unwrap()).unwrap();
    std::fs::write(scratch.autostart(), stale).unwrap();
    scratch.run("--refresh-autostart", &appimage_env);
    let entry = std::fs::read_to_string(scratch.autostart()).unwrap();
    assert_eq!(
        exec_of(&entry),
        // A `%` in the program path needs the sh wrapper (see desktop_entry.rs).
        format!(
            r#"sh -c "exec \\"\\$0\\" \\"\\$@\\"" "{}" --hidden"#,
            image.to_str().unwrap().replace('%', "%%")
        ),
        "{entry}"
    );
    assert!(entry.contains("X-GNOME-Autostart-Delay=2\n"), "{entry}");
    assert!(!entry.contains(".mount_"), "{entry}");

    // Without the AppImage runtime, the ordinary executable path is used.
    scratch.run("--refresh-autostart", &[]);
    let entry = std::fs::read_to_string(scratch.autostart()).unwrap();
    assert_eq!(exec_of(&entry), format!("{} --hidden", exe.to_str().unwrap()), "{entry}");

    // An entry the user disabled is left alone.
    let disabled = format!("{entry}Hidden=true\n");
    std::fs::write(scratch.autostart(), &disabled).unwrap();
    scratch.run("--refresh-autostart", &appimage_env);
    assert_eq!(std::fs::read_to_string(scratch.autostart()).unwrap(), disabled);
}
