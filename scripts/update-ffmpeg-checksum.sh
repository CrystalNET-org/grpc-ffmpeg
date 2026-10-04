#!/bin/sh
# Sets JELLYFIN_FFMPEG_SHA256 in docker/Dockerfile.server to the checksum of the
# jellyfin-ffmpeg .deb of JELLYFIN_FFMPEG_VERSION, which the image build checks.
# jellyfin-ffmpeg publishes no checksums, so the file is downloaded once here;
# Renovate runs this when it updates the version (renovate.json), so the
# checksum changes in the same reviewed commit.
set -eu

dockerfile=docker/Dockerfile.server
version=$(sed -n 's/^ARG JELLYFIN_FFMPEG_VERSION=//p' "$dockerfile")
if [ -z "$version" ]; then
    echo "JELLYFIN_FFMPEG_VERSION not found in $dockerfile" >&2
    exit 1
fi

url="https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v${version}/jellyfin-ffmpeg${version%%.*}_${version}-trixie_amd64.deb"
file=$(mktemp)
trap 'rm -f "$file"' EXIT
if command -v curl >/dev/null; then curl -fsSL -o "$file" "$url"; else wget -q -O "$file" "$url"; fi
sum=$(sha256sum "$file" | cut -d' ' -f1)
sed -i "s/^ARG JELLYFIN_FFMPEG_SHA256=.*/ARG JELLYFIN_FFMPEG_SHA256=$sum/" "$dockerfile"
echo "jellyfin-ffmpeg $version: $sum"
