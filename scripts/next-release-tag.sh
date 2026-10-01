#!/bin/sh
# Prints the tag for releasing the current commit if the bundled jellyfin-ffmpeg
# version differs from the one in the latest release, and nothing otherwise.
#
# Release tags are <upstream jellyfin-ffmpeg version>-<our version>, e.g.
# 7.1.4-7.5 for jellyfin-ffmpeg 7.1.4-3. Our version is <major>.<minor> and
# the minor number increases with every release.
set -eu

dockerfile=docker/Dockerfile.server
ffmpeg_version() { sed -n 's/^ARG JELLYFIN_FFMPEG_VERSION=//p'; }

current=$(ffmpeg_version < "$dockerfile")
if [ -z "$current" ]; then
    echo "JELLYFIN_FFMPEG_VERSION not found in $dockerfile" >&2
    exit 1
fi

# Latest release by our version
latest=$(git tag -l \
    | grep -E '^[0-9]+\.[0-9]+\.[0-9]+-[0-9]+\.[0-9]+$' \
    | awk -F- '{ split($2, v, "."); print v[1], v[2], $0 }' \
    | sort -n -k1,1 -k2,2 \
    | tail -n 1 \
    | cut -d' ' -f3)

if [ -z "$latest" ]; then
    next="${current%%.*}.1"
else
    released=$(git show "$latest:$dockerfile" 2>/dev/null | ffmpeg_version || true)
    if [ "$current" = "$released" ]; then
        exit 0
    fi
    ours=${latest#*-}
    next="${ours%%.*}.$(( ${ours#*.} + 1 ))"
fi

echo "${current%%-*}-$next"
