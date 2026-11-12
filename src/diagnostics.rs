//! Diagnostics export: assemble one plain-text bundle on request.
//!
//! The user explicitly picks the destination via a native save-file dialog;
//! nothing is uploaded or sent anywhere. The bundle is plain text for
//! readability and attachments to issue trackers.

use std::path::Path;

use anyhow::Result;

use crate::AppWindow;
use crate::logging;
use slint::ComponentHandle;

/// Per log file included in the bundle, to keep the report a sane size.
const MAX_LOG_FILE_BYTES: usize = 128 * 1024;

/// Wire the Diagnostics card: remember whether the last run crashed, and
/// export a bundle when the button is pressed. Bundle assembly and writing
/// run off the UI thread, following the installation callbacks' pattern.
pub fn setup(ui: &AppWindow, crashed_last_run: bool) {
    ui.set_crashed_last_run(crashed_last_run);
    let weak = ui.as_weak();
    ui.on_export_diagnostics(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_diagnostics_busy() {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export diagnostics")
            .set_file_name(default_file_name())
            .save_file()
        else {
            return; // cancelled; not an error
        };
        ui.set_diagnostics_busy(true);
        let done_weak = weak.clone();
        let spawn = std::thread::Builder::new()
            .name("esquie-diagnostics".into())
            .spawn(move || {
                let result = export_to(&path);
                let _ = done_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_diagnostics_busy(false);
                    let message = match result {
                        Ok(bytes) => {
                            format!("Diagnostics written to {} ({bytes} bytes).", path.display())
                        }
                        Err(e) => format!("Could not write diagnostics: {e:#}"),
                    };
                    ui.set_diagnostics_message(message.into());
                });
            });
        if let Err(e) = spawn
            && let Some(ui) = weak.upgrade()
        {
            ui.set_diagnostics_busy(false);
            ui.set_diagnostics_message(format!("Could not start export: {e}").into());
        }
    });
}

/// Suggested file name, date-stamped so repeat exports don't collide.
fn default_file_name() -> String {
    let days = unix_seconds() / 86_400;
    format!("esquie-diagnostics-{days}.txt")
}

/// Write the assembled bundle to `path`; returns the number of bytes written.
pub fn export_to(path: &Path) -> Result<usize> {
    let bundle = collect();
    std::fs::write(path, &bundle)?;
    Ok(bundle.len())
}

/// Gather every section of the bundle into one String.
pub fn collect() -> String {
    assemble(
        env!("CARGO_PKG_VERSION"),
        &system_info(),
        &desktop_info(),
        &pipewire_summary(),
        &logging::last_crash().unwrap_or_else(|| "none".into()),
        &logging::ring_contents(),
        &log_files(),
    )
}

/// Lay out the bundle sections. Separate from [`collect`] so tests can check
/// assembly without touching the real machine.
fn assemble(
    version: &str,
    system: &str,
    desktop: &str,
    pipewire: &str,
    crash: &str,
    ring: &str,
    logs: &str,
) -> String {
    let section = |title: &str, body: &str| {
        let body = body.trim();
        let body = if body.is_empty() { "(empty)" } else { body };
        format!("\n== {title} ==\n{body}\n")
    };
    format!(
        "Esquie diagnostics\napp version: {version}\ngenerated: unix={}\n{}\n{}\n{}\n{}\n{}\n{}",
        unix_seconds(),
        section("System", system),
        section("Desktop session", desktop),
        section("PipeWire", pipewire),
        section("Previous crash", crash),
        section("Recent log entries (ring buffer)", ring),
        section("Log files", logs),
    )
}

fn system_info() -> String {
    command_output("uname", &["-a"])
}

fn desktop_info() -> String {
    let env = |name: &str| std::env::var(name).map(|v| format!("{name}={v}")).ok();
    let display = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else if std::env::var_os("DISPLAY").is_some() {
        "x11"
    } else {
        "none"
    };
    [
        env("XDG_CURRENT_DESKTOP"),
        env("XDG_SESSION_TYPE"),
        env("DESKTOP_SESSION"),
    ]
    .into_iter()
    .flatten()
    .chain([format!("display-server={display}")])
    .collect::<Vec<_>>()
    .join("\n")
}

/// Server info via `pw-cli info 0` (version, name, cookie); empty when the
/// tool is missing or PipeWire is down, which is itself useful information.
fn pipewire_summary() -> String {
    command_output("pw-cli", &["info", "0"])
}

/// Contents of every log file under the state directory, oldest first,
/// each capped at [`MAX_LOG_FILE_BYTES`].
fn log_files() -> String {
    let Some(dir) = logging::log_dir() else {
        return "(no log directory)".into();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return format!("(could not read {})", dir.display());
    };
    let mut files: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort(); // rotated names are date-stamped, so lexical == chronological
    if files.is_empty() {
        return "(no log files)".into();
    }
    files
        .iter()
        .map(|path| {
            let text =
                std::fs::read_to_string(path).unwrap_or_else(|e| format!("(unreadable: {e})"));
            let (text, note) = if text.len() > MAX_LOG_FILE_BYTES {
                let cut = floor_char_boundary(&text, MAX_LOG_FILE_BYTES);
                (
                    &text[..cut],
                    format!("\n(truncated to {MAX_LOG_FILE_BYTES} bytes)"),
                )
            } else {
                (&text[..], String::new())
            };
            format!("-- {} --\n{}{}", path.display(), text.trim_end(), note)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `str::floor_char_boundary` is still unstable; a safe stand-in that never
/// splits inside a multi-byte character.
fn floor_char_boundary(s: &str, up_to: usize) -> usize {
    if up_to >= s.len() {
        return s.len();
    }
    let mut i = up_to;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn command_output(program: &str, args: &[&str]) -> String {
    match std::process::Command::new(program).args(args).output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Err(e) => format!("({program} not available: {e})"),
    }
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_contains_all_sections() {
        let bundle = assemble(
            "9.9.9",
            "Linux test 6.1",
            "display-server=x11",
            "pw-cli server info",
            "src/x.rs:1: boom",
            "ring line",
            "-- esquie.log --\nhello",
        );
        for expected in [
            "app version: 9.9.9",
            "== System ==",
            "Linux test 6.1",
            "== Desktop session ==",
            "== PipeWire ==",
            "pw-cli server info",
            "== Previous crash ==",
            "src/x.rs:1: boom",
            "== Recent log entries (ring buffer) ==",
            "ring line",
            "== Log files ==",
            "hello",
        ] {
            assert!(
                bundle.contains(expected),
                "missing {expected:?} in:\n{bundle}"
            );
        }
    }

    #[test]
    fn empty_sections_are_marked() {
        let bundle = assemble("1", "", "", "", "", "", "");
        assert!(bundle.contains("(empty)"));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let s = "aä€".repeat(100);
        let i = floor_char_boundary(&s, 5);
        assert!(s.is_char_boundary(i));
        assert!(i <= 5);
    }

    #[test]
    fn default_file_name_is_dated() {
        assert!(default_file_name().starts_with("esquie-diagnostics-"));
        assert!(default_file_name().ends_with(".txt"));
    }
}
