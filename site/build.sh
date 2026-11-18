#!/bin/sh
set -e

# Assembles the static landing page + mdBook docs into a single output dir.
# Used by CI (GitHub Pages) and locally.

MDBOOK_VERSION="${MDBOOK_VERSION:-0.5.3}"

if ! command -v mdbook >/dev/null 2>&1; then
  echo "Installing mdbook ${MDBOOK_VERSION}..."
  case "$(uname -m)-$(uname -s)" in
    arm64-Darwin)  triple="aarch64-apple-darwin" ;;
    x86_64-Darwin) triple="x86_64-apple-darwin" ;;
    x86_64-Linux)  triple="x86_64-unknown-linux-gnu" ;;
    aarch64-Linux) triple="aarch64-unknown-linux-gnu" ;;
    *) echo "Unsupported platform: $(uname -m)-$(uname -s)"; exit 1 ;;
  esac
  mkdir -p .bin
  curl -sL "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/mdbook-v${MDBOOK_VERSION}-${triple}.tar.gz" | tar xz -C .bin
  export PATH="$PWD/.bin:$PATH"
fi

echo "Using $(mdbook --version)"

OUT="_build"
rm -rf "$OUT"
mkdir -p "$OUT"

# 1. Static landing page
cp index.html "$OUT/"
cp favicon.ico favicon-16x16.png favicon-32x32.png "$OUT/"
cp apple-touch-icon.png android-chrome-192x192.png android-chrome-512x512.png "$OUT/"
cp site.webmanifest "$OUT/"

# 2. mdBook docs
mdbook build docs --dest-dir "$OUT/docs"
