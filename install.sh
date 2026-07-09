#!/usr/bin/env sh
# Craft installer. Fetches the latest release binary for the current platform
# from https://github.com/craft-build/craft and installs it to ~/.cargo/bin.
set -eu

REPO="craft-build/craft"
INSTALL_DIR="${CRAFT_INSTALL_DIR:-$HOME/.cargo/bin}"
BINARY_NAME="craft"

err() {
    printf '\033[31merror:\033[0m %s\n' "$1" >&2
    exit 1
}

info() {
    printf '\033[36m==>\033[0m %s\n' "$1"
}

# Detect target triple.
uname_s="$(uname -s)"
uname_m="$(uname -m)"

case "$uname_s" in
    Linux) os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *) err "unsupported OS: $uname_s" ;;
esac

case "$uname_m" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) err "unsupported architecture: $uname_m" ;;
esac

target="${arch}-${os}"
artifact="craft-${target}.tar.gz"

info "looking up the latest release"
api_url="https://api.github.com/repos/${REPO}/releases/latest"
download_url="$(curl -fsSL "$api_url" \
    | grep -o "https://[^\"']*${artifact}" \
    | head -n1)"

[ -n "$download_url" ] || err "no release asset found for ${target}"

info "downloading ${artifact}"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
curl -fsSL -o "${tmpdir}/${artifact}" "$download_url"

tar -C "$tmpdir" -xzf "${tmpdir}/${artifact}"

info "installing ${BINARY_NAME} to ${INSTALL_DIR}"
mkdir -p "$INSTALL_DIR"
mv -f "${tmpdir}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        printf '\n'
        printf '%s\n' "Craft was installed to ${INSTALL_DIR}, which is not on your PATH."
        printf '%s\n' "Add it with:"
        printf '\n'
        printf '  %s\n' "export PATH=\"${INSTALL_DIR}:\$PATH\""
        printf '\n'
        ;;
esac

info "done. Run 'craft --version' to verify."
