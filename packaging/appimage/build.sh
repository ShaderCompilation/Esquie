#!/usr/bin/env bash
# Builds Esquie's AppImage in a clean Ubuntu 24.04 container (Docker or Podman).
#
#   bash packaging/appimage/build.sh
#
# Output: target/appimage/Esquie-<version>-x86_64.AppImage and .sha256, with
# build evidence in target/appimage/evidence/. Cargo output is cached in
# target/appimage-build; delete it for a from-scratch build.
set -euo pipefail
cd "$(dirname "$0")/../.."

engine=${CONTAINER_ENGINE:-$(command -v podman || command -v docker || true)}
[[ -n $engine ]] || { echo "Install Podman or Docker" >&2; exit 1; }

image=docker.io/library/ubuntu:24.04
"$engine" pull --quiet --platform linux/amd64 "$image" >/dev/null
digest=$("$engine" image inspect --format '{{index .RepoDigests 0}}' "$image")
echo "Building in $digest"

# Optional host cache directories (set by CI) let fresh containers reuse
# Cargo downloads and the pinned Rust toolchain.
cache_args=()
if [[ -n ${esquie_cargo_cache:-} && -n ${esquie_rustup_cache:-} ]]; then
    mkdir -p "$esquie_cargo_cache" "$esquie_rustup_cache"
    cache_args=(
        --env CARGO_HOME=/cache/cargo --env RUSTUP_HOME=/cache/rustup
        --volume "$esquie_cargo_cache:/cache/cargo"
        --volume "$esquie_rustup_cache:/cache/rustup"
    )
fi

# label=disable lets SELinux hosts mount the checkout without relabeling it.
"$engine" run --rm --platform linux/amd64 \
    --security-opt label=disable \
    --env CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$(nproc)}" \
    "${cache_args[@]}" \
    --volume "$PWD:/workspace" --workdir /workspace \
    "$digest" bash packaging/appimage/container-build.sh
echo "$digest" > target/appimage/evidence/image.txt
ls -l target/appimage/*.AppImage*
