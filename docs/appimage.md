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

Download the AppImage, its `.sha256` file, and its `.sha256.minisig` signature
into the same directory, then:

```sh
minisign -Vm Esquie-<version>-x86_64.AppImage.sha256 -P RWQvr7YQ+BC9FdWSOgeTvrDOyn82Z0R97RNx2GhlrtIVQ3dhhQJ6sMPh
sha256sum --check Esquie-<version>-x86_64.AppImage.sha256
chmod +x Esquie-<version>-x86_64.AppImage
./Esquie-<version>-x86_64.AppImage
```

The [minisign](https://jedisct1.github.io/minisign/) step proves the checksum
was signed with Esquie's release key (also in
[`packaging/release/esquie.pub`](../packaging/release/esquie.pub)); the checksum
then proves the AppImage is the one that was signed. The key is the same for
every release, and Esquie's update check uses it too. Releases from before
signing was introduced have no `.minisig`; verify those with the checksum alone.

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
- A **StatusNotifier tray** is optional; see [tray and login behavior](usage.md#tray-and-start-on-login).

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

Esquie checks GitHub for a newer stable release shortly after startup and at
most once a day. This is its only network access; microphone audio is never
sent anywhere. Turn it off under **Settings → About and updates → Check for
updates automatically**; **Check now** works either way. A newer release is
announced only when its AppImage, checksum file, and a valid signature of
that checksum by the release key are published. Source builds and extracted
copies get instructions instead of **Update**.

**Update** replaces the selected copy: the installed copy after **Install a
copy**, otherwise the AppImage you started. It never touches a different file,
such as the original download after you installed a copy. Esquie:

1. records the selected file's identity and SHA-256, following a symlink to
   the file it points at;
2. downloads the new AppImage into a hidden `.esquie-update-<version>.part` file
   in the same directory, and checks its size and signed SHA-256 (audio keeps
   running, and **Cancel download** stops it);
3. checks again that the selected file is unchanged, and stops without
   replacing anything if another tool updated or moved it;
4. gives the new file the old one's permissions, keeps the old file as
   `.<name>.previous` beside it, and renames the new file into place. The
   path and file name stay the same, so launchers, the icon, Start on login,
   and settings keep working unchanged.

The path always holds either the complete old AppImage or the complete new one.
Only the most recent previous version is kept; each update replaces the backup.
No administrator access is needed, but you must be able to write to the
directory holding the copy. If you cannot, if the disk is full, or if the
download or verification fails, Esquie says so, removes its partial file, and
leaves your current copy unchanged. **Retry update** starts again from the
beginning; partial downloads are not resumed.

The running session keeps using the old version. **Restart now** saves your
settings, stops audio, and starts the new version from the same path; the
virtual microphone disappears for a few seconds and apps using it may need to
reselect it. **Later** keeps the current session running until you quit; the
next start uses the new version. Quitting during an update cancels the
download; a replacement that has already started is allowed to finish first.

If Esquie stops during an update (a crash or power loss), the next start removes
leftover `.esquie-update-*` files beside the selected copy and finishes keeping
the backup. If you later selected a copy in another directory, delete leftover
`.esquie-update-*` files there yourself.

### Going back to the previous version

Esquie does not roll back automatically. To restore the backup by hand:

1. Quit Esquie from its tray menu (or close the window if there is no tray).
2. In the directory of the updated copy, replace it with the backup, keeping
   the same name. For the default installation:

   ```sh
   cd ~/.local/bin
   mv .esquie.previous esquie
   ```

   For an AppImage named `Esquie-0.2.0-x86_64.AppImage`, the backup is
   `.Esquie-0.2.0-x86_64.AppImage.previous`. File names are not renamed
   during updates, so the name can show an older version than the contents.
   `./<file> --version` prints the real version.
3. Start Esquie again.

Settings in `~/.config/esquie/settings` are kept across versions in both
directions. An older version keeps settings it does not recognize, unchanged.
When it finds settings saved by a newer version, it says so in **About and
updates**. Before it first changes anything, it copies the file to
`settings.saved-by-<newer version>`. To go back to exactly those settings after
updating again, quit Esquie and copy that file over `settings`. (Versions from
before the updater replaced its own file did not keep unrecognized settings.)

### Updating copies you replace yourself

You can still update by hand, for example an extracted copy or one in a
directory you cannot write to:

1. Quit Esquie.
2. Download the new AppImage, `.sha256`, and `.sha256.minisig`, and verify
   them as above.
3. Make it executable and start it. If it replaces the old file at the same
   path, nothing else changes. If the file name or directory differs, use
   **Add launcher without moving the app** from the new copy to select its path
   and refresh owned integration. If you used **Install a copy**, install the
   new version into the same directory and confirm replacement instead.

Versions 0.1.0 and 0.1.1 cannot find signed updates, so updating from them is
always manual, once.

## Removing

Turn off **Start on login** (or delete `~/.config/autostart/esquie.desktop`),
use **Settings → Remove launcher**, then quit Esquie. Delete the installed
`~/.local/bin/esquie` (or your chosen copy), its `.esquie.previous` backup
(`.<name>.previous` for other names), and the original download if no longer
needed. Remove external integration through its manager. Delete
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
APPIMAGE_EXTRACT_AND_RUN=1 ./Esquie-0.2.0-x86_64.AppImage
```

Start on login launches the AppImage without that variable, so it still needs
FUSE. Use extraction instead if FUSE cannot be installed.

**Extract permanently:** this works everywhere and starts slightly faster.

```sh
mkdir -p ~/Applications && cd ~/Applications
/path/to/Esquie-0.2.0-x86_64.AppImage --appimage-extract
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
