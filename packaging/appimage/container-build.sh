#!/usr/bin/env bash
# Builds the x86_64 AppImage inside a fresh Ubuntu 24.04 container, the oldest
# supported baseline. Run through packaging/appimage/build.sh; it writes
# target/appimage/Esquie-<version>-x86_64.AppImage, its .sha256, and build
# evidence.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD
here=$repo/packaging/appimage

source /etc/os-release
if [[ $ID != ubuntu || $VERSION_ID != 24.04 ]]; then
    echo "The release AppImage is built on Ubuntu 24.04, not $PRETTY_NAME" >&2
    exit 1
fi

out=$repo/target/appimage
work=$out/work
rm -rf "$out"
mkdir -p "$work/tools" "$out/evidence"

bash ci/install-deps.sh
# appimagetool identifies the payload's architecture with file(1).
apt-get install -y --no-install-recommends file
cat /etc/os-release > "$out/evidence/os-release.txt"
dpkg-query -W > "$out/evidence/packages.txt"

# Pinned tools, all checked against tools.sha256 before anything runs.
fetch() { curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$work/tools/$2" "$1"; }
fetch https://static.rust-lang.org/rustup/archive/1.29.3/x86_64-unknown-linux-gnu/rustup-init rustup-init
fetch https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage linuxdeploy-x86_64.AppImage
fetch https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage appimagetool-x86_64.AppImage
fetch https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64 runtime-x86_64
grep -v '^#' "$here/tools.sha256" | (cd "$work/tools" && sha256sum --check --strict)
cp "$here/tools.sha256" "$out/evidence/tools.sha256"
chmod +x "$work/tools/"*

"$work/tools/rustup-init" -y --no-modify-path --profile minimal --default-toolchain none
export PATH="$HOME/.cargo/bin:$PATH"
export RUSTFLAGS="-C target-cpu=x86-64"
# Kept apart from target/release, which may hold host-built artifacts whose
# PipeWire bindings and linked libraries came from a different distro.
export CARGO_TARGET_DIR=$repo/target/appimage-build
rustc -Vv > "$out/evidence/rustc.txt"
cargo -V > "$out/evidence/cargo.txt"
pkg-config --modversion libpipewire-0.3 > "$out/evidence/pipewire-headers.txt"
cargo build --release --locked
cp Cargo.lock "$out/evidence/Cargo.lock"

bin=$CARGO_TARGET_DIR/release/esquie
version=$("$bin" --version | awk '{print $2}')
cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ $version != "$cargo_version" ]]; then
    echo "Binary reports $version but Cargo.toml says $cargo_version" >&2
    exit 1
fi

# The packaging tools are AppImages themselves; containers have no FUSE, so
# extract them instead of mounting.
for tool in linuxdeploy appimagetool; do
    (cd "$work/tools" && "./$tool-x86_64.AppImage" --appimage-extract >/dev/null \
        && mv squashfs-root "$tool")
done

# The launcher entry comes from the binary so it matches what install.sh and
# autostart write; inside the AppImage it runs the bundled executable by name.
"$bin" --print-launcher-entry | sed 's|^Exec=.*|Exec=esquie|' > "$work/esquie.desktop"
cp ui/icon.svg "$work/esquie.svg"

appdir=$work/AppDir
# linuxdeploy bundles the binary's library dependencies except those on its
# standard excludelist (glibc, the loader, libgcc_s, fontconfig, X11/Wayland,
# GL/EGL drivers, ...). PipeWire must come from the host: its client library
# loads the host's SPA plugins and reads the host's configuration, which a
# bundled copy could mismatch. libpng, bzip2 and Brotli are only needed by the
# host's (excluded) FreeType, which resolves them from the host; bundled copies
# would never be loaded.
"$work/tools/linuxdeploy/AppRun" \
    --appdir "$appdir" \
    --executable "$bin" \
    --desktop-file "$work/esquie.desktop" \
    --icon-file "$work/esquie.svg" \
    --exclude-library 'libpipewire-0.3.so*' \
    --exclude-library 'libpng16.so*' \
    --exclude-library 'libbz2.so*' \
    --exclude-library 'libbrotli*.so*' \
    2>&1 | tee "$out/evidence/linuxdeploy.log"

appimage=$out/Esquie-$version-x86_64.AppImage
ARCH=x86_64 "$work/tools/appimagetool/AppRun" --no-appstream \
    --runtime-file "$work/tools/runtime-x86_64" \
    "$appdir" "$appimage" 2>&1 | tee "$out/evidence/appimagetool.log"
chmod 755 "$appimage"

bash "$here/inspect.sh" "$appimage" | tee "$out/evidence/inspect.txt"
(cd "$out" && sha256sum "$(basename "$appimage")" > "$(basename "$appimage").sha256")
cat "$appimage.sha256"
