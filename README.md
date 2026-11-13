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

- **Quick setup**: new users choose a microphone, optionally check their sound,
  then select **Esquie (noise removed)** in their calling or recording app.
  Reopen the guide from **Settings → Run microphone setup**.
- **Microphone**: choose the real mic to clean. Until you pick one, it follows
  the system default. Input and output meters show the original and outgoing
  levels. The status explains whether Esquie is ready, in use by an app,
  passing through original audio, or recovering from an audio problem.
- **Noise removal**: on/off. Off passes your microphone through unchanged.
- **Cleanup strength**: choose Natural, Balanced, or Maximum. Maximum removes
  as much noise as it can; lower strengths leave more room sound. Existing
  preferences and the Maximum default are preserved.
- **Fine-tune sound**: opens the precise strength slider, **Extra cleanup**
  (suppresses leftover noise, at a possible cost to voice fidelity), and
  **Silence between words** (quiets pauses). Both switches are on by default.
- **Sound check**: stays beside the microphone and cleanup controls in the wider
  workspace, or below them in narrow windows. Live input/output meters stay
  visible while testing; they show microphone levels, not recorded playback.
  Record a sample, then explicitly play it and switch
  between **Original** and **Cleaned**. Recordings stop after ten seconds;
  stopping keeps the sample without playing it automatically. Fine-tuning
  applies to Cleaned playback. **Listen live** is a separate mode; use headphones
  and hold **Hear original** to compare, or hold Space/Enter while that control
  is focused. Opening Settings, hiding, or minimizing the window stops the test.
  Hearing the original in live comparison affects only your headphone preview;
  connected apps keep receiving the audio selected by the Noise removal switch.
- **Settings**: install a per-user copy, choose its directory, or add, repair,
  and remove Esquie-owned menu launchers. Installation keeps audio running;
  switching to the installed copy is explicit. About and updates explains that
  automatic checks and downloads are not available yet.

The header stays visible while pages scroll. Use **Done** or **Back**, or press
Escape, to return. Tab moves between controls; Page Up/Down and Home/End scroll
the page when a focused control does not use those keys itself.
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
