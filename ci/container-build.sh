#!/usr/bin/env bash
# Entry point inside a fresh distro container; writes validation evidence.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p target/validation
bash ci/install-deps.sh --ci
cat /etc/os-release > target/validation/os-release.txt
source /etc/os-release
case "$ID" in
    ubuntu|debian) dpkg-query -W > target/validation/packages.txt ;;
    fedora) rpm -qa | sort > target/validation/packages.txt ;;
    arch) pacman -Q > target/validation/packages.txt ;;
esac

curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/esquie-rustup-init.sh
sh /tmp/esquie-rustup-init.sh -y --profile minimal --default-toolchain none
export PATH="$HOME/.cargo/bin:$PATH"
# Baseline x86_64, including when the runner itself supports newer instructions.
export RUSTFLAGS="-C target-cpu=x86-64"
rustc -Vv > target/validation/rustc.txt
cargo -V > target/validation/cargo.txt
pkg-config --modversion libpipewire-0.3 > target/validation/pipewire.txt
bash ci/check.sh
target/release/esquie --version > target/validation/version.txt
cp Cargo.lock target/validation/Cargo.lock
sha256sum target/release/esquie > target/validation/sha256.txt
