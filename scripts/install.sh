#!/bin/sh
# easi installer — usage:
#   curl -fsSL https://raw.githubusercontent.com/xuckless/easi-install/main/scripts/install.sh | sh
set -eu

REPO="${EASI_REPO:-xuckless/easi-install}"
VERSION="${EASI_VERSION:-latest}"
BIN_DIR="${HOME}/.local/bin"
MINISIGN_PUBKEY="${EASI_MINISIGN_PUBKEY:-RWQnPmfJNaxDO2ecscxiezHEPZmmtizudyau8OENs5gw21DZWlmc2IRQ}"

die() {
    printf 'easi install error: %s\n' "$1" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64) printf 'x86_64' ;;
        aarch64|arm64) printf 'aarch64' ;;
        *) die "unsupported architecture: $(uname -m)" ;;
    esac
}

verify_sha256() {
    sums_file="$1"
    target_name="$2"
    expected="$(awk -v f="$target_name" '$2 == f { print $1 }' "$sums_file")"
    [ -n "$expected" ] || die "checksum for $target_name not found in SHA256SUMS"
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$target_name" | awk '{ print $1 }')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$target_name" | awk '{ print $1 }')"
    else
        die "neither sha256sum nor shasum is available"
    fi
    [ "$actual" = "$expected" ] || die "SHA-256 mismatch for $target_name"
}

main() {
    [ "$(uname -s)" = "Linux" ] || die "easi only supports Linux"
    need_cmd curl
    need_cmd tar

    arch="$(detect_arch)"
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    if [ "$VERSION" = "latest" ]; then
        tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
            | head -n 1)"
        [ -n "$tag" ] || die "could not determine latest release tag for ${REPO}"
    else
        tag="$VERSION"
    fi

    tarball="easi-${tag}-linux-${arch}.tar.gz"
    base_url="https://github.com/${REPO}/releases/download/${tag}"

    printf 'Downloading %s...\n' "$tarball"
    curl -fsSL "${base_url}/${tarball}" -o "${tmpdir}/${tarball}"
    curl -fsSL "${base_url}/SHA256SUMS" -o "${tmpdir}/SHA256SUMS"

    if [ -n "${MINISIGN_PUBKEY}" ] && command -v minisign >/dev/null 2>&1; then
        if curl -fsSL "${base_url}/SHA256SUMS.minisig" -o "${tmpdir}/SHA256SUMS.minisig" 2>/dev/null; then
            minisign -V -P "${MINISIGN_PUBKEY}" -m "${tmpdir}/SHA256SUMS" >/dev/null \
                || die "minisign signature verification failed"
            printf 'Verified minisign signature.\n'
        fi
    fi

    (cd "$tmpdir" && verify_sha256 "SHA256SUMS" "$tarball")

    tar -xzf "${tmpdir}/${tarball}" -C "$tmpdir"
    [ -f "${tmpdir}/easi/easi" ] || die "unexpected archive layout (missing easi/easi)"

    mkdir -p "$BIN_DIR"
    install -m 0755 "${tmpdir}/easi/easi" "${BIN_DIR}/easi"

    printf 'Installed easi %s to %s/easi\n' "$tag" "$BIN_DIR"
    "${BIN_DIR}/easi" --setup || true

    case ":${PATH}:" in
        *":${BIN_DIR}:"*) ;;
        *)
            printf '\nNote: %s is not in your PATH.\n' "$BIN_DIR"
            printf 'Add this to your shell profile:\n  export PATH="$HOME/.local/bin:$PATH"\n'
            ;;
    esac
}

main "$@"
