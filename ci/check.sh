#!/usr/bin/env bash
# The same locked build and checks run locally and in each CI container.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked
ESQUIE_UI_OUTPUT_DIR="${CARGO_TARGET_DIR:-target}/validation/ui" cargo test --release --locked
ESQUIE_PRIVATE_BUS=1 dbus-run-session -- timeout 30s \
    cargo test --release --locked tray::live::tray_availability -- --ignored --exact
cargo build --release --locked --example software_smoke --example audio_harness
python3 ci/audio-integration.py --harness "${CARGO_TARGET_DIR:-target}/release/examples/audio_harness"

# -displayfd reports a free display only after the server is ready. Disabling
# GLX and explicitly choosing Slint's software renderer removes GPU dependence.
smoke_dir=$(mktemp -d)
xvfb_pid=
cleanup() {
    if [[ -n "$xvfb_pid" ]]; then
        kill "$xvfb_pid" 2>/dev/null || true
        wait "$xvfb_pid" 2>/dev/null || true
    fi
    rm -rf "$smoke_dir"
}
trap cleanup EXIT
Xvfb -displayfd 3 -screen 0 1280x1024x24 -extension GLX -nolisten tcp -ac \
    3>"$smoke_dir/display" >"$smoke_dir/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in {1..100}; do
    if [[ -s "$smoke_dir/display" ]]; then break; fi
    if ! kill -0 "$xvfb_pid" 2>/dev/null; then
        cat "$smoke_dir/xvfb.log" >&2
        exit 1
    fi
    sleep 0.1
done
if [[ ! -s "$smoke_dir/display" ]]; then
    cat "$smoke_dir/xvfb.log" >&2
    echo "Xvfb did not become ready" >&2
    exit 1
fi
mkdir -m 700 "$smoke_dir/runtime"
env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
    DISPLAY=":$(cat "$smoke_dir/display")" XDG_RUNTIME_DIR="$smoke_dir/runtime" \
    SLINT_BACKEND=winit-software \
    dbus-run-session -- timeout 30s "${CARGO_TARGET_DIR:-target}/release/examples/software_smoke"
