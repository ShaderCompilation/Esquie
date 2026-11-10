//! "Start on login" via an XDG autostart entry (~/.config/autostart/esquie.desktop).

use std::path::PathBuf;

/// Passed by the autostart entry so Esquie starts quietly in the tray.
pub const HIDDEN_FLAG: &str = "--hidden";

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("esquie.desktop"))
}

/// The entry file is the source of truth, so removing it from the desktop's
/// own startup settings is reflected here too.
pub fn is_enabled() -> bool {
    path().is_some_and(|p| p.exists())
}

pub fn set_enabled(on: bool) -> std::io::Result<()> {
    let Some(p) = path() else { return Ok(()) };
    if !on {
        return match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    // Desktop entry quoting: wrap in quotes, escape `"`, `` ` ``, `$` and `\`.
    let mut quoted = String::from("\"");
    for c in exe.to_string_lossy().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Esquie\n\
         Comment=Noise removal for your microphone\n\
         Exec={quoted} {HIDDEN_FLAG}\n\
         Icon=audio-input-microphone\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n"
    );
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, entry)
}
