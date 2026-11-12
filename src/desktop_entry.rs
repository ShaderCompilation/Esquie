//! Desktop entries Esquie writes: the application launcher (generated for
//! `install.sh` through `--print-launcher-entry`) and the login autostart entry.
//! Both share the executable resolution and `Exec` quoting here.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// Passed by the autostart entry so Esquie starts quietly in the tray.
pub const HIDDEN_FLAG: &str = "--hidden";

const COMMENT: &str = "Noise removal for your microphone";

/// The path desktop entries should launch.
///
/// Inside an AppImage the running executable lives in a temporary mount
/// (`APPDIR`) that disappears on exit, so the AppImage file itself (`APPIMAGE`)
/// is used instead. Ordinary binaries and persistently extracted AppImages,
/// where the runtime sets no `APPIMAGE`, use the executable path.
pub fn executable() -> io::Result<PathBuf> {
    resolve_executable(
        std::env::current_exe()?,
        std::env::var_os("APPIMAGE"),
        std::env::var_os("APPDIR"),
    )
}

fn resolve_executable(
    exe: PathBuf,
    appimage: Option<OsString>,
    appdir: Option<OsString>,
) -> io::Result<PathBuf> {
    let non_empty = |v: Option<OsString>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    let (Some(appimage), Some(appdir)) = (non_empty(appimage), non_empty(appdir)) else {
        return Ok(exe);
    };
    let appdir = std::fs::canonicalize(&appdir).unwrap_or(appdir);
    if !exe.starts_with(&appdir) {
        // Inherited from some other AppImage that launched us.
        return Ok(exe);
    }
    if appimage.is_absolute() && appimage.is_file() {
        Ok(appimage)
    } else {
        // Never fall back to the mount directory: it is gone after this run.
        Err(io::Error::other(format!(
            "running from an AppImage mount, but APPIMAGE ({}) is not an existing absolute path",
            appimage.display()
        )))
    }
}

/// The application-menu entry launching `exe`.
pub fn launcher(exe: &Path) -> io::Result<String> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Esquie\n\
         X-Esquie-Owned=true\n\
         Comment={COMMENT}\n\
         Exec={}\n\
         Icon=esquie\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;Utility;\n\
         Keywords=noise;microphone;mic;suppression;denoise;\n\
         StartupNotify=false\n\
         StartupWMClass=esquie\n",
        exec_line(exe, &[])?
    ))
}

/// The login autostart entry launching `exe` hidden in the tray.
pub fn autostart(exe: &Path) -> io::Result<String> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Esquie\n\
         X-Esquie-Owned=true\n\
         Comment={COMMENT}\n\
         Exec={}\n\
         Icon=audio-input-microphone\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        autostart_exec(exe)?
    ))
}

/// The `Exec` value of the autostart entry.
pub fn autostart_exec(exe: &Path) -> io::Result<String> {
    exec_line(exe, &[HIDDEN_FLAG])
}

/// Runs `$0` with the remaining arguments; see [`exec_line`].
const SH_EXEC: &str = r#"exec "$0" "$@""#;

/// An `Exec` value, already escaped for the desktop file's string syntax.
fn exec_line(program: &Path, args: &[&str]) -> io::Result<String> {
    let program = program.to_str().ok_or_else(|| {
        io::Error::other(format!(
            "{} is not valid UTF-8, which desktop entries require",
            program.display()
        ))
    })?;
    // GLib looks up the program before expanding `%%`, so it rejects such an
    // entry (and mangles a lone `%`). Arguments expand correctly everywhere,
    // so pass a program path containing `%` to sh as an argument instead.
    let wrapper: &[&str] = if program.contains('%') {
        &["sh", "-c", SH_EXEC]
    } else {
        &[]
    };
    let line = wrapper
        .iter()
        .copied()
        .chain(std::iter::once(program))
        .chain(args.iter().copied())
        .map(quote_arg)
        .collect::<Vec<_>>()
        .join(" ");
    escape_value(&line)
}

/// Desktop Entry Specification "Exec key" quoting for one argument, including
/// `%%` for literal percent signs (which are field codes otherwise).
fn quote_arg(arg: &str) -> String {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`',
    ];
    let needs_quotes = arg.is_empty() || arg.contains(RESERVED);
    let mut out = String::with_capacity(arg.len() + 2);
    if needs_quotes {
        out.push('"');
    }
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' if needs_quotes => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    if needs_quotes {
        out.push('"');
    }
    out
}

/// The desktop file's general string escaping, applied after `Exec` quoting.
/// This is why a literal backslash ends up written as four.
fn escape_value(value: &str) -> io::Result<String> {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                return Err(io::Error::other(
                    "desktop entries cannot contain control characters",
                ));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Only keys in the main group count; actions can have unrelated commands.
pub fn main_group(contents: &str) -> impl Iterator<Item = (usize, &str, &str)> {
    let mut in_main = false;
    contents.lines().enumerate().filter_map(move |(i, line)| {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
        }
        if !in_main || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        Some((i, key.trim(), value.trim()))
    })
}

pub fn value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    main_group(contents).find_map(|(_, k, v)| (k == key).then_some(v))
}

/// Explicit ownership for new entries, or the signature of our old templates.
/// A name or executable match alone never grants permission to change a file.
pub fn owned(contents: &str) -> bool {
    if value(contents, "Name") != Some("Esquie") {
        return false;
    }
    // Managers can retain keys from the packaged desktop entry when they
    // integrate it. Their metadata takes precedence over our original marker.
    if main_group(contents).any(|(_, k, _)| {
        k.starts_with("X-AppImage") || k.to_ascii_lowercase().contains("gearlever")
    }) {
        return false;
    }
    if value(contents, "X-Esquie-Owned") == Some("true") {
        return true;
    }
    value(contents, "Comment") == Some(COMMENT)
        && value(contents, "Type") == Some("Application")
        && (value(contents, "StartupWMClass") == Some("esquie")
            || (value(contents, "Icon") == Some("audio-input-microphone")
                && value(contents, "X-GNOME-Autostart-enabled").is_some()))
}

/// Decode an Exec command without invoking a shell. Also understands the exact
/// percent-path wrapper generated above, and external managers' `env` prefix.
pub fn exec_program(contents: &str) -> Option<PathBuf> {
    let raw = value(contents, "Exec")?;
    let mut unescaped = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        unescaped.push(if c == '\\' {
            match chars.next()? {
                's' => ' ',
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '\\' => '\\',
                _ => return None,
            }
        } else {
            c
        });
    }
    let mut argv = Vec::new();
    let mut arg = String::new();
    let mut quote = None;
    let mut active = false;
    let mut chars = unescaped.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' if quote == Some(c) => quote = None,
            '"' | '\'' if quote.is_none() => {
                quote = Some(c);
                active = true;
            }
            '\\' if quote != Some('\'') => {
                arg.push(chars.next()?);
                active = true;
            }
            c if c.is_whitespace() && quote.is_none() => {
                if active {
                    argv.push(std::mem::take(&mut arg));
                    active = false;
                }
            }
            '%' => match chars.next()? {
                '%' => {
                    arg.push('%');
                    active = true;
                }
                'f' | 'F' | 'u' | 'U' | 'i' | 'c' | 'k' => {}
                _ => return None,
            },
            c => {
                arg.push(c);
                active = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if active {
        argv.push(arg);
    }
    // Field-code arguments don't affect the executable's identity.
    program_from_args(&argv)
}

fn program_from_args(argv: &[String]) -> Option<PathBuf> {
    if argv.len() >= 4
        && matches!(argv[0].as_str(), "sh" | "/bin/sh")
        && argv[1] == "-c"
        && argv[2] == SH_EXEC
    {
        return Some(PathBuf::from(&argv[3]));
    }
    let mut args = argv.iter();
    let first = args.next()?;
    if matches!(first.as_str(), "env" | "/usr/bin/env") {
        args.find(|a| {
            !a.split_once('=').is_some_and(|(key, _)| {
                !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            }) && a.as_str() != "--"
        })
        .map(PathBuf::from)
    } else {
        Some(PathBuf::from(first))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// How a desktop environment reads an `Exec` value back: string unescape,
    /// `%%` field code, then Exec argument quoting.
    pub(crate) fn parse_exec(value: &str) -> Vec<String> {
        let mut unescaped = String::new();
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                unescaped.push(c);
                continue;
            }
            match chars.next().expect("dangling backslash") {
                's' => unescaped.push(' '),
                'n' => unescaped.push('\n'),
                't' => unescaped.push('\t'),
                'r' => unescaped.push('\r'),
                '\\' => unescaped.push('\\'),
                other => panic!("invalid string escape \\{other}"),
            }
        }

        let mut args = Vec::new();
        let mut chars = unescaped.chars().peekable();
        loop {
            while chars.next_if_eq(&' ').is_some() {}
            let Some(first) = chars.next() else {
                return args;
            };
            let mut arg = String::new();
            fn push(arg: &mut String, c: char, chars: &mut std::iter::Peekable<std::str::Chars>) {
                if c == '%' {
                    assert_eq!(chars.next(), Some('%'), "unexpected field code");
                }
                arg.push(c);
            }
            if first == '"' {
                loop {
                    match chars.next().expect("unterminated quote") {
                        '"' => break,
                        '\\' => {
                            let c = chars.next().expect("dangling backslash");
                            assert!(
                                matches!(c, '"' | '`' | '$' | '\\'),
                                "invalid quoted escape \\{c}"
                            );
                            arg.push(c);
                        }
                        c => push(&mut arg, c, &mut chars),
                    }
                }
                assert!(
                    matches!(chars.peek(), None | Some(' ')),
                    "text after closing quote"
                );
            } else {
                assert!(
                    !matches!(first, '"' | '\\' | '$' | '`'),
                    "reserved character outside quotes"
                );
                push(&mut arg, first, &mut chars);
                while let Some(c) = chars.next_if(|&c| c != ' ') {
                    assert!(
                        !matches!(c, '"' | '\\' | '$' | '`'),
                        "reserved character outside quotes"
                    );
                    push(&mut arg, c, &mut chars);
                }
            }
            args.push(arg);
        }
    }

    /// The command a parsed `Exec` runs, seeing through the `sh` wrapper.
    pub(crate) fn command(mut argv: Vec<String>) -> Vec<String> {
        if argv.len() >= 4 && argv[..3] == ["sh", "-c", SH_EXEC] {
            argv.drain(..3);
        }
        argv
    }

    #[test]
    fn exec_quoting_examples() {
        let exec = |p: &str| autostart_exec(Path::new(p)).unwrap();
        assert_eq!(exec("/usr/bin/esquie"), "/usr/bin/esquie --hidden");
        assert_eq!(
            exec("/home/a b/Esquie.AppImage"),
            r#""/home/a b/Esquie.AppImage" --hidden"#
        );
        assert_eq!(exec(r#"/x/say "hi"/h"#), r#""/x/say \\"hi\\"/h" --hidden"#);
        assert_eq!(exec(r"/x/back\slash/h"), r#""/x/back\\\\slash/h" --hidden"#);
        assert_eq!(
            exec("/x/100%/h"),
            r#"sh -c "exec \\"\\$0\\" \\"\\$@\\"" /x/100%%/h --hidden"#
        );
        assert_eq!(
            exec("/x/100% sure/h"),
            r#"sh -c "exec \\"\\$0\\" \\"\\$@\\"" "/x/100%% sure/h" --hidden"#
        );
        assert_eq!(
            exec("/x/$HOME/`id`/h"),
            r#""/x/\\$HOME/\\`id\\`/h" --hidden"#
        );
        assert_eq!(exec("/x/it's/h"), r#""/x/it's/h" --hidden"#);
    }

    #[test]
    fn exec_round_trips_awkward_paths() {
        for path in [
            "/opt/Esquie/esquie",
            "/home/user/My Apps/Esquie-0.1.0-x86_64.AppImage",
            r#"/tmp/"quoted" dir/esquie"#,
            r"/tmp/back\slash\\double/esquie",
            "/tmp/50%/%f %%u/esquie",
            "/tmp/$(rm -rf ~)/`x`;&|<>*?#~/esquie",
            "/tmp/tab\tand\nnewline/esquie",
            "/tmp/ünïcødé/esquie",
        ] {
            let exec = autostart_exec(Path::new(path)).unwrap();
            assert_eq!(command(parse_exec(&exec)), [path, HIDDEN_FLAG], "{exec}");
            assert_eq!(exec.starts_with("sh "), path.contains('%'), "{exec}");
            let launcher = launcher(Path::new(path)).unwrap();
            assert_eq!(exec_program(&launcher).unwrap(), Path::new(path));
            let line = launcher
                .lines()
                .find_map(|l| l.strip_prefix("Exec="))
                .unwrap();
            assert_eq!(command(parse_exec(line)), [path], "{line}");
        }
    }

    #[test]
    fn manager_metadata_overrides_packaged_ownership_marker() {
        let packaged = launcher(Path::new("/tmp/Esquie.AppImage")).unwrap();
        assert!(owned(&packaged));
        assert!(!owned(&format!("{packaged}X-AppImage-Name=Esquie\n")));
        assert!(!owned(&format!("{packaged}X-AppImageLauncher-Version=1\n")));
        assert!(!owned(
            "[Desktop Entry]\nName=Esquie\nExec=/external/image\n"
        ));
        let external = "[Desktop Entry]\nName=Custom\nExec=env SOME_FLAG=1 \"/home/My Apps/a=b.AppImage\" %U\n";
        assert_eq!(
            exec_program(external).unwrap(),
            Path::new("/home/My Apps/a=b.AppImage")
        );
        assert!(exec_program("[Desktop Entry]\nExec=\"unterminated\n").is_none());
    }

    #[test]
    fn rejects_unrepresentable_paths() {
        use std::os::unix::ffi::OsStrExt;
        let non_utf8 = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff/esquie"));
        assert!(autostart_exec(non_utf8).is_err());
        assert!(autostart_exec(Path::new("/tmp/bell\x07/esquie")).is_err());
    }

    #[test]
    fn appimage_uses_persistent_path() {
        let dir =
            std::env::temp_dir().join(format!("esquie-appimage-test-{}", std::process::id()));
        let mount = dir.join(".mount_EsquieAbCdEf");
        std::fs::create_dir_all(mount.join("usr/bin")).unwrap();
        let image = dir.join("My Apps/Esquie.AppImage");
        std::fs::create_dir_all(image.parent().unwrap()).unwrap();
        std::fs::write(&image, b"").unwrap();
        let mount = std::fs::canonicalize(&mount).unwrap();
        let exe = mount.join("usr/bin/esquie");
        let some = |p: &Path| Some(p.as_os_str().to_owned());

        let resolve = |appimage, appdir| resolve_executable(exe.clone(), appimage, appdir);
        // Running inside the AppImage mount: use the AppImage file.
        assert_eq!(resolve(some(&image), some(&mount)).unwrap(), image);
        // Ordinary binary or persistently extracted AppDir: the executable.
        assert_eq!(resolve(None, None).unwrap(), exe);
        assert_eq!(resolve(None, some(&mount)).unwrap(), exe);
        // Environment inherited from an unrelated AppImage.
        assert_eq!(
            resolve(some(&image), some(&dir.join("elsewhere"))).unwrap(),
            exe
        );
        // A mount without a usable AppImage path must not be referenced.
        assert!(resolve(some(&dir.join("missing.AppImage")), some(&mount)).is_err());
        assert!(resolve(some(Path::new("relative.AppImage")), some(&mount)).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
