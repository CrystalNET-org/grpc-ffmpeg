#!/bin/sh
# Prints the tag for releasing the current commit if what is shipped changed
# since the latest release: the bundled jellyfin-ffmpeg version, or the worker,
# client or protocol (the paths below). Prints nothing otherwise.
#
# Release tags are <upstream jellyfin-ffmpeg version>-<our version>, e.g.
# 7.1.4-7.5 for jellyfin-ffmpeg 7.1.4-3. Our version is <major>.<minor> and
# the minor number increases with every release.
set -eu

dockerfile=docker/Dockerfile.server
# What the worker image and the client binaries are built from
shipped="docker src/server src/client src/proto requirements.txt"
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
    if ! git cat-file -e "$latest^{commit}" 2>/dev/null; then
        echo "The commit of $latest is missing; fetch the tags first" >&2
        exit 1
    fi
    released=$(git show "$latest:$dockerfile" 2>/dev/null | ffmpeg_version || true)
    # shellcheck disable=SC2086 # one argument per path
    if [ "$current" = "$released" ] && git diff --quiet "$latest" HEAD -- $shipped; then
        exit 0
    fi
    ours=${latest#*-}
    next="${ours%%.*}.$(( ${ours#*.} + 1 ))"
fi

echo "${current%%-*}-$next"
