//! Installation identity, read-only XDG inventory, and per-user integration.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{
    autostart, desktop_entry,
    file_ops::Transaction,
    settings::{InstallPrompt, Settings},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    AppImage,
    Binary,
    Extracted,
}

#[derive(Clone, Debug)]
pub struct Launcher {
    pub path: PathBuf,
    pub owned: bool,
    pub user: bool,
}

#[derive(Clone)]
pub struct Locations {
    pub data: PathBuf,
    pub applications: Vec<PathBuf>,
    pub config: PathBuf,
    pub default_bin: PathBuf,
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}
impl Locations {
    pub fn get() -> io::Result<Self> {
        let home =
            env_path("HOME").ok_or_else(|| io::Error::other("HOME must be an absolute path"))?;
        let data = env_path("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let config = env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let mut applications = vec![data.join("applications")];
        let dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        for dir in std::env::split_paths(&dirs).filter(|p| p.is_absolute()) {
            let dir = dir.join("applications");
            if !applications.contains(&dir) {
                applications.push(dir);
            }
        }
        Ok(Self {
            data,
            config,
            applications,
            default_bin: home.join(".local/bin"),
        })
    }
}

pub struct Inventory {
    pub executable: PathBuf,
    pub kind: Kind,
    pub launchers: Vec<Launcher>,
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

fn kind(exe: &Path, persistent: &Path) -> Kind {
    if exe != persistent {
        return Kind::AppImage;
    }
    // linuxdeploy's AppRun links to usr/bin/esquie. This works without APPDIR
    // and avoids inferring extraction from a directory or filename alone.
    if exe
        .ancestors()
        .skip(1)
        .take(3)
        .any(|root| root.join("esquie.desktop").is_file() && same_path(&root.join("AppRun"), exe))
    {
        Kind::Extracted
    } else {
        Kind::Binary
    }
}

impl Inventory {
    pub fn current(locations: &Locations, settings: &Settings) -> io::Result<Self> {
        let current = std::env::current_exe()?;
        let persistent = desktop_entry::executable()?;
        Self::read(
            locations,
            &current,
            &persistent,
            settings.installed_path.as_deref(),
        )
    }

    fn read(
        locations: &Locations,
        current: &Path,
        persistent: &Path,
        installed: Option<&Path>,
    ) -> io::Result<Self> {
        let mut launchers = Vec::new();
        for dir in &locations.applications {
            scan(
                dir,
                &locations.data.join("applications"),
                persistent,
                installed,
                &mut launchers,
            )?;
        }
        Ok(Self {
            executable: persistent.into(),
            kind: kind(current, persistent),
            launchers,
        })
    }

    pub fn should_prompt(&self, decision: InstallPrompt, visible: bool, development: bool) -> bool {
        visible
            && !development
            && self.kind == Kind::AppImage
            && decision != InstallPrompt::Never
            && self.launchers.is_empty()
    }

    pub fn description(&self) -> String {
        let kind = match self.kind {
            Kind::AppImage => "AppImage",
            Kind::Binary => "Source binary",
            Kind::Extracted => "Permanently extracted copy",
        };
        let owned = self.launchers.iter().filter(|l| l.owned && l.user).count();
        format!(
            "{kind}: {}\n{owned} Esquie launcher(s), {} external/system launcher(s).",
            self.executable.display(),
            self.launchers.len() - owned
        )
    }
}

fn scan(
    dir: &Path,
    user_dir: &Path,
    exe: &Path,
    installed: Option<&Path>,
    found: &mut Vec<Launcher>,
) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            scan(&path, user_dir, exe, installed, found)?;
            continue;
        }
        if path.extension().is_none_or(|e| e != "desktop") {
            continue;
        }
        let contents = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let identity = ["Name", "StartupWMClass", "X-AppImage-Name"]
            .iter()
            .any(|k| {
                desktop_entry::value(&contents, k).is_some_and(|v| v.eq_ignore_ascii_case("esquie"))
            });
        let path_match = desktop_entry::exec_program(&contents)
            .is_some_and(|p| same_path(&p, exe) || installed.is_some_and(|i| same_path(&p, i)));
        if identity || path_match {
            found.push(Launcher {
                owned: desktop_entry::owned(&contents),
                user: path.starts_with(user_dir) && !entry.file_type()?.is_symlink(),
                path,
            });
        }
    }
    Ok(())
}

pub fn preferred_executable() -> io::Result<PathBuf> {
    if let Some(p) = Settings::load().installed_path {
        if p.is_file() {
            return Ok(p);
        }
        return Err(io::Error::other(format!(
            "The installed copy at {} is missing. Repair it from Settings.",
            p.display()
        )));
    }
    desktop_entry::executable()
}

pub fn destination(directory: &Path) -> io::Result<PathBuf> {
    if !directory.is_absolute()
        || directory
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(io::Error::other(
            "Choose an absolute directory without '..'.",
        ));
    }
    // Validate desktop-entry representation before copying anything.
    let path = directory.join("esquie");
    desktop_entry::launcher(&path)?;
    if directory.exists() && !directory.is_dir() {
        return Err(io::Error::other("The destination is not a directory."));
    }
    Ok(path)
}

#[derive(Debug)]
pub enum Outcome {
    Collision(PathBuf),
    Complete { target: PathBuf, external: bool },
}

/// No process is launched here. The caller offers an explicit audio handoff.
pub fn install(
    locations: &Locations,
    inventory: &Inventory,
    directory: &Path,
    replace: bool,
) -> io::Result<Outcome> {
    if inventory.kind == Kind::Extracted {
        return Err(io::Error::other(
            "Keep the complete extracted directory together. Use Add launcher at its current path.",
        ));
    }
    let target = destination(directory)?;
    let same = same_path(&inventory.executable, &target);
    if target.try_exists()? && !same && !replace {
        return Ok(Outcome::Collision(target));
    }
    let mut tx = Transaction::default();
    if !same {
        tx.copy(&inventory.executable, &target)?;
    }
    let external = stage_integration(locations, inventory, &target, &mut tx)?;
    tx.commit()?;
    refresh_icon_cache(locations);
    Ok(Outcome::Complete { target, external })
}

pub fn integrate(locations: &Locations, inventory: &Inventory, target: &Path) -> io::Result<bool> {
    if !target.is_absolute() || !target.is_file() {
        return Err(io::Error::other("The persistent executable is missing."));
    }
    let mut tx = Transaction::default();
    let external = stage_integration(locations, inventory, target, &mut tx)?;
    tx.commit()?;
    refresh_icon_cache(locations);
    Ok(external)
}

fn stage_integration(
    locations: &Locations,
    inventory: &Inventory,
    target: &Path,
    tx: &mut Transaction,
) -> io::Result<bool> {
    let owned: Vec<_> = inventory
        .launchers
        .iter()
        .filter(|l| l.owned && l.user)
        .collect();
    let external = inventory.launchers.iter().any(|l| !l.owned || !l.user);
    // Respect managers' existing launchers rather than creating a duplicate.
    if !owned.is_empty() || !external {
        let icon = locations.data.join("icons/hicolor/scalable/apps/esquie.svg");
        if icon.exists()
            && fs::read(&icon)? != include_bytes!("../ui/icon.svg")
            && (owned.is_empty() || external)
        {
            return Err(io::Error::other(
                "An unrecognized esquie.svg already exists; it was left unchanged.",
            ));
        }
        tx.write(&icon, include_bytes!("../ui/icon.svg"), 0o644)?;
        let launcher = desktop_entry::launcher(target)?;
        if owned.is_empty() {
            let path = locations.data.join("applications/esquie.desktop");
            if path.try_exists()? {
                return Err(io::Error::other(
                    "An external esquie.desktop occupies the launcher path; it was left unchanged.",
                ));
            }
            tx.write(&path, launcher.as_bytes(), 0o644)?;
        } else {
            for entry in owned {
                if !desktop_entry::owned(&fs::read_to_string(&entry.path)?) {
                    return Err(io::Error::other("Launcher ownership changed; retry."));
                }
                tx.write(&entry.path, launcher.as_bytes(), 0o644)?;
            }
        }
    }
    autostart::stage_refresh_all(&locations.config.join("autostart"), target, tx)?;
    Ok(external)
}

/// GTK stops scanning an icon theme directory once `icon-theme.cache` exists,
/// so a freshly installed icon is invisible until the cache is refreshed.
/// Best effort: regenerate it, or drop it so lookups fall back to scanning.
fn refresh_icon_cache(locations: &Locations) {
    let hicolor = locations.data.join("icons/hicolor");
    let cache = hicolor.join("icon-theme.cache");
    if !cache.exists() {
        return;
    }
    let refreshed = std::process::Command::new("gtk-update-icon-cache")
        .arg("-q")
        .arg("-f")
        .arg("-t")
        .arg(&hicolor)
        .status()
        .is_ok_and(|s| s.success());
    if !refreshed {
        let _ = fs::remove_file(&cache);
    }
}

/// Remove only owned user entries, and our exact embedded icon. Never binaries.
pub fn remove_launchers(locations: &Locations, inventory: &Inventory) -> io::Result<()> {
    let mut tx = Transaction::default();
    let mut removed = false;
    for entry in inventory.launchers.iter().filter(|l| l.owned && l.user) {
        if desktop_entry::owned(&fs::read_to_string(&entry.path)?) {
            tx.remove(&entry.path)?;
            removed = true;
        }
    }
    let icon = locations.data.join("icons/hicolor/scalable/apps/esquie.svg");
    if removed
        && !inventory.launchers.iter().any(|l| !l.owned || !l.user)
        && fs::read(&icon).is_ok_and(|b| b == include_bytes!("../ui/icon.svg"))
    {
        tx.remove(&icon)?;
    }
    tx.commit()?;
    if removed {
        refresh_icon_cache(locations);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);

    struct Scratch {
        root: PathBuf,
        locations: Locations,
        image: PathBuf,
    }
    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "esquie-install-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let data = root.join("custom user data");
            let locations = Locations {
                applications: vec![
                    data.join("applications"),
                    root.join("system data/applications"),
                    root.join("other system/applications"),
                ],
                data,
                config: root.join("custom config"),
                default_bin: root.join(".local/bin"),
            };
            let image = root.join("Downloads/Esquie 100% $special 'é'.AppImage");
            put(&image, b"original payload");
            Self {
                root,
                locations,
                image,
            }
        }
        fn inventory(&self) -> Inventory {
            Inventory::read(
                &self.locations,
                &self.root.join("mount/usr/bin/esquie"),
                &self.image,
                None,
            )
            .unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn put(path: &Path, contents: impl AsRef<[u8]>) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn inventory_matches_alternate_names_paths_and_xdg_locations_without_writes() {
        let s = Scratch::new();
        let alternate = s.locations.applications[0].join("nested/renamed.desktop");
        let entry = desktop_entry::launcher(&s.image)
            .unwrap()
            .replace("Name=Esquie", "Name=Custom label")
            .replace("StartupWMClass=esquie", "StartupWMClass=other")
            .replace("X-Esquie-Owned=true\n", "");
        put(&alternate, &entry);
        let system = s.locations.applications[1].join("gearlever-123.desktop");
        let external = "[Desktop Entry]\nName=Esquie\nX-AppImage-Name=Esquie\nExec=\"/some other copy/image\" %U\n";
        put(&system, external);
        let third = s.locations.applications[2].join("alternate.desktop");
        put(
            &third,
            "[Desktop Entry]\nName=Different\nStartupWMClass=esquie\nExec=/usr/bin/else\n",
        );
        put(
            &s.locations.applications[0].join("unrelated.desktop"),
            "[Desktop Entry]\nName=Unrelated\nExec=/bin/true\n[Desktop Action fake]\nName=Esquie\n",
        );
        let alias = s.root.join("other/path/alias.AppImage");
        fs::create_dir_all(alias.parent().unwrap()).unwrap();
        symlink(&s.image, &alias).unwrap();
        put(
            &s.locations.applications[0].join("path-alias.desktop"),
            desktop_entry::launcher(&alias)
                .unwrap()
                .replace("Name=Esquie", "Name=Alias")
                .replace("StartupWMClass=esquie", "StartupWMClass=alias"),
        );
        let inventory = s.inventory();
        assert_eq!(inventory.kind, Kind::AppImage);
        assert_eq!(inventory.launchers.len(), 4);
        assert!(inventory.launchers.iter().all(|l| !l.owned));
        assert!(!inventory.should_prompt(InstallPrompt::Ask, true, false));
        assert_eq!(fs::read_to_string(alternate).unwrap(), entry);
        assert_eq!(fs::read_to_string(system).unwrap(), external);
    }

    #[test]
    fn prompt_requires_visible_unintegrated_appimage_and_respects_dismissal() {
        let s = Scratch::new();
        let mut inventory = s.inventory();
        assert!(inventory.should_prompt(InstallPrompt::Ask, true, false));
        assert!(inventory.should_prompt(InstallPrompt::Later, true, false));
        assert!(!inventory.should_prompt(InstallPrompt::Never, true, false));
        assert!(!inventory.should_prompt(InstallPrompt::Ask, false, false));
        assert!(!inventory.should_prompt(InstallPrompt::Ask, true, true));
        inventory.kind = Kind::Binary;
        assert!(!inventory.should_prompt(InstallPrompt::Ask, true, false));
        inventory.kind = Kind::Extracted;
        assert!(!inventory.should_prompt(InstallPrompt::Ask, true, false));
    }

    #[test]
    fn distinguishes_extracted_payload_using_apprun_not_filename() {
        let s = Scratch::new();
        assert_eq!(kind(&s.image, &s.image), Kind::Binary);
        let exe = s.root.join("permanent/usr/bin/esquie");
        put(&exe, b"payload");
        symlink("usr/bin/esquie", s.root.join("permanent/AppRun")).unwrap();
        put(&s.root.join("permanent/esquie.desktop"), b"entry");
        assert_eq!(kind(&exe, &exe), Kind::Extracted);
    }

    #[test]
    fn installs_copy_with_permissions_persistent_quoted_paths_and_autostart() {
        let s = Scratch::new();
        let startup = s.locations.config.join("autostart/esquie.desktop");
        let old = desktop_entry::autostart(&s.image).unwrap() + "X-GNOME-Autostart-Delay=3\n";
        put(&startup, &old);
        let directory = s.root.join("My Apps/100% $'special' \\");
        let result = install(&s.locations, &s.inventory(), &directory, false).unwrap();
        let target = directory.join("esquie");
        assert!(matches!(result, Outcome::Complete { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"original payload");
        assert_eq!(fs::read(&s.image).unwrap(), b"original payload");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let launcher =
            fs::read_to_string(s.locations.data.join("applications/esquie.desktop")).unwrap();
        assert_eq!(desktop_entry::exec_program(&launcher).unwrap(), target);
        let startup = fs::read_to_string(startup).unwrap();
        assert_eq!(desktop_entry::exec_program(&startup).unwrap(), target);
        assert!(startup.contains("X-GNOME-Autostart-Delay=3"));
        assert!(!launcher.contains("mount/usr"));
    }

    #[test]
    fn collisions_require_explicit_replacement_or_allow_another_directory() {
        let s = Scratch::new();
        let target = s.locations.default_bin.join("esquie");
        put(&target, b"old install");
        assert!(matches!(
            install(
                &s.locations,
                &s.inventory(),
                &s.locations.default_bin,
                false
            )
            .unwrap(),
            Outcome::Collision(_)
        ));
        assert_eq!(fs::read(&target).unwrap(), b"old install");
        install(
            &s.locations,
            &s.inventory(),
            &s.root.join("alternative"),
            false,
        )
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old install");
        install(&s.locations, &s.inventory(), &s.locations.default_bin, true).unwrap();
        assert_eq!(fs::read(target).unwrap(), b"original payload");
    }

    #[test]
    fn launcher_only_repair_and_removal_preserve_external_entries() {
        let s = Scratch::new();
        integrate(&s.locations, &s.inventory(), &s.image).unwrap();
        assert!(!s.locations.default_bin.join("esquie").exists());
        let launcher = s.locations.data.join("applications/esquie.desktop");
        let renamed = s.locations.data.join("applications/alternate-name.desktop");
        fs::rename(&launcher, &renamed).unwrap();
        let stale = desktop_entry::launcher(Path::new("/old/esquie")).unwrap();
        put(&renamed, stale);
        let external = s.locations.applications[1].join("manager.desktop");
        let external_text =
            "[Desktop Entry]\nName=Esquie\nExec=/external/image\nX-AppImage-Name=Esquie\n";
        put(&external, external_text);
        integrate(&s.locations, &s.inventory(), &s.image).unwrap();
        assert!(!launcher.exists());
        assert_eq!(
            desktop_entry::exec_program(&fs::read_to_string(&renamed).unwrap()).unwrap(),
            s.image
        );
        remove_launchers(&s.locations, &s.inventory()).unwrap();
        assert!(!renamed.exists());
        assert_eq!(fs::read_to_string(external).unwrap(), external_text);
        // External integration may still reference our shared icon.
        assert!(
            s.locations
                .data
                .join("icons/hicolor/scalable/apps/esquie.svg")
                .exists()
        );
        assert!(s.image.exists());
    }

    #[test]
    fn external_launcher_and_startup_are_preserved_without_duplicate_launcher() {
        let s = Scratch::new();
        let path = s.locations.applications[0].join("gearlever.desktop");
        let text = "[Desktop Entry]\nName=Esquie\nExec=/managed/image\nX-AppImage-Name=Esquie\n";
        put(&path, text);
        let startup = s.locations.config.join("autostart/esquie.desktop");
        put(&startup, text);
        install(
            &s.locations,
            &s.inventory(),
            &s.locations.default_bin,
            false,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), text);
        assert_eq!(fs::read_to_string(startup).unwrap(), text);
        assert!(!s.locations.data.join("applications/esquie.desktop").exists());
    }

    #[test]
    fn preparation_failure_keeps_previous_install_and_integration_usable() {
        let s = Scratch::new();
        let target = s.locations.default_bin.join("esquie");
        put(&target, b"previous binary");
        integrate(&s.locations, &s.inventory(), &target).unwrap();
        let launcher = s.locations.data.join("applications/esquie.desktop");
        let old = fs::read(&launcher).unwrap();
        let startup = s.locations.config.join("autostart/esquie.desktop");
        put(&startup, desktop_entry::autostart(&s.image).unwrap());
        let directory = startup.parent().unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o555)).unwrap();
        let result = install(&s.locations, &s.inventory(), &s.locations.default_bin, true);
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(target).unwrap(), b"previous binary");
        assert_eq!(fs::read(launcher).unwrap(), old);
        assert_eq!(
            desktop_entry::exec_program(&fs::read_to_string(startup).unwrap()).unwrap(),
            s.image
        );
    }

    #[test]
    fn rejects_invalid_destinations_and_does_not_copy_extracted_payload() {
        let s = Scratch::new();
        for path in ["relative", "/tmp/../elsewhere", "/tmp/bell\x07"] {
            assert!(destination(Path::new(path)).is_err());
        }
        let mut inventory = s.inventory();
        inventory.kind = Kind::Extracted;
        assert!(install(&s.locations, &inventory, &s.locations.default_bin, false).is_err());
        assert!(!s.locations.default_bin.exists());
    }

    #[test]
    fn stale_icon_cache_is_refreshed_after_install_and_removal() {
        let s = Scratch::new();
        let cache = s.locations.data.join("icons/hicolor/icon-theme.cache");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, "stale cache without esquie").unwrap();
        install(&s.locations, &s.inventory(), &s.locations.default_bin, false).unwrap();
        // Refreshed caches list esquie; if the tool is unavailable the stale
        // cache is removed so lookups fall back to scanning the directory.
        let fresh = fs::read_to_string(&cache);
        assert!(fresh.is_err() || fresh.unwrap().contains("esquie"));

        let mut inventory = s.inventory();
        inventory.launchers.retain(|l| !l.owned || !l.user);
        remove_launchers(&s.locations, &inventory).unwrap();
        let fresh = fs::read_to_string(&cache);
        assert!(fresh.is_err() || !fresh.unwrap().contains("esquie"));
    }
}
