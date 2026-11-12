# Esquie

Offline, real-time noise removal for your microphone on Linux (PipeWire), using
[DeepFilterNet 3](https://github.com/Rikorose/DeepFilterNet). The model is
embedded in the binary; nothing leaves your machine.

Esquie reads your real mic, removes background noise, and exposes a virtual
microphone called **"Esquie (noise removed)"**. Select it as the input device in
Discord, Zoom, OBS, your browser, etc.

## Linux compatibility

Esquie targets Linux desktops using PipeWire for audio, with a running session
manager such as WirePlumber. Applications using the PulseAudio API can use the
virtual microphone through PipeWire's `pipewire-pulse` compatibility service.
Standalone PulseAudio, ALSA-only, and JACK-only audio setups are outside the
current scope.

Compatibility work focuses on installation and dependencies across distributions,
Wayland and X11 desktop sessions, operation with or without a system tray, and
audio recovery after device changes, suspend/resume, or PipeWire restarts.
The configured CI matrix covers source builds and AppImage checks on Ubuntu,
Debian, Fedora, and Arch Linux. Container tests cover synthetic audio and
software rendering; they do not establish real desktop or hardware compatibility.
Esquie reconnects after PipeWire restarts and falls back to another microphone
while the selected one is unplugged. After a restart, consumer apps may need to
select the virtual microphone again. See [building and checks](docs/building.md)
for the matrix and [troubleshooting](docs/troubleshooting.md) for help.

## AppImage

`Esquie-<version>-x86_64.AppImage` runs on x86_64 desktops with PipeWire 1.0+
without Rust or development packages. It uses your system's PipeWire library,
plugins, and configuration, and never starts its own audio server.

```sh
sha256sum --check Esquie-0.1.0-x86_64.AppImage.sha256
chmod +x Esquie-0.1.0-x86_64.AppImage
./Esquie-0.1.0-x86_64.AppImage
```

See [running the AppImage](docs/appimage.md) for host prerequisites, menu
integration, manual updates, and what to do when FUSE is unavailable. To build
it yourself, see [building the AppImage](docs/building.md#building-the-appimage).

## Build & run

Install Rust through rustup and the native dependencies for your distribution;
see [build and runtime requirements](docs/building.md) for separate package lists
and the local equivalents of CI checks. The repository pins Rust **1.99.1** and
uses the committed `Cargo.lock`.

```sh
sudo bash ci/install-deps.sh
cargo build --release --locked
cargo test --release --locked
./target/release/esquie --version
./target/release/esquie
```

To render without GPU acceleration, run
`SLINT_BACKEND=winit-software ./target/release/esquie` in a Wayland or X11
session. `--version` prints the package name and version without connecting to
the display, D-Bus, or PipeWire.

## UI

- **Microphone**: which real mic to clean. Until you pick one, it follows the system default.
- **Noise removal**: on/off (off = passthrough, handy for A/B comparison).
- **Strength**: how much noise may be removed. "Max" removes everything it can;
  lower values leave some ambience in and sound more natural.
- **Extra cleanup**: DeepFilterNet's post-filter; removes a bit more leftover
  noise at a small cost to voice fidelity. On by default.
- **Silence between words**: fades to silence when you're not speaking. On by default.
- **Input / Output meters**: raw mic level vs. cleaned level.
- **Settings**: install a per-user copy, choose its directory, or add, repair,
  and remove Esquie-owned menu launchers. Installation keeps audio running;
  switching to the installed copy is explicit. Update controls explain that
  automatic checks and downloads are not available yet.

Settings are saved in `~/.config/esquie/settings`.

## Tray and start on login

With a StatusNotifier tray (KDE, Xfce, GNOME with the AppIndicator extension,
or a panel such as Waybar), closing the window keeps Esquie running in the
tray; quit from the tray menu. Without a tray, closing the window quits. Esquie
notices a tray that starts late, restarts, or goes away: if it is hidden and the
tray is still missing after a few seconds, the window reappears.

**Start on login** writes `~/.config/autostart/esquie.desktop`, which starts
Esquie with `--hidden`. `--hidden` only hides the window while a tray exists to
bring it back from. The entry points at the installed binary, or at the
AppImage file itself (never its temporary mount). An in-app installation
remembers the selected persistent copy, even while the original is still
running. After moving it, select its path again with **Add launcher without
moving the app** in Settings. Turning the option off in the desktop's own
startup settings is respected; external startup entries are preserved.

Compositors that don't process XDG autostart entries (for example sway,
Hyprland, niri, and river without a session manager) need Esquie in their own
startup configuration instead:

```sh
# sway / i3:       exec /path/to/esquie --hidden
# Hyprland:        exec-once = /path/to/esquie --hidden
# niri (KDL):      spawn-at-startup "/path/to/esquie" "--hidden"
```

## How it works

```
real mic ──► esquie-input (capture stream)
                 │  48 kHz mono, 10 ms frames
                 ▼
            high-pass filter (80 Hz, removes rumble/hum)
                 │
                 ▼
            DeepFilterNet 3 (tract, CPU)
                 │
                 ▼
            speech gate (driven by the model's speech estimate)
                 │
                 ▼
            small buffer ──► "esquie" (Audio/Source) ──► your apps
```

- `src/audio.rs`: PipeWire streams, mic discovery, DeepFilterNet processing.
- `src/dsp.rs`: high-pass filter, speech gate, and the developer-tuned constants.
- `src/main.rs`: Slint UI wiring. `ui/app.slint`: the window.
- Added latency is about 40–50 ms; CPU use is about 20% of one core.

## Notes

- You can make "Esquie" your system default mic; Esquie never uses its own
  virtual mic as its input.
- `ESQUIE_LOG=debug` logs buffer underruns (crackles); `ESQUIE_LOG` overrides
  `RUST_LOG`, and the default level is `info`. Logs are kept under
  `${XDG_STATE_HOME:-~/.local/state}/esquie/logs/`. Export recent logs and crash
  details from the Diagnostics card in Settings.
- `cargo test --release --locked` checks that the model removes synthetic noise.

## Help and contributions

Bug reports, feature requests, and testing on different Linux desktops and
microphones are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md).
Before starting a code contribution, open an issue and obtain maintainer
approval for its scope.

## License

Copyright © 2026 Hayk Sahakyan. Esquie's code, documentation, and artwork are
licensed under [GPL-3.0-only](LICENSE). Dependencies and the embedded model retain their own
licenses; see [third-party notices](THIRD_PARTY_NOTICES.md).
