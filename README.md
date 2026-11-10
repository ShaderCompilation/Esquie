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
- **Extra cleanup**: DeepFilterNet's post-filter; removes a bit more leftover
  noise at a small cost to voice fidelity. On by default.
- **Silence between words**: fades to silence when you're not speaking. On by default.
- **Input / Output meters**: raw mic level vs. cleaned level.

Settings are saved in `~/.config/esquie/settings`.

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
- `cargo test --release` checks that the model removes synthetic noise.

## Tuning (developers)

Values that aren't in the UI live in `Tuning` in `src/dsp.rs`. Each can be
overridden with an environment variable for listening tests, e.g.
`ESQUIE_GATE_OPEN_DB=3 cargo run --release`:

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

`cargo test --release tune -- --ignored --nocapture` measures leftover noise and
speech loss on the sample recordings in the DeepFilterNet checkout.
