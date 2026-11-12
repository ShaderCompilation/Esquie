//! Diagnostics export: assemble one plain-text bundle on request.
//!
//! The user explicitly picks the destination via a native save-file dialog;
//! nothing is uploaded or sent anywhere. The bundle is plain text for
//! readability and attachments to issue trackers.

use std::io::{self, Read, Seek, SeekFrom};
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
        .filter(|e| e.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|e| e.path())
        .filter(|p| logging::is_log_file(p))
        .collect();
    files.sort(); // rotated names are date-stamped, so lexical == chronological
    if files.is_empty() {
        return "(no log files)".into();
    }
    files
        .iter()
        .map(|path| {
            let (text, truncated) = std::fs::File::open(path)
                .and_then(|mut file| read_log_tail(&mut file))
                .unwrap_or_else(|e| (format!("(unreadable: {e})"), false));
            let note = if truncated {
                format!("\n(truncated to the last {MAX_LOG_FILE_BYTES} bytes)")
            } else {
                String::new()
            };
            format!("-- {} --\n{}{}", path.display(), text.trim_end(), note)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Seek before reading, bounding both allocation and I/O even for very large
/// daily logs. Discard any partial UTF-8 character at the start of the tail.
fn read_log_tail(reader: &mut (impl Read + Seek)) -> io::Result<(String, bool)> {
    let len = reader.seek(SeekFrom::End(0))?;
    let start = len.saturating_sub(MAX_LOG_FILE_BYTES as u64);
    reader.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((len - start) as usize);
    reader
        .take(MAX_LOG_FILE_BYTES as u64)
        .read_to_end(&mut bytes)?;
    let skip = if start > 0 {
        bytes
            .iter()
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count()
    } else {
        0
    };
    Ok((
        String::from_utf8_lossy(&bytes[skip..]).into_owned(),
        start > 0,
    ))
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
    fn log_tail_keeps_recent_errors_on_a_utf8_boundary() {
        let text = format!(
            "{}\nrecent crash backtrace",
            "aä€".repeat(MAX_LOG_FILE_BYTES)
        );
        let (tail, truncated) = read_log_tail(&mut std::io::Cursor::new(text.as_bytes())).unwrap();
        assert!(truncated);
        assert!(tail.len() <= MAX_LOG_FILE_BYTES);
        assert!(text.ends_with(&tail));
        assert!(tail.ends_with("recent crash backtrace"));
        assert!(!tail.contains('\u{fffd}'));
    }

    #[test]
    fn small_log_tail_preserves_the_whole_file() {
        for text in ["", "aä€\n"] {
            let (tail, truncated) =
                read_log_tail(&mut std::io::Cursor::new(text.as_bytes())).unwrap();
            assert_eq!(tail, text);
            assert!(!truncated);
        }
    }

    #[test]
    fn default_file_name_is_dated() {
        assert!(default_file_name().starts_with("esquie-diagnostics-"));
        assert!(default_file_name().ends_with(".txt"));
    }
}
