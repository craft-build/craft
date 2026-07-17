#!/usr/bin/env sh
# Craft installer. Builds the latest release from source via `cargo install`
# from https://github.com/craft-build/craft and installs it to ~/.cargo/bin.
set -eu

REPO="craft-build/craft"
INSTALL_DIR="${CRAFT_INSTALL_DIR:-$HOME/.cargo/bin}"
BINARY_NAME="craft"
PLAYWRIGHT_VERSION="1.60.0"

err() {
    printf '\033[31merror:\033[0m %s\n' "$1" >&2
    exit 1
}

github_curl() {
    token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
    if [ -n "${token}" ]; then
        curl -fsSL \
            -H "Authorization: Bearer ${token}" \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: craft-install" \
            "$@"
    else
        curl -fsSL \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: craft-install" \
            "$@"
    fi
}

info() {
    printf '\033[36m==>\033[0m %s\n' "$1"
}

warn() {
    printf '\033[33mwarning:\033[0m %s\n' "$1" >&2
}

command_exists() {
    command -v "$1" >/dev/null 2>&1
}

if ! command_exists cargo; then
    err "cargo not found. Install Rust from https://rustup.rs and re-run this script."
fi

info "looking up the latest release"
api_url="https://api.github.com/repos/${REPO}/releases/latest"
tag="$(github_curl "$api_url" \
    | grep -o '"tag_name":[[:space:]]*"[^"]*"' \
    | head -n1 \
    | sed -E 's/.*"([^"]+)"$/\1/')"

[ -n "$tag" ] || err "could not determine the latest release tag"

export PLAYWRIGHT_DRIVER_VERSION="$PLAYWRIGHT_VERSION"
export PLAYWRIGHT_SKIP_DRIVER_DOWNLOAD="1"

info "building ${BINARY_NAME} ${tag} from source (this compiles all dependencies and can take several minutes)"
cargo install --locked --force --git "https://github.com/${REPO}.git" --tag "$tag" "$BINARY_NAME"

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

if command_exists npm; then
    printf '\n'
    printf '%s\n' "Browser tooling (optional) needs the Playwright driver:"
    printf '  %s\n' "npm install -g playwright@${PLAYWRIGHT_VERSION}"
else
    printf '\n'
    warn "Browser tooling needs the Playwright driver, but npm was not found."
    warn "Install Node.js, then run: npm install -g playwright@${PLAYWRIGHT_VERSION}"
fi

info "done. Run 'craft --version' to verify."
