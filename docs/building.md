# Building and checking Esquie on Linux

The initial x86_64 matrix is Ubuntu 24.04 and 26.04 LTS, Debian 13, Fedora 44
and 45, and current Arch Linux. These are CI targets; a configured job is not
evidence that the distro or a real desktop session has passed validation.
Local clean-container results are recorded in [Phase 1 validation](phase1-validation.md).

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
| Arch Linux | `pipewire wireplumber pipewire-pulse` | `wayland libx11 libxcb libxcursor libxi libxrandr libxinerama libxkbcommon libxkbcommon-x11 fontconfig freetype2 ttf-dejavu` | `libglvnd mesa` or the appropriate graphics driver |

Use the existing desktop's D-Bus installation (`dbus-user-session` on
Debian/Ubuntu; `dbus` on Fedora/Arch). Development package installation in CI
does not start or replace the host's audio services.

Force software rendering, with no GPU acceleration required:

```sh
SLINT_BACKEND=winit-software ./target/release/esquie
```

This still requires a graphical desktop and PipeWire for normal operation.
`./target/release/esquie --version` is the early-exit exception: it prints
`esquie 0.1.0` (from Cargo package metadata) without loading settings or
initializing the display, tray, D-Bus, or audio engine. Installed shared
libraries are still required by the executable's dynamic loader.

## Automated checks and local CI reproduction

The workflow runs independent clean-container jobs for all six matrix entries.
Each job installs native dependencies, installs the pinned Rust compiler, runs
a locked release build, runs the existing automated tests plus the headless
version regression test, and renders the production Slint UI with Winit's
software renderer. The display check uses Xvfb with GLX disabled, an isolated
D-Bus session, and a 30-second deadline; it checks a nonblank rendered snapshot
without starting the audio engine or tray. Containers receive no GPU devices.

To run those same checks on an existing development machine:

```sh
sudo bash ci/install-deps.sh --ci
bash ci/check.sh
```

`--ci` adds only test infrastructure: `xvfb dbus-daemon` on Debian/Ubuntu,
`xorg-x11-server-Xvfb dbus-daemon` on Fedora, or `xorg-server-xvfb dbus` on Arch.
The existing ignored audio tuning tests remain opt-in and need the sample
recordings in the developer's DeepFilterNet checkout.

To reproduce a clean CI job locally with Docker (or substitute `podman`):

```sh
docker pull --platform linux/amd64 ubuntu:24.04
image_digest=$(docker image inspect --format '{{index .RepoDigests 0}}' ubuntu:24.04)
# Use a fresh checkout for each distro.
docker run --rm --platform linux/amd64 \
  --env CARGO_BUILD_JOBS=2 \
  --volume "$PWD:/workspace" --workdir /workspace \
  "$image_digest" bash ci/container-build.sh
```

On an SELinux host, add `:Z` to the volume mount for a dedicated checkout.
Other matrix image tags are `ubuntu:26.04`, `debian:13`, `fedora:44`,
`fedora:45`, and `archlinux:base`. CI resolves each tag once and runs the
resulting digest. Each job uploads `target/validation` with that image digest,
the installed package versions, compiler/Cargo versions, PipeWire header
version, check log, lockfile, version output, and binary checksum.

The distro repositories intentionally track supported updates (and Arch's
current packages). These records identify a validation environment; they do
not promise byte-identical builds or pin OS package repositories to snapshots.
Wayland rendering, accessibility, real audio, and desktop/hardware checks are
separate validation work described in the compatibility plan.
