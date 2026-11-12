#!/usr/bin/env bash
# Runs the built AppImage in fresh runtime-only containers across the initial
# distro matrix (or the images given as arguments).
#
#   bash packaging/appimage/test.sh [image ...]
#
# Results land in target/appimage/test/<image>/; see container-test.sh.
set -euo pipefail
cd "$(dirname "$0")/../.."

engine=${CONTAINER_ENGINE:-$(command -v podman || command -v docker || true)}
[[ -n $engine ]] || { echo "Install Podman or Docker" >&2; exit 1; }
shopt -s nullglob
appimages=(target/appimage/Esquie-*-x86_64.AppImage)
[[ ${#appimages[@]} -eq 1 ]] || { echo "Build exactly one AppImage first (build.sh)" >&2; exit 1; }
(cd target/appimage && sha256sum --check --strict "$(basename "${appimages[0]}").sha256")

images=("$@")
if [[ ${#images[@]} -eq 0 ]]; then
    images=(ubuntu:24.04 ubuntu:26.04 debian:13 fedora:44 fedora:45 archlinux:base)
fi

failed=()
for image in "${images[@]}"; do
    name=${image//[:\/]/-}
    results=target/appimage/test/$name
    rm -rf "$results"
    mkdir -p "$results"
    "$engine" pull --quiet --platform linux/amd64 "docker.io/library/$image" >/dev/null
    digest=$("$engine" image inspect --format '{{index .RepoDigests 0}}' "docker.io/library/$image")
    echo "$digest" > "$results/image.txt"
    echo "== $image ($digest)"
    # Only the AppImage and the test script are mounted, never the source tree.
    if "$engine" run --rm --platform linux/amd64 --security-opt label=disable \
        --volume "$PWD/${appimages[0]}:/in/$(basename "${appimages[0]}"):ro" \
        --volume "$PWD/packaging/appimage/container-test.sh:/in/container-test.sh:ro" \
        --volume "$PWD/$results:/out" \
        "$digest" bash /in/container-test.sh "/in/$(basename "${appimages[0]}")" /out \
        > "$results/test.log" 2>&1; then
        tail -n 1 "$results/test.log"
    else
        failed+=("$image")
        tail -n 20 "$results/test.log"
    fi
done
if [[ ${#failed[@]} -gt 0 ]]; then
    echo "Failed: ${failed[*]}" >&2
    exit 1
fi
echo "All ${#images[@]} images passed."
