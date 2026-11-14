# Building and checking Esquie on Linux

CI builds and tests on Ubuntu 24.04 and current Arch Linux. The dependency
installer below also supports Ubuntu 26.04, Debian 13, and Fedora 44/45 for
local builds, but those distros are no longer CI targets. A passing or
configured job is not evidence that a distro or a real desktop session has
passed validation.

## Toolchain and dependency resolution

Install [rustup](https://rustup.rs/) first. `rust-toolchain.toml` selects Rust
**1.99.1** with the minimal profile. Cargo automatically installs that exact
toolchain when needed. All build, test, and installer commands use `--locked`
with the committed `Cargo.lock`, including the locked DeepFilterNet Git revision.
Keep the lockfile in version control; update it deliberately when changing
dependencies. Release builds use generic x86_64 instructions; do not set
`target-cpu=native`. CI explicitly sets `RUSTFLAGS="-C target-cpu=x86-64"`.

The PipeWire crate retains its `v1_0_0` feature baseline: native headers and the
runtime must be PipeWire **1.0 or newer**, despite the `0.3` in library/package
names. Slint defaults are disabled. The selected features provide Winit with
Wayland and X11, FemtoVG/OpenGL, software rendering (including the UI's path
icons), and AccessKit accessibility. Qt, Skia, and a GPU are not build
requirements. See Slint's [feature reference](https://docs.slint.dev/latest/docs/rust/slint/docs/cargo_features/)
and [Winit renderer documentation](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/).

## Build dependencies

In addition to rustup, install the following native tools and development
packages. The dependency installer also installs the client runtime libraries
from the next section so the result can run and be tested locally.

| Family | Build tools | Development packages |
| --- | --- | --- |
| Ubuntu 24.04/26.04, Debian 13 | `build-essential pkg-config clang libclang-dev git curl ca-certificates` | `libpipewire-0.3-dev libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libfontconfig1-dev` |
| Fedora 44/45 | `gcc gcc-c++ make pkgconf-pkg-config clang clang-libs git curl-minimal ca-certificates` | `pipewire-devel libxkbcommon-devel wayland-devel libX11-devel libxcb-devel fontconfig-devel` |
| Arch Linux | `base-devel pkgconf clang git curl ca-certificates` | `pipewire libxkbcommon wayland libx11 libxcb` (Arch includes headers in its library packages) |

Clang/libclang is needed by bindgen for PipeWire/SPA bindings. These are
source-build dependencies; users running a built binary do not need Rust,
compilers, development headers, or pkg-config.
Fedora's `curl-minimal` provides the HTTPS support used here without conflicting
with the minimal container image's installed curl package.

From the repository root, on one of the listed distros:

```sh
sudo bash ci/install-deps.sh
cargo build --release --locked
cargo test --release --locked
./target/release/esquie --version
./target/release/esquie
```

Arch's installer uses a full `pacman -Syu` to avoid partial upgrades.
The existing `./install.sh` also builds with `--release --locked` before
installing for the current user.

## Runtime requirements

Normal operation requires a Wayland compositor or X11 server, a user D-Bus
session, PipeWire 1.0+, and a running session manager (tested target:
WirePlumber). PulseAudio clients need `pipewire-pulse`. A StatusNotifier tray
is optional. These services must belong to the logged-in desktop user; building
and unit testing do not require a running audio server.

Install the packages matching the desktop's display protocol. The installer
includes client libraries for both protocols. Standard glibc and C/C++ runtime
libraries are supplied by each distro's dependency resolver.

| Family | Audio services and client library | Display, text, and fonts | OpenGL renderer |
| --- | --- | --- | --- |
| Ubuntu 24.04/26.04, Debian 13 | `pipewire wireplumber pipewire-pulse libpipewire-0.3-0t64` | `libwayland-client0 libwayland-cursor0 libwayland-egl1 libx11-6 libx11-xcb1 libxcb1 libxcursor1 libxi6 libxrandr2 libxinerama1 libxkbcommon0 libxkbcommon-x11-0 fontconfig libfreetype6 fonts-dejavu-core` | `libegl1 libgl1` and the appropriate graphics driver |
| Fedora 44/45 | `pipewire pipewire-libs wireplumber pipewire-pulseaudio` | `libwayland-client libwayland-cursor libwayland-egl libX11 libX11-xcb libxcb libXcursor libXi libXrandr libXinerama libxkbcommon libxkbcommon-x11 fontconfig freetype dejavu-sans-fonts` | `mesa-libEGL mesa-libGL` and the appropriate graphics driver |
| Arch Linux | `pipewire pipewire-audio wireplumber pipewire-pulse` | `wayland libx11 libxcb libxcursor libxi libxrandr libxinerama libxkbcommon libxkbcommon-x11 fontconfig freetype2 ttf-dejavu` | `libglvnd mesa` or the appropriate graphics driver |

Use the existing desktop's D-Bus installation (`dbus-user-session` on
Debian/Ubuntu; `dbus` on Fedora/Arch). Development package installation in CI
does not start or replace the host's audio services.

Force software rendering, with no GPU acceleration required:

```sh
SLINT_BACKEND=winit-software ./target/release/esquie
```

This still requires a graphical desktop and PipeWire for normal operation.
`./target/release/esquie --version` is the early-exit exception: it prints
`esquie 0.1.1` (from Cargo package metadata) without loading settings or
initializing the display, tray, D-Bus, or audio engine. Installed shared
libraries are still required by the executable's dynamic loader.

## Automated checks and local CI reproduction

The workflow runs independent fresh-container jobs for both matrix entries
(Ubuntu 24.04 and Arch Linux). Jobs restore GitHub Actions caches of the
pinned Rust toolchain, Cargo downloads, and per-distro build artifacts; the
distro containers themselves are always fresh, so native dependencies are
installed and the checks run from scratch in each image.

Each job installs native dependencies, installs the pinned Rust compiler, runs
a locked release build, runs the existing automated tests plus the headless
version regression test, the tray availability regression on a private D-Bus
session, the audio integration suite below, and renders the production Slint UI
with Winit's software renderer. The display check uses Xvfb with GLX disabled, an isolated
D-Bus session, and a 30-second deadline; it checks a nonblank rendered snapshot
without starting the audio engine or tray. Containers receive no GPU devices.

To run those same checks on an existing development machine:

```sh
sudo bash ci/install-deps.sh --ci
bash ci/check.sh
```

`--ci` adds test infrastructure: Xvfb, a D-Bus session daemon, Python 3, and the
distro's PipeWire utilities and WirePlumber (`pipewire-audio` on Arch).
The existing ignored audio tuning tests remain opt-in and need the sample
recordings in the developer's DeepFilterNet checkout.

To reproduce a clean CI job locally with Docker (or substitute `podman`):

```sh
docker pull --platform linux/amd64 ubuntu:24.04
image_digest=$(docker image inspect --format '{{index .RepoDigests 0}}' ubuntu:24.04)
# Use a fresh checkout for each distro.
docker run --rm --platform linux/amd64 \
  --env CARGO_BUILD_JOBS=4 \
  --volume "$PWD:/workspace" --workdir /workspace \
  "$image_digest" bash ci/container-build.sh
```

On an SELinux host, add `:Z` to the volume mount for a dedicated checkout.
The same snippet also runs with `ubuntu:26.04`, `debian:13`, `fedora:44`, and
`fedora:45`; the installer supports them even though CI no longer tests them.
Resolve each tag to a digest before use. CI jobs upload `target/validation`
with that image digest,
the installed package versions, compiler/Cargo versions, PipeWire header
version, check log, lockfile, version output, and binary checksum.

The distro repositories intentionally track supported updates (and Arch's
current packages). These records identify a validation environment; they do
not promise byte-identical builds or pin OS package repositories to snapshots.
Wayland rendering, accessibility, real hardware audio, and desktop/hardware checks are
separate checks; synthetic audio and virtual displays do not establish hardware
compatibility, speech quality, latency, or GPU behavior.

The `ui_flow` integration test also renders the production UI directly with
Slint's software renderer, without a display or audio services. It exercises
navigation, setup, preset callbacks, the two-column workspace and its narrow
layout (including the 760 px breakpoint), recording/listening/comparison layouts,
momentary bypass release on navigation/hide/minimize, meter polling, scrolling,
dark/light themes, and display scaling.
Pointer checks find controls by their accessible names and click their rendered
bounds, so distro font metrics do not determine the click coordinates. The build
generates a separate UI module with Slint test metadata; production uses the
module without that metadata. The test helper crate is pinned to the locked
Slint version and must be updated with it.
`ci/check.sh` saves its screenshots under `target/validation/ui`. To run it alone:

```sh
ESQUIE_UI_OUTPUT_DIR=target/validation/ui cargo test --release --locked --test ui_flow
```

These checks cover simulated UI input and rendering, not screen-reader behavior
or real desktop audio.

### Isolated audio integration

```sh
cargo build --release --locked --example audio_harness
python3 ci/audio-integration.py
```

`ci/check.sh` first runs the Python graph-parser regression tests. The audio
suite applies `pw-dump` snapshots and removal records in order, including the
separate JSON arrays emitted by older PipeWire versions during graph changes.

The audio integration suite is a required part of `ci/check.sh`. The Python
standard library is sufficient; no pip packages are needed. The example calls
the production audio engine and exposes a test-only JSON command/event protocol.
The test creates private PipeWire/WirePlumber instances and synthetic mono
sources, supplies a known tone from fixed-format harness source processes, and
records two native consumers. Session-manager tests recreate those sources to model hardware
monitor device re-enumeration. It tests recovery, preference
restoration, metadata replacement, error persistence, preview separation,
recording/meter cleanup, and retry cancellation. Demand checks verify that
capture continues while model-hop counts stop, that same-name consumers and
non-application routes wake processing, and that preview and recovery transitions
release or retain demand correctly. Repeated attachment trials require first
observed output within 200 ms, at least 90 ms of a 100 ms synthetic onset, and
old-tone correlation below 0.005 amplitude. They use 6 dB attenuation with the
gate disabled and a quiet pilot to advance the model after the burst; this is
synthetic timing/queue coverage, not speech-quality or first-word validation.
The suite verifies actual graph rates/quanta using `pw-top` and finite output
with the cleaning engine enabled.
The engine retains PipeWire loop ownership between polls, re-registers its
streams when session-manager metadata disappears, and flushes queued source
buffers on pause and new-reader transitions. These paths are exercised by the
consumer, restart, and onset scenarios above.
Graph recovery waits are limited to ten seconds and Quit to one second. The
recording-limit scenario allows twelve seconds to capture a full ten-second
sample and checks that playback starts only after an explicit Play command.

The suite whitelists its environment, isolates runtime/config/state/cache/data,
disables hardware monitors, and never invokes `systemctl` or connects to desktop
audio sockets. It requires permission to bind Unix sockets; run outside a
sandbox that forbids socket creation. `--harness PATH` selects a different build
directory and `--out PATH` selects the evidence directory (default:
`target/validation/audio`). A failed or interrupted check must not be treated
as a pass; review the overall status, all scenario results, and command exit
status. Results apply only to the tested environment and scenario.

## Building the AppImage

For opt-in CPU, memory, background-state, and startup measurements, see
[Measuring resource usage](performance.md). These measurements use private audio
services and are separate from the automated correctness checks above.

The release AppImage is built in a clean Ubuntu 24.04 container, the oldest
supported baseline, so it runs on newer glibc versions. It needs Podman or
Docker on the host (set `CONTAINER_ENGINE` to choose one):

```sh
bash packaging/appimage/build.sh
bash packaging/appimage/test.sh            # both CI matrix images
bash packaging/appimage/test.sh fedora:45  # or selected images
```

`build.sh` pulls `ubuntu:24.04`, records its digest, and runs
`packaging/appimage/container-build.sh` in it. That script installs the build
dependencies above and downloads pinned tools, checking each against
`packaging/appimage/tools.sha256` before running any of them:

| Tool | Pinned release |
| --- | --- |
| rustup-init | 1.29.3 (then the toolchain from `rust-toolchain.toml`) |
| linuxdeploy | `1-alpha-20251107-1` |
| appimagetool | `1.9.1` |
| AppImage type 2 runtime | `20251108` |

It then builds with `--release --locked` and `-C target-cpu=x86-64` into
`target/appimage-build` (kept separate from host builds; delete it for a
from-scratch build), lets linuxdeploy assemble the AppDir from the binary, the
generated launcher entry (`Exec=esquie`), and `ui/icon.svg`, and packs it with
appimagetool and the pinned runtime. Results:

- `target/appimage/Esquie-<version>-x86_64.AppImage` and its `.sha256`
- `target/appimage/evidence/`: image digest, OS release, installed packages,
  compiler and PipeWire header versions, lockfile, tool checksums, linuxdeploy
  and appimagetool logs, and the payload inspection

linuxdeploy bundles any library dependency that is not on its standard
excludelist (glibc and the loader, libgcc_s, fontconfig/FreeType, X11/Wayland,
and GL/EGL drivers stay on the host). `libpipewire-0.3.so.0` is excluded
explicitly: the host's copy must load the host's SPA plugins and read the host's
configuration. The current binary's only other dependencies are on the
excludelist, so the payload includes the executable, desktop entry, icon, and
license documentation under `usr/share/doc/esquie/`. Inspect the built payload
when dependencies change; additional bundled libraries need their own notices.
`packaging/appimage/inspect.sh` fails the build if the AppImage is not type 2,
bundles PipeWire, its plugins or configuration, an audio service, glibc, the
loader, or graphics drivers, changes the environment through `AppRun`, or no
longer links the host's `libpipewire-0.3.so.0`.

`test.sh` checks the `.sha256`, then runs the AppImage in a fresh container per
image with only runtime packages installed (the script fails if Rust, a C
compiler, pkg-config, or PipeWire headers are present). Only the AppImage and
the test script are mounted. Each run checks `--version`, the launcher entry's
path, and manual extraction; then it starts the distro's own PipeWire and
WirePlumber under a private D-Bus session, adds a synthetic microphone, and runs
the AppImage on Xvfb with the software renderer. It passes when Esquie's
virtual microphone appears and the synthetic microphone is linked to its capture
stream, with `libpipewire` and SPA plugins loaded from the host and no
PipeWire variables added to Esquie's environment. Containers have no FUSE, so
each run exercises both `APPIMAGE_EXTRACT_AND_RUN=1` and the manually extracted
`AppRun` through live GUI/audio startup; FUSE mounting is checked on a host.
Logs, package lists, PipeWire versions, and the loaded library paths are written
to `target/appimage/test/<image>/`.

GitHub Actions builds one AppImage candidate on Ubuntu 24.04, uploads it with
its checksum and build evidence, and tests that same artifact in both
runtime-only matrix containers on pushes to master, manual dispatches, and
tagged releases; pull requests build the candidate but skip runtime testing.
The aggregate **All automated release gates** job also requires both source
build/test jobs. Candidate uploads do not publish a
release. Automated results do not replace real desktop, microphone, Bluetooth,
login, and suspend/resume testing.

### What CI prepares for a release

The workflow runs on pushes to master, pull requests, or manual dispatch;
changes that touch only documentation skip the build, test, and packaging
jobs, and the gates job reports success for them. Its repository
permissions are read-only: it uploads review artifacts, but does not create a
tag, publish a GitHub Release, or upload public release assets. A local build
or test pass is separate from a passing Actions run for the release commit.

The **All automated release gates** job requires both source build/test
jobs, the AppImage build and payload inspection, and — except on pull
requests — both runtime jobs to succeed. Failed or cancelled dependencies
prevent it from passing; intentionally skipped jobs (documentation-only
changes, runtime tests on pull requests) do not.

Packaging generates dependency license reports with pinned `cargo-about`,
checks reviewed license-input checksums and required copyright supplements,
and includes notices and `Cargo.lock` in the AppImage. Payload inspection
checks that the required notice files are present. The evidence records the
build environment, tool checksums, dependency inventory, and inspection output.

These checks do not prepare or verify a complete corresponding-source package.
CI downloads a checksum-pinned prebuilt AppImage runtime; it does not build
that runtime from source, resolve its incorporated Alpine library revisions,
or collect and test its source/relinking materials. The license-input source
URLs identify notice snapshots, not a complete runtime source package.

Before binary publication, separately prepare matching Esquie source, locked
dependency/model sources, build scripts, and runtime/library source and
relinking materials, with clear download instructions beside the binary.
Use tracked files from the release commit for the Esquie source archive;
do not archive the entire working directory. A repository source archive alone
does not collect external dependency or runtime sources. See
[third-party notices](../THIRD_PARTY_NOTICES.md#finding-the-texts-and-source).

To update a pinned tool, change its URL in `container-build.sh` and its line in
`tools.sha256`, using the digest published with the release, then rebuild and
rerun the tests.

### Signing and publishing a release

Pushing a `v<version>` tag runs the **Tagged release** workflow. It builds and
tests the AppImage and creates a **draft** GitHub Release with the AppImage and
its `.sha256`. Drafts are invisible to the update check. CI never holds the
signing key. To publish, run this on the maintainer's machine:

```sh
packaging/release/sign.sh v<version>
```

The script downloads the draft's files and checks the checksum. It then signs
the `.sha256` with the minisign secret key (`~/.minisign/esquie.key`, or
`ESQUIE_SECRET_KEY`), using the trusted comment `file:<checksum name>`. It
verifies the signature against
[`packaging/release/esquie.pub`](../packaging/release/esquie.pub), uploads the
`.minisig`, and publishes after you confirm. Esquie compiles that public key in.
It offers an update only when the signature verifies with that key and its
trusted comment names exactly that release's checksum file. So controlling the
GitHub repository is not enough to push an update.

Key care:

- Keep the secret key password-protected and off shared machines and CI, with
  an offline backup.
- If the key is lost, released builds cannot verify new updates. Generate a new
  key, commit its public file, and ask users to install the next release by
  hand.
- To rotate a key that is still available, ship one release signed with the old
  key that embeds the new public key. Sign everything after that with the new
  key.
- If the key leaks, rotate immediately. Builds that still embed the old key
  would accept updates signed with it.

## Audio tuning

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

## License documentation

The AppImage build installs pinned `cargo-about` **0.9.3** and generates its
license bundle automatically. To generate the same documentation locally:

```sh
cargo install --locked --version 0.9.3 --features cli cargo-about
python3 packaging/licenses/generate.py
```

This requires Python 3 and uses `about.toml`, the committed template and reviewed
license inputs in `packaging/licenses/`, and the existing `Cargo.lock`.
Output is ignored under `target/licenses/`; do not commit generated reports.
`--offline` works when the locked crate sources are already cached.
Generation fails on unresolved licenses, missing upstream copyright supplements,
or changed input checksums. Build dependencies are included; dependencies used
only by tests are excluded. Review notices when changing dependencies or runtime
pins, including any new files that linuxdeploy adds to the payload.

The AppImage includes these texts and its lockfile in `usr/share/doc/esquie/`.
Payload inspection requires them. See [third-party notices](../THIRD_PARTY_NOTICES.md)
for the embedded model, AppImage runtime, and corresponding-source requirements.

## Audio pipeline and source layout

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

For resource-usage measurement methods and their limitations, see
[Measuring resource usage](performance.md).
