#!/usr/bin/env bash
# Checks an AppImage's format and payload: type 2, the expected entry points,
# and no bundled PipeWire, audio services, glibc, loader, or graphics drivers.
# Usage: bash packaging/appimage/inspect.sh Esquie-<version>-x86_64.AppImage
# Needs readelf (binutils). Prints the bundled files and the binary's NEEDED list.
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "Usage: bash packaging/appimage/inspect.sh APPIMAGE" >&2
    exit 2
fi
appimage=$(readlink -f "$1")
fail() { echo "FAIL: $*" >&2; exit 1; }

# Type 2 AppImages carry "AI\x02" at offset 8 of the ELF runtime header.
magic=$(od -An -tx1 -j8 -N3 "$appimage" | tr -d ' \n')
[[ $magic == 414902 ]] || fail "not a type 2 AppImage (magic $magic)"
echo "Format: type 2 AppImage"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# Extraction needs no FUSE and writes squashfs-root into the current directory.
(cd "$tmp" && "$appimage" --appimage-extract >/dev/null)
root=$tmp/squashfs-root

for path in AppRun .DirIcon esquie.desktop esquie.svg usr/bin/esquie; do
    [[ -e $root/$path ]] || fail "missing $path"
done
doc=$root/usr/share/doc/esquie
for name in LICENSE THIRD_PARTY_NOTICES.md dependencies.html cargo-notices.html \
    dependencies.json Cargo.lock license-sources.json deepfilternet.txt \
    appimage-runtime.txt appimage-runtime-source-header.txt musl.txt \
    libfuse-LGPL-2.1.txt libfuse-GPL-2.0.txt squashfuse.txt zstd.txt zlib.txt mimalloc.txt; do
    [[ -s $doc/$name ]] || fail "missing or empty license documentation: $name"
done
grep -q 'GNU GENERAL PUBLIC LICENSE' "$doc/LICENSE" || fail "missing GPL license text"
grep -q 'Esquie dependency licenses' "$doc/dependencies.html" || fail "missing dependency licenses"

grep -qx 'Exec=esquie' "$root/esquie.desktop" || fail "desktop entry Exec is not 'esquie'"
grep -qx 'Icon=esquie' "$root/esquie.desktop" || fail "desktop entry Icon is not 'esquie'"
[[ $(readlink "$root/AppRun") == usr/bin/esquie ]] \
    || fail "AppRun should be linuxdeploy's plain link to usr/bin/esquie (no environment changes)"

# Nothing that would replace or reconfigure the host's audio stack, and
# nothing on the standard exclusion list for system libraries and drivers.
forbidden='libpipewire|/spa-0\.2/|/pipewire-0\.3/|/pipewire/|wireplumber|pulseaudio|libpulse'
forbidden+='|/libc\.so|/libm\.so|/libdl\.so|/libpthread\.so|/librt\.so|/ld-linux'
forbidden+='|libGL|libEGL|libGLX|libGLdispatch|libgbm|libdrm|libvulkan|libnvidia|/dri/'
found=$(cd "$root" && find . \( -type f -o -type l \) | grep -E "$forbidden" || true)
[[ -z $found ]] || fail "forbidden files bundled:"$'\n'"$found"
for exe in pipewire pipewire-pulse wireplumber pw-cli; do
    [[ -z $(find "$root" -name "$exe" -print -quit) ]] || fail "bundles $exe"
done

needed=$(readelf -d "$root/usr/bin/esquie" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
grep -qx 'libpipewire-0.3.so.0' <<<"$needed" \
    || fail "esquie does not link the host's libpipewire-0.3.so.0"

echo "Payload:"
(cd "$root" && find . -mindepth 1 \( -type f -o -type l \) | sort | sed 's/^/  /')
echo "esquie NEEDED (resolved from the host unless bundled above):"
sed 's/^/  /' <<<"$needed"
echo "RUNPATH: $(readelf -d "$root/usr/bin/esquie" | sed -n 's/.*(RUNPATH).*\[\(.*\)\]/\1/p')"
echo "OK"
