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
[Clean builds and automated checks pass across the initial distro matrix](docs/phase1-validation.md).
Esquie waits for PipeWire if it starts first, reconnects after PipeWire or
WirePlumber restarts, and falls back to another microphone while the selected
one is unplugged ([recovery checks](docs/phase2-validation.md)). Desktop and
hardware coverage still needs validation.

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
AppImage file itself (never its temporary mount). After moving the AppImage,
launch it once from the new location to update the entry; turning the option
off in the desktop's own startup settings is respected.

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
- `ESQUIE_DEBUG=1` logs buffer underruns (crackles) to stderr.
- `cargo test --release --locked` checks that the model removes synthetic noise.

## Tuning (developers)

Values that aren't in the UI live in `Tuning` in `src/dsp.rs`. Each can be
overridden with an environment variable for listening tests, e.g.
`ESQUIE_GATE_OPEN_DB=3 cargo run --release --locked`:

| Variable | Default | What it does |
|---|---|---|
| `ESQUIE_HPF_HZ` | 80 | High-pass cutoff (0 = off) |
| `ESQUIE_MIN_DB` / `ESQUIE_MAX_ERB_DB` / `ESQUIE_MAX_DF_DB` | -10 / 30 / 20 | DeepFilterNet's speech thresholds |
| `ESQUIE_PF_BETA` | 0.02 | Post-filter strength ("Extra cleanup") |
| `ESQUIE_GATE_OPEN_DB` | 0 | Speech estimate that opens the gate |
| `ESQUIE_GATE_CONFIRM_FRAMES` | 2 | 10 ms frames in a row needed to open (ignores clicks) |
| `ESQUIE_GATE_HOLD_MS` | 250 | How long it stays open after speech |
| `ESQUIE_GATE_ATTACK_MS` / `ESQUIE_GATE_RELEASE_MS` | 5 / 120 | Fade in / fade out |
| `ESQUIE_GATE_FLOOR_DB` | -50 | How quiet "closed" is |

`cargo test --release --locked tune -- --ignored --nocapture` measures leftover noise and
speech loss on the sample recordings in the DeepFilterNet checkout.
