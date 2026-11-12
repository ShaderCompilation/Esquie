# Running the Esquie AppImage

`Esquie-<version>-x86_64.AppImage` is a single file that runs Esquie on
x86_64 Linux desktops without installing Rust, a compiler, or development
packages. It is built on Ubuntu 24.04, the oldest supported baseline. The source
installer remains available: `./install.sh` builds from a checkout and installs
for the current user (see [building Esquie](building.md)).

GitHub Actions artifacts are review candidates, not published releases. Automated
container checks do not establish real desktop, GPU, microphone, Bluetooth, or
suspend/resume compatibility. See [troubleshooting](troubleshooting.md) for help.

## Download, verify, and run

Download the AppImage and its `.sha256` file into the same directory, then:

```sh
sha256sum --check Esquie-0.1.0-x86_64.AppImage.sha256
chmod +x Esquie-0.1.0-x86_64.AppImage
./Esquie-0.1.0-x86_64.AppImage
```

Downloads are not executable until you set the executable bit, either with
`chmod +x` or in your file manager (Properties → Permissions → "Allow executing
file as program"). Run it as your normal desktop user, not with `sudo`: Esquie
connects to your user's PipeWire session.

New users first choose a microphone, optionally check their sound, and select
Esquie in their apps. After setup, Esquie can offer to install a copy from the
Microphone view. **Not now** defers until a later launch; **Don't ask again**
dismisses permanently. Hidden startup,
source/development runs, and existing desktop integration do not trigger the
offer. Installation remains available in **Settings**.

`./Esquie-<version>-x86_64.AppImage --version` prints the version without
opening a window or connecting to audio.

## Host prerequisites

The AppImage contains Esquie, its desktop entry, its icon, and license notices. It deliberately
uses these from your system:

- **PipeWire 1.0 or newer** with a running session manager (WirePlumber), as on
  current Ubuntu, Debian, Fedora, and Arch desktops. Esquie loads the host's
  `libpipewire-0.3.so.0`, PipeWire's plugins, and your PipeWire configuration
  (including `PIPEWIRE_REMOTE` and `PIPEWIRE_RUNTIME_DIR` if you set them). It
  does not ship or start an audio server or session manager.
- **`pipewire-pulse`** if the apps that should use the virtual microphone talk
  to PulseAudio (most browsers and Discord do).
- **A Wayland or X11 desktop session** and its standard client libraries
  (Wayland, X11/XCB, xkbcommon, fontconfig, FreeType), plus OpenGL/EGL from your
  graphics driver. Desktop installs already have these. Without working GPU
  drivers, run with `SLINT_BACKEND=winit-software`.
- **glibc 2.39 or newer** (Ubuntu 24.04's), which every supported distro has.
- **FUSE** to mount the AppImage: the `fusermount3` or `fusermount` program and
  `/dev/fuse`. See [FUSE troubleshooting](#fuse-troubleshooting) if it is missing.
- A **StatusNotifier tray** is optional, as described in the README.

## Adding it to the application menu

Open **Settings → Installation and launchers**:

- **Install a copy** copies the AppImage to `~/.local/bin/esquie` by default.
  Change the installation directory first to use another absolute path. A file
  already there requires explicit replacement or a different directory. No root
  access is needed, and the original download stays untouched.
- **Add launcher without moving the app** uses the current persistent path.
  Move the AppImage somewhere permanent before choosing this option.
- **Repair launcher** repairs Esquie-owned menu entries and installs the icon.
  **Remove launcher** removes owned menu entries; it keeps the executable and
  startup preference. A shared icon is preserved when external launchers exist.

Launchers and icons use `XDG_DATA_HOME` (default `~/.local/share`). Existing
integration is detected across user and system XDG application directories,
including alternate filenames. Gear Lever, AppImageLauncher, and other external
entries are preserved. If an external launcher already exists, Esquie avoids
creating a duplicate; change its target through that manager after copying.
Externally managed startup entries are also left alone.

Installing does not restart audio. Owned launchers and enabled owned autostart
entries point at the selected copy immediately, and that path is remembered
across launches, including later runs of the original download. Choose
**Switch to installed copy** to end the current audio session and launch the
copy. This briefly interrupts the virtual microphone. You can also quit and
launch the installed copy later.

The command-line `--print-launcher-entry` still prints a menu entry without
writing integration files. Existing source installations can continue to use
`install.sh`.

## Updating

Updates are manual: the AppImage does not check for or download updates.

1. Quit Esquie from its tray menu (or close the window if there is no tray).
2. Download the new AppImage and `.sha256`, and verify it as above.
3. Make it executable and start it. If it replaces the old file at the same
   path, nothing else changes. If the file name or directory differs (new
   versions have new names), use **Add launcher without moving the app** from
   the new copy to select its path and refresh owned integration. If you used
   **Install a copy**, install the new version into the same directory and
   confirm replacement instead.
4. Delete the old AppImage.

Settings in `~/.config/esquie/settings` are kept across versions.

## Removing

Turn off **Start on login** (or delete `~/.config/autostart/esquie.desktop`),
use **Settings → Remove launcher**, then quit Esquie. Delete the installed
`~/.local/bin/esquie` (or your chosen copy) and the original download if no
longer needed. Remove external integration through its manager. Delete
`~/.config/esquie` to remove settings. With custom XDG directories, use the
corresponding config/data paths.

## FUSE troubleshooting

An AppImage normally mounts itself with FUSE. If this fails, it reports that it
cannot mount the AppImage, mentions `fusermount`, or says FUSE is missing. Common
causes are a missing FUSE package, `/dev/fuse` not being available (often in
containers and some sandboxes), or `nosuid`/permission restrictions. See the
upstream [AppImage FUSE guide](https://docs.appimage.org/user-guide/troubleshooting/fuse.html).

**Install FUSE** (this AppImage's runtime works with FUSE 3 or 2; only the
command-line helper is needed, not libfuse 2):

| Distro | Command |
| --- | --- |
| Ubuntu, Debian | `sudo apt install fuse3` |
| Fedora | `sudo dnf install fuse3` |
| Arch Linux | `sudo pacman -S fuse3` |

**Run without FUSE, once:** set `APPIMAGE_EXTRACT_AND_RUN=1`. The AppImage
unpacks itself to a temporary directory for that run and cleans up afterwards:

```sh
APPIMAGE_EXTRACT_AND_RUN=1 ./Esquie-0.1.0-x86_64.AppImage
```

Start on login launches the AppImage without that variable, so it still needs
FUSE. Use extraction instead if FUSE cannot be installed.

**Extract permanently:** this works everywhere and starts slightly faster.

```sh
mkdir -p ~/Applications && cd ~/Applications
/path/to/Esquie-0.1.0-x86_64.AppImage --appimage-extract
mv squashfs-root Esquie
~/Applications/Esquie/AppRun
```

An extracted copy supports **Add launcher without moving the app**. Its
launcher and **Start on login** point at
`~/Applications/Esquie/usr/bin/esquie`. Keep the complete directory intact;
copying only its executable through **Install a copy** is disabled. To update,
extract the new version and replace the directory. Select its path again from
Settings if you moved it.

## License information

Esquie is licensed under [GPL-3.0-only](../LICENSE). After extraction, license
texts and third-party notices are in `squashfs-root/usr/share/doc/esquie/`.
The [third-party notice overview](../THIRD_PARTY_NOTICES.md) explains the
embedded model and other components.
