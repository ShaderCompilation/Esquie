# Esquie

Offline, real-time noise removal for your microphone on Linux (PipeWire), using
[DeepFilterNet 3](https://github.com/Rikorose/DeepFilterNet). The model is
embedded in the binary; nothing leaves your machine.

Esquie reads your real mic, removes background noise, and exposes a virtual
microphone called **"Esquie (noise removed)"**. Select it as the input device in
Discord, Zoom, OBS, your browser, etc.

## Build & run

```sh
sudo dnf install pipewire-devel   # Debian/Ubuntu: libpipewire-0.3-dev, plus clang
cargo run --release
```

## UI

- **Microphone**: which real mic to clean. Until you pick one, it follows the system default.
- **Noise removal**: on/off (off = passthrough, handy for A/B comparison).
- **Strength**: how much noise may be removed. "Max" removes everything it can;
  lower values leave some ambience in and sound more natural.
- **Input / Output meters**: raw mic level vs. cleaned level.

Settings are saved in `~/.config/esquie/settings`.

## How it works

```
real mic ──► esquie-input (capture stream)
                 │  48 kHz mono, 10 ms frames
                 ▼
            DeepFilterNet 3 (tract, CPU)
                 │
                 ▼
            small buffer ──► "esquie" (Audio/Source) ──► your apps
```

- `src/audio.rs`: PipeWire streams, mic discovery, DeepFilterNet processing.
- `src/main.rs`: Slint UI wiring. `ui/app.slint`: the window.
- Added latency is about 40–50 ms; CPU use is about 20% of one core.

## Notes

- You can make "Esquie" your system default mic; Esquie never uses its own
  virtual mic as its input.
- `ESQUIE_DEBUG=1` logs buffer underruns (crackles) to stderr.
- `cargo test --release` checks that the model removes synthetic noise.
