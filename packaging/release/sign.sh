#!/usr/bin/env bash
# Sign and publish a draft release made by the "Tagged release" workflow.
#
#   packaging/release/sign.sh v0.2.0
#
# Downloads the draft's AppImage and checksum, verifies them, signs the checksum
# with the offline minisign key, verifies the signature against the public key
# the app embeds, uploads it, and publishes the release after confirmation.
# Esquie only offers updates whose checksum carries this signature.
#
# ESQUIE_SECRET_KEY overrides the secret key path (default ~/.minisign/esquie.key).
set -euo pipefail

tag=${1:?usage: sign.sh v<version>}
[[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Expected a stable tag like v1.2.3, got $tag" >&2; exit 1; }
version=${tag#v}
appimage="Esquie-${version}-x86_64.AppImage"
checksum="${appimage}.sha256"
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
public_key="$here/esquie.pub"
secret_key=${ESQUIE_SECRET_KEY:-$HOME/.minisign/esquie.key}

for tool in gh minisign sha256sum; do
    command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }
done
[[ -r $secret_key ]] || { echo "Secret key not found: $secret_key" >&2; exit 1; }

# Resolve the repository from the script's own checkout so the gh calls below
# keep working from any directory, including the temporary one used later.
repo=$(cd "$here" && gh repo view --json nameWithOwner --jq .nameWithOwner)

draft=$(gh release view -R "$repo" "$tag" --json isDraft --jq .isDraft)
[[ $draft == true ]] || { echo "$tag is not a draft release; refusing to re-sign it." >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
gh release download -R "$repo" "$tag" --dir "$work" --pattern "$appimage" --pattern "$checksum"
cd "$work"

# The signature vouches for these exact bytes, so check them before signing.
sha256sum --check --strict "$checksum"
minisign -S -s "$secret_key" -m "$checksum" -t "file:$checksum"
minisign -V -p "$public_key" -m "$checksum"

echo
echo "Signed $checksum for $tag."
gh release upload -R "$repo" "$tag" "$checksum.minisig"
read -r -p "Publish $tag now? Esquie installs will be offered this update. [y/N] " answer
if [[ $answer == [yY] ]]; then
    gh release edit -R "$repo" "$tag" --draft=false --latest
    echo "Published $tag."
else
    echo "Left $tag as a signed draft. Publish later with: gh release edit -R $repo $tag --draft=false --latest"
fi
