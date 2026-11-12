#!/usr/bin/env bash
# Runs a built AppImage in a fresh distro container that has only runtime
# packages: no Rust, compilers, or development headers. The distro's own
# PipeWire and WirePlumber run privately inside the container; the AppImage must
# connect to them, publish its virtual microphone, and capture a synthetic mic.
#
# Usage (as root, inside the container): bash container-test.sh APPIMAGE OUT_DIR
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "Usage: bash container-test.sh APPIMAGE OUT_DIR" >&2
    exit 2
fi
src=$(readlink -f "$1")
out=$2
mkdir -p "$out"
echo '{"kind":"automated","result":"failed"}' > "$out/status.json"
fail() { echo "FAIL: $*" >&2; exit 1; }

source /etc/os-release
cat /etc/os-release > "$out/os-release.txt"
case "$ID" in
    ubuntu|debian)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update
        apt-get install -y --no-install-recommends \
            pipewire pipewire-bin wireplumber libpipewire-0.3-0t64 \
            libwayland-client0 libwayland-cursor0 libwayland-egl1 libx11-6 libx11-xcb1 \
            libxcb1 libxcursor1 libxi6 libxrandr2 libxinerama1 libxkbcommon0 \
            libxkbcommon-x11-0 fontconfig libfreetype6 fonts-dejavu-core libegl1 libgl1 \
            xvfb dbus-daemon
        dpkg-query -W > "$out/packages.txt"
        ;;
    fedora)
        dnf install -y --setopt=install_weak_deps=False \
            pipewire pipewire-libs pipewire-utils wireplumber \
            libwayland-client libwayland-cursor libwayland-egl libX11 libX11-xcb libxcb \
            libXcursor libXi libXrandr libXinerama libxkbcommon libxkbcommon-x11 \
            fontconfig freetype dejavu-sans-fonts mesa-libEGL mesa-libGL \
            xorg-x11-server-Xvfb dbus-daemon
        rpm -qa | sort > "$out/packages.txt"
        ;;
    arch)
        pacman -Syu --noconfirm --needed \
            pipewire pipewire-audio wireplumber wayland libx11 libxcb libxcursor libxi libxrandr \
            libxinerama libxkbcommon libxkbcommon-x11 fontconfig freetype2 ttf-dejavu \
            libglvnd mesa xorg-server-xvfb dbus
        pacman -Q > "$out/packages.txt"
        ;;
    *) fail "unsupported distro $ID" ;;
esac

# The point of the AppImage: none of the build toolchain is present.
for tool in cargo rustc rustup cc gcc clang pkg-config; do
    ! command -v "$tool" >/dev/null || fail "$tool is installed"
done
# Arch ships headers inside its runtime pipewire package; elsewhere they come
# only from the -dev/-devel package.
if [[ $ID != arch && -e /usr/include/pipewire-0.3 ]]; then
    fail "PipeWire development headers are installed"
fi
host_pw=$(find /usr/lib /usr/lib64 -name 'libpipewire-0.3.so.0' -print -quit 2>/dev/null || true)
[[ -n $host_pw ]] || fail "host libpipewire-0.3.so.0 not found"
pipewire --version > "$out/pipewire.txt"

# A path with a space, as users' download folders often have.
apps="/opt/My Apps"
mkdir -p "$apps"
appimage="$apps/$(basename "$src")"
install -m 755 "$src" "$appimage"
expected=$(sed -n 's/^Esquie-\(.*\)-x86_64\.AppImage$/esquie \1/p' <<<"$(basename "$src")")
[[ -n $expected ]] || fail "unexpected AppImage name $(basename "$src")"

test_home=$(mktemp -d)
export HOME=$test_home XDG_CONFIG_HOME=$test_home/config XDG_STATE_HOME=$test_home/state XDG_DATA_HOME=$test_home/data
export XDG_RUNTIME_DIR=$test_home/run
mkdir -m 700 "$XDG_RUNTIME_DIR"

# Containers have no FUSE: the documented fallbacks are extract-and-run and
# a manual extraction.
export APPIMAGE_EXTRACT_AND_RUN=1
version=$("$appimage" --version)
[[ $version == "$expected" ]] || fail "--version printed '$version', expected '$expected'"
echo "$version" > "$out/version.txt"

entry=$("$appimage" --print-launcher-entry)
grep -qxF "Exec=\"$appimage\"" <<<"$entry" \
    || fail "launcher entry does not run the AppImage file:"$'\n'"$entry"

extract=$(mktemp -d)
(cd "$extract" && "$appimage" --appimage-extract >/dev/null)
[[ $(env -u APPIMAGE_EXTRACT_AND_RUN "$extract/squashfs-root/AppRun" --version) == "$expected" ]] \
    || fail "extracted AppRun --version failed"

# Live check against the distro's PipeWire, under a private D-Bus session.
cat > "$test_home/live.sh" <<'LIVE'
set -euo pipefail
out=$1 appimage=$2
fail() { echo "FAIL: $*" >&2; exit 1; }
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

pipewire >"$out/pipewire.log" 2>&1 & pids+=($!)
for _ in {1..100}; do [[ -S $XDG_RUNTIME_DIR/pipewire-0 ]] && break; sleep 0.1; done
[[ -S $XDG_RUNTIME_DIR/pipewire-0 ]] || fail "PipeWire did not start"
wireplumber >"$out/wireplumber.log" 2>&1 & pids+=($!)
pw-loopback -c 1 -m '[MONO]' --capture-props='node.name=testmic-feed node.autoconnect=false' \
    --playback-props='media.class=Audio/Source node.name=testmic node.description=Test priority.session=2500' \
    >"$out/loopback.log" 2>&1 & pids+=($!)

Xvfb -displayfd 3 -screen 0 1280x1024x24 -extension GLX -nolisten tcp -ac \
    3>"$out/display" >"$out/xvfb.log" 2>&1 & pids+=($!)
for _ in {1..100}; do [[ -s $out/display ]] && break; sleep 0.1; done
[[ -s $out/display ]] || fail "Xvfb did not start"

DISPLAY=":$(cat "$out/display")" SLINT_BACKEND=winit-software \
    "$appimage" >"$out/esquie.log" 2>&1 & pids+=($!)

esquie_pid() {
    local p
    for p in /proc/[0-9]*; do
        [[ $(readlink "$p/exe" 2>/dev/null) == */usr/bin/esquie ]] && { echo "${p#/proc/}"; return; }
    done
}
ok=
for _ in {1..300}; do
    nodes=$(pw-cli ls Node 2>/dev/null || true)
    links=$(pw-link -l 2>/dev/null || true)
    if grep -q 'node.name = "esquie"' <<<"$nodes" \
        && grep -A4 '^testmic:' <<<"$links" | grep -q 'esquie-input:'; then
        ok=1; break
    fi
    sleep 0.1
done
pw-cli ls Node > "$out/nodes.txt" 2>&1 || true
pw-link -l > "$out/links.txt" 2>&1 || true
[[ -n $ok ]] || fail "Esquie did not publish its virtual mic and capture testmic within 30 s"

pid=$(esquie_pid)
[[ -n $pid ]] || fail "esquie process not found"
grep -o '/[^ ]*libpipewire-0\.3\.so[^ ]*' "/proc/$pid/maps" | sort -u > "$out/libpipewire-loaded.txt"
[[ -s $out/libpipewire-loaded.txt ]] || fail "libpipewire not loaded"
! grep -q appimage "$out/libpipewire-loaded.txt" || fail "libpipewire loaded from the AppImage"
grep -o '/[^ ]*spa-0\.2/[^ ]*' "/proc/$pid/maps" | sort -u > "$out/spa-plugins-loaded.txt" || true
! grep -q appimage "$out/spa-plugins-loaded.txt" || fail "SPA plugins loaded from the AppImage"
tr '\0' '\n' < "/proc/$pid/environ" | grep -E '^(PIPEWIRE|SPA|WIREPLUMBER)_' > "$out/pipewire-env.txt" || true
[[ ! -s $out/pipewire-env.txt ]] || fail "AppImage set PipeWire environment variables"
echo "live check passed (pid $pid)"
LIVE
# Exercise both supported no-FUSE paths through live GUI/audio startup, not
# just --version. The same runtime-only container and host libraries are used.
for mode in extract-and-run extracted; do
    mode_out="$out/$mode"
    mkdir -p "$mode_out"
    if [[ $mode == extracted ]]; then
        payload="$extract/squashfs-root/AppRun"
    else
        payload="$appimage"
    fi
    dbus-run-session -- timeout 90s bash "$test_home/live.sh" "$mode_out" "$payload"
done
rm -rf "$extract"
echo '{"kind":"automated","result":"passed","paths":["extract-and-run","extracted"]}' > "$out/status.json"
echo "PASS: $PRETTY_NAME"
