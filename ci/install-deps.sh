#!/usr/bin/env bash
# Run as root on a supported distro. --ci adds virtual-display test tools.
set -euo pipefail

if [[ $# -gt 1 || ( $# -eq 1 && $1 != --ci ) ]]; then
    echo "Usage: bash ci/install-deps.sh [--ci]" >&2
    exit 2
fi
source /etc/os-release
case "$ID" in
    ubuntu|debian)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update
        packages=(build-essential pkg-config clang libclang-dev libpipewire-0.3-dev
            libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libfontconfig1-dev
            git curl ca-certificates
            libxcursor1 libxi6 libxrandr2 libxinerama1 libxkbcommon-x11-0
            libegl1 libgl1 fontconfig fonts-dejavu-core)
        if [[ ${1:-} == --ci ]]; then packages+=(xvfb dbus-daemon); fi
        apt-get install -y --no-install-recommends "${packages[@]}"
        ;;
    fedora)
        packages=(gcc gcc-c++ make pkgconf-pkg-config clang clang-libs pipewire-devel
            libxkbcommon-devel wayland-devel libX11-devel libxcb-devel fontconfig-devel
            git curl-minimal ca-certificates
            libXcursor libXi libXrandr libXinerama libxkbcommon-x11
            mesa-libEGL mesa-libGL fontconfig dejavu-sans-fonts)
        if [[ ${1:-} == --ci ]]; then packages+=(xorg-x11-server-Xvfb dbus-daemon); fi
        dnf install -y --setopt=install_weak_deps=False "${packages[@]}"
        ;;
    arch)
        packages=(base-devel pkgconf clang pipewire libxkbcommon wayland libx11 libxcb
            git curl ca-certificates libxcursor libxi libxrandr libxinerama
            libxkbcommon-x11 libglvnd mesa fontconfig ttf-dejavu)
        if [[ ${1:-} == --ci ]]; then packages+=(xorg-server-xvfb dbus); fi
        pacman -Syu --noconfirm --needed "${packages[@]}"
        ;;
    *) echo "Unsupported distro: $ID" >&2; exit 1 ;;
esac
