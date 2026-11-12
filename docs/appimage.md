# Running the Esquie AppImage

`Esquie-<version>-x86_64.AppImage` is a single file that runs Esquie on
x86_64 Linux desktops without installing Rust, a compiler, or development
packages. It is built on Ubuntu 24.04, the oldest supported baseline. The source
installer remains available: `./install.sh` builds from a checkout and installs
for the current user (see [building Esquie](building.md)).

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

Keep the AppImage somewhere permanent, such as `~/Applications`, before turning
on **Start on login**: the autostart entry points at the AppImage file.

`./Esquie-<version>-x86_64.AppImage --version` prints the version without
opening a window or connecting to audio.

## Host prerequisites

The AppImage contains Esquie, its desktop entry, and its icon. It deliberately
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

AppImages don't install launcher entries by themselves. To add one that runs
this AppImage from where it is now:

```sh
./Esquie-0.1.0-x86_64.AppImage --print-launcher-entry > ~/.local/share/applications/esquie.desktop
./Esquie-0.1.0-x86_64.AppImage --appimage-extract esquie.svg
install -Dm644 squashfs-root/esquie.svg ~/.local/share/icons/hicolor/scalable/apps/esquie.svg
rm -r squashfs-root
```

Tools such as Gear Lever or AppImageLauncher can manage this for you instead.
Repeat the first command after moving the AppImage.

## Updating

Updates are manual: the AppImage does not check for or download updates.

1. Quit Esquie from its tray menu (or close the window if there is no tray).
2. Download the new AppImage and `.sha256`, and verify it as above.
3. Make it executable and start it. If it replaces the old file at the same
   path, nothing else changes. If the file name or directory differs (new
   versions have new names), launching it once updates the **Start on login**
   entry to the new path; regenerate the menu entry if you added one.
4. Delete the old AppImage.

Settings in `~/.config/esquie/settings` are kept across versions.

## Removing

Turn off **Start on login** (or delete `~/.config/autostart/esquie.desktop`),
quit Esquie, then delete the AppImage plus any menu entry and icon you added.
Delete `~/.config/esquie` to remove settings.

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

An extracted copy behaves like an ordinary install: **Start on login** and
`--print-launcher-entry` point at `~/Applications/Esquie/usr/bin/esquie`.
To update, extract the new version and replace the directory, then launch it
once.
