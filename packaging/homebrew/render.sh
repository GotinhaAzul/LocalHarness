#!/bin/sh
# Render packaging/homebrew/localharness.rb.in with real release checksums.
# The release workflow uses this and attaches the result; a tap maintainer can
# run it by hand after downloading SHA256SUMS.
#
# Usage: render.sh VERSION SHA256SUMS SOURCE_SHA256
#   VERSION      release version, with or without a leading v
#   SHA256SUMS   the combined checksum file from the GitHub release
#   SOURCE_SHA256 checksum of the vVERSION source archive (macOS build)

set -eu

if [ "$#" -ne 3 ]; then
    printf 'usage: %s VERSION SHA256SUMS SOURCE_SHA256\n' "$0" >&2
    exit 2
fi

version="${1#v}"
sums="$2"
source_sha="$3"
here="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
template="${here}/localharness.rb.in"

[ -f "$sums" ] || {
    printf 'error: %s does not exist\n' "$sums" >&2
    exit 1
}

checksum() {
    awk -v artifact="$1" '
        {
            name = $2
            sub(/^\*/, "", name)
            if (name == artifact) {
                print $1
                exit
            }
        }
    ' "$sums"
}

x86_64="$(checksum "lh-v${version}-x86_64-unknown-linux-musl.tar.gz")"
aarch64="$(checksum "lh-v${version}-aarch64-unknown-linux-musl.tar.gz")"

for value in "$x86_64" "$aarch64" "$source_sha"; do
    case "$value" in
        "" | *[!0-9a-fA-F]*) printf 'error: missing or malformed SHA-256 in %s\n' "$sums" >&2; exit 1 ;;
    esac
    [ "${#value}" -eq 64 ] || {
        printf 'error: SHA-256 values must be 64 hex characters\n' >&2
        exit 1
    }
done

rendered="$(
    sed \
        -e "s/__VERSION__/${version}/g" \
        -e "s/__SHA256_LINUX_X86_64__/${x86_64}/g" \
        -e "s/__SHA256_LINUX_AARCH64__/${aarch64}/g" \
        -e "s/__SHA256_SOURCE__/${source_sha}/g" \
        "$template"
)"

if printf '%s\n' "$rendered" | grep -q '__[A-Z0-9_]*__'; then
    printf 'error: unreplaced placeholder in %s\n' "$template" >&2
    exit 1
fi

printf '%s\n' "$rendered"
