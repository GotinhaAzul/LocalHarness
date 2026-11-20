#!/bin/sh
# Totality installer: download a static `tot` binary, verify its SHA-256
# checksum, and install it under ~/.local/bin without sudo.
#
# Usage:
#   sh install.sh [VERSION]
#
# Environment overrides:
#   TOT_VERSION             release to install (default: latest)
#   TOT_INSTALL_DIR         binary directory (default: ~/.local/bin)
#   TOT_INSTALL_BASE_URL    release download root
#                          (default: https://github.com/<repo>/releases/download)
#   TOT_INSTALL_SOURCE_DIR  read artifacts from a local directory instead of
#                          downloading; used by the offline installer test
#   TOT_REPO                owner/name on GitHub (default: GotinhaAzul/Totality)
#   TOT_TARGET              override the detected target triple
#   TOT_NO_EXTRAS=1         skip completions and the man page
#
# The pre-0.3 LH_* names of these variables still work.
#   XDG_DATA_HOME          completion and man-page root (default: ~/.local/share)
#
# The installer never uses sudo and never runs a package manager. A checksum
# mismatch removes the temporary download and exits before anything is
# installed.

set -eu

REPO="${TOT_REPO:-${LH_REPO:-GotinhaAzul/Totality}}"
VERSION="${TOT_VERSION:-${LH_VERSION:-${1:-latest}}}"
DEST="${TOT_INSTALL_DIR:-${LH_INSTALL_DIR:-${HOME}/.local/bin}}"
BASE_URL="${TOT_INSTALL_BASE_URL:-${LH_INSTALL_BASE_URL:-https://github.com/${REPO}/releases/download}}"
SOURCE_DIR="${TOT_INSTALL_SOURCE_DIR:-${LH_INSTALL_SOURCE_DIR:-}}"

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

note() {
    printf '%s\n' "$*" >&2
}

if [ -z "$SOURCE_DIR" ]; then
    command -v curl >/dev/null 2>&1 || fail "curl is required to download a release; install curl or set TOT_INSTALL_SOURCE_DIR"
fi

TOT_TARGET="${TOT_TARGET:-${LH_TARGET:-}}"
if [ -z "${TOT_TARGET:-}" ]; then
    os="$(uname -s)"
    [ "$os" = "Linux" ] || fail "unsupported operating system ${os}; release binaries are Linux only (macOS can build from source)"
    case "$(uname -m)" in
        x86_64 | amd64) TARGET="x86_64-unknown-linux-musl" ;;
        aarch64 | arm64) TARGET="aarch64-unknown-linux-musl" ;;
        *) fail "unsupported architecture $(uname -m); release binaries exist for x86_64 and aarch64" ;;
    esac
else
    TARGET="$TOT_TARGET"
fi

if [ "$VERSION" = "latest" ]; then
    tag="$(curl -fsSL --proto '=https' --tlsv1.2 \
        "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n 1)" || true
    [ -n "$tag" ] || fail "cannot resolve the latest release from GitHub; set TOT_VERSION to a release such as 0.2.1"
    VERSION="$tag"
fi
VERSION="${VERSION#v}"

# Releases before 0.3 shipped the binary as `lh`.
case "$VERSION" in
    0.1.*|0.2.*) BIN="lh" ;;
    *) BIN="tot" ;;
esac
ARTIFACT="${BIN}-v${VERSION}-${TARGET}.tar.gz"
SHA_FILE="SHA256SUMS"

tmp=""
cleanup() {
    if [ -n "$tmp" ]; then
        rm -rf "$tmp"
    fi
}
trap cleanup EXIT HUP INT TERM
tmp="$(mktemp -d "${TMPDIR:-/tmp}/tot-install.XXXXXX")" || fail "cannot create a temporary directory"

fetch() {
    name="$1"
    dest="$2"
    if [ -n "$SOURCE_DIR" ]; then
        [ -f "${SOURCE_DIR}/${name}" ] || fail "missing ${name} in ${SOURCE_DIR}"
        cp "${SOURCE_DIR}/${name}" "$dest"
        return 0
    fi
    curl -fsSL --proto '=https' --tlsv1.2 "${BASE_URL}/v${VERSION}/${name}" -o "$dest" \
        || fail "cannot download ${name} for version ${VERSION}"
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "no SHA-256 tool found; install coreutils (sha256sum) or Perl shasum"
    fi
}

note "Totality ${VERSION} for ${TARGET}"
fetch "$SHA_FILE" "${tmp}/${SHA_FILE}"
fetch "$ARTIFACT" "${tmp}/${ARTIFACT}"

expected="$(awk -v artifact="$ARTIFACT" '
    {
        name = $2
        sub(/^\*/, "", name)
        if (name == artifact) {
            print tolower($1)
            exit
        }
    }
' "${tmp}/${SHA_FILE}")"

[ -n "$expected" ] || fail "no checksum for ${ARTIFACT} in ${SHA_FILE}; refusing to install"
case "$expected" in
    *[!0-9a-f]*) fail "malformed checksum for ${ARTIFACT} in ${SHA_FILE}; refusing to install" ;;
esac
[ "${#expected}" -eq 64 ] || fail "malformed checksum for ${ARTIFACT} in ${SHA_FILE}; refusing to install"

actual="$(sha256_file "${tmp}/${ARTIFACT}" | tr '[:upper:]' '[:lower:]')"
if [ "$expected" != "$actual" ]; then
    fail "checksum mismatch for ${ARTIFACT}: expected ${expected}, got ${actual}. The download was corrupted or tampered with; nothing was installed."
fi

tar -xzf "${tmp}/${ARTIFACT}" -C "$tmp" || fail "cannot extract ${ARTIFACT}"
[ -f "${tmp}/${BIN}" ] || fail "${ARTIFACT} does not contain a ${BIN} binary"

mkdir -p "$DEST"
cp "${tmp}/${BIN}" "${DEST}/${BIN}"
chmod 0755 "${DEST}/${BIN}"
if [ "$BIN" = "tot" ]; then
    ln -sf tot "${DEST}/totality"
fi

if [ "${TOT_NO_EXTRAS:-${LH_NO_EXTRAS:-}}" != "1" ]; then
    data="${XDG_DATA_HOME:-${HOME}/.local/share}"
    if [ -f "${tmp}/completions/${BIN}.bash" ]; then
        mkdir -p "${data}/bash-completion/completions"
        cp "${tmp}/completions/${BIN}.bash" "${data}/bash-completion/completions/${BIN}"
    fi
    if [ -f "${tmp}/completions/${BIN}.zsh" ]; then
        mkdir -p "${data}/zsh/site-functions"
        cp "${tmp}/completions/${BIN}.zsh" "${data}/zsh/site-functions/_${BIN}"
    fi
    if [ -f "${tmp}/completions/${BIN}.fish" ]; then
        mkdir -p "${data}/fish/vendor_completions.d"
        cp "${tmp}/completions/${BIN}.fish" "${data}/fish/vendor_completions.d/${BIN}.fish"
    fi
    if [ -f "${tmp}/man/${BIN}.1" ]; then
        mkdir -p "${data}/man/man1"
        cp "${tmp}/man/${BIN}.1" "${data}/man/man1/${BIN}.1"
    fi
fi

note "installed ${DEST}/${BIN}"
"${DEST}/${BIN}" --version >&2

case ":${PATH}:" in
    *":${DEST}:"*) ;;
    *) note "add ${DEST} to PATH to run ${BIN} from a new shell" ;;
esac
