#!/bin/sh
set -eu

REPO="craft-build/craft"
BINARY="craft"
INSTALL_DIR="${CRAFT_INSTALL_DIR:-/usr/local/bin}"

main() {
    need_cmd curl

    case "$(uname -s)" in
        Linux)  os="unknown-linux-musl" ;;
        Darwin) os="apple-darwin" ;;
        *) err "unsupported OS: $(uname -s)" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64)   arch="x86_64" ;;
        aarch64|arm64)  arch="aarch64" ;;
        *) err "unsupported architecture: $(uname -m)" ;;
    esac

    target="${arch}-${os}"

    tag="${1:-$(curl -fsSL "https://gitlab.com/api/v4/projects/craft-build%2Fcraft/releases" \
        | grep '"tag_name"' | head -1 | cut -d'"' -f4)}"
    [ -n "${tag}" ] || err "failed to determine latest release tag"

    url="https://gitlab.com/craft-build/craft/-/releases/${tag}/downloads/${BINARY}-${tag}-${target}.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT

    echo "downloading ${BINARY} ${tag} for ${target}..."
    curl -fsSL "${url}" | tar xz -C "${tmp}"

    if [ -w "${INSTALL_DIR}" ]; then
        mv "${tmp}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    else
        echo "installing to ${INSTALL_DIR} (requires sudo)..."
        sudo mv "${tmp}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    fi

    chmod +x "${INSTALL_DIR}/${BINARY}"
    echo "${BINARY} ${tag} installed to ${INSTALL_DIR}/${BINARY}"
    echo ""
}

need_cmd() {
    command -v "$1" > /dev/null 2>&1 || err "need '$1' (not found)"
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"
