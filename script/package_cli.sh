#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist/release"
VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$ROOT_DIR/Cargo.toml")"

case "$(uname -m)" in
  arm64) ARCH="aarch64" ;;
  x86_64) ARCH="x86_64" ;;
  *) ARCH="$(uname -m)" ;;
esac

case "$(uname -s)" in
  Darwin) OS="apple-darwin" ;;
  Linux) OS="unknown-linux-gnu" ;;
  *) OS="$(uname -s | tr '[:upper:]' '[:lower:]')" ;;
esac

TARGET="${ARCH}-${OS}"
PACKAGE_NAME="shirabe-v${VERSION}-${TARGET}"
PACKAGE_DIR="$DIST_DIR/$PACKAGE_NAME"
ARCHIVE_PATH="$DIST_DIR/$PACKAGE_NAME.tar.gz"

cd "$ROOT_DIR"

cargo build --release

if [ -f "$ROOT_DIR/ui/package-lock.json" ]; then
  (cd "$ROOT_DIR/ui" && npm ci && npm run build)
else
  (cd "$ROOT_DIR/ui" && npm install && npm run build)
fi

rm -rf "$PACKAGE_DIR" "$ARCHIVE_PATH"
mkdir -p "$PACKAGE_DIR/bin" "$PACKAGE_DIR/ui"

cp "$ROOT_DIR/target/release/shirabe" "$PACKAGE_DIR/bin/shirabe"
cp -R "$ROOT_DIR/ui/dist" "$PACKAGE_DIR/ui/dist"
cp "$ROOT_DIR/README.md" "$PACKAGE_DIR/README.md"
cp "$ROOT_DIR/LICENSE" "$PACKAGE_DIR/LICENSE"

tar -czf "$ARCHIVE_PATH" -C "$DIST_DIR" "$PACKAGE_NAME"
shasum -a 256 "$ARCHIVE_PATH" >"$ARCHIVE_PATH.sha256"

echo "$ARCHIVE_PATH"
