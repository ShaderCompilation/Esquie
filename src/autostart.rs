//! "Start on login" via an XDG autostart entry (~/.config/autostart/esquie.desktop).
//!
//! Sessions that ignore XDG autostart (sway, Hyprland, niri, ...) need
//! `esquie --hidden` in the compositor's own startup configuration instead.

use std::io;
use std::path::{Path, PathBuf};

use crate::desktop_entry;

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join("esquie.desktop"))
}

/// The entry file is the source of truth, so removing or disabling it from
/// the desktop's own startup settings is reflected here too.
pub fn is_enabled() -> bool {
    path().and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|c| entry_enabled(&c))
}

pub fn set_enabled(on: bool) -> io::Result<()> {
    let Some(p) = path() else { return Ok(()) };
    if !on {
        return match std::fs::remove_file(&p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let entry = desktop_entry::autostart(&desktop_entry::executable()?)?;
    write(&p, &entry)
}

/// Points an enabled entry at where Esquie now runs from, e.g. after the
/// AppImage was moved. Returns whether the entry changed.
pub fn refresh() -> io::Result<bool> {
    let Some(p) = path() else { return Ok(false) };
    let contents = match std::fs::read_to_string(&p) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        other => other?,
    };
    if !entry_enabled(&contents) {
        return Ok(false);
    }
    let exe = desktop_entry::executable()?;
    let updated = match with_exec(&contents, &desktop_entry::autostart_exec(&exe)?) {
        Some(updated) => updated,
        // No Exec line to update: the entry is unusable, so rewrite it.
        None => desktop_entry::autostart(&exe)?,
    };
    if updated == contents {
        return Ok(false);
    }
    write(&p, &updated)?;
    Ok(true)
}

/// Replace the file atomically so a crash never leaves a truncated entry.
fn write(p: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("desktop.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, p).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `(key, value)` pairs of the `[Desktop Entry]` group, with line indices.
fn main_group(contents: &str) -> impl Iterator<Item = (usize, &str, &str)> {
    let mut in_main = false;
    contents.lines().enumerate().filter_map(move |(i, line)| {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            return None;
        }
        if !in_main || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        Some((i, key.trim(), value.trim()))
    })
}

fn entry_enabled(contents: &str) -> bool {
    !main_group(contents)
        .any(|(_, k, v)| (k == "Hidden" && v == "true") || (k == "X-GNOME-Autostart-enabled" && v == "false"))
}

/// `contents` with its `Exec` value replaced, keeping every other line
/// (including ones the user added). `None` without an `Exec` key.
fn with_exec(contents: &str, exec: &str) -> Option<String> {
    let (index, _, _) = main_group(contents).find(|(_, k, _)| *k == "Exec")?;
    let mut out = String::with_capacity(contents.len() + exec.len());
    for (i, line) in contents.lines().enumerate() {
        if i == index {
            out.push_str("Exec=");
            out.push_str(exec);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !contents.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_entry::tests::parse_exec;

    #[test]
    fn enabled_state() {
        let entry = desktop_entry::autostart(Path::new("/usr/bin/esquie")).unwrap();
        assert!(entry_enabled(&entry));
        assert!(!entry_enabled(&entry.replace("X-GNOME-Autostart-enabled=true", "X-GNOME-Autostart-enabled=false")));
        assert!(!entry_enabled(&format!("{entry}Hidden=true\n")));
        // Keys in other groups don't count.
        assert!(entry_enabled(&format!("{entry}[Desktop Action x]\nHidden=true\n")));
    }

    #[test]
    fn refresh_replaces_only_exec() {
        let old = "[Desktop Entry]\nType=Application\nName=Esquie\nExec=/tmp/.mount_EsquieX/usr/bin/esquie --hidden\n\
                   X-GNOME-Autostart-Delay=5\n\n[Desktop Action x]\nExec=other\n";
        let exec = desktop_entry::autostart_exec(Path::new("/home/a b/Esquie.AppImage")).unwrap();
        let new = with_exec(old, &exec).unwrap();
        assert_eq!(
            new,
            "[Desktop Entry]\nType=Application\nName=Esquie\nExec=\"/home/a b/Esquie.AppImage\" --hidden\n\
             X-GNOME-Autostart-Delay=5\n\n[Desktop Action x]\nExec=other\n"
        );
        let line = new.lines().find_map(|l| l.strip_prefix("Exec=")).unwrap();
        assert_eq!(parse_exec(line), ["/home/a b/Esquie.AppImage", "--hidden"]);
        // Already current: unchanged.
        assert_eq!(with_exec(&new, &exec).unwrap(), new);
        assert_eq!(with_exec("[Desktop Entry]\nName=Esquie\n", &exec), None);
    }
}
