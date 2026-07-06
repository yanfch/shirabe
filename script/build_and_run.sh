#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-run}"
APP_NAME="ShirabeBar"
BUNDLE_ID="dev.yanfch.ShirabeBar"
MIN_SYSTEM_VERSION="13.0"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT_DIR/app/macos/ShirabeBar"
DIST_DIR="$ROOT_DIR/dist"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"
APP_CONTENTS="$APP_BUNDLE/Contents"
APP_MACOS="$APP_CONTENTS/MacOS"
APP_RESOURCES="$APP_CONTENTS/Resources"
APP_BINARY="$APP_MACOS/$APP_NAME"
SERVER_BINARY="$APP_MACOS/ShirabeServer"
INFO_PLIST="$APP_CONTENTS/Info.plist"
SOURCE_RESOURCES="$APP_DIR/Sources/ShirabeBar/Resources"

pkill -x "$APP_NAME" >/dev/null 2>&1 || true
pkill -x "ShirabeServer" >/dev/null 2>&1 || true

(cd "$ROOT_DIR" && cargo build)
(cd "$ROOT_DIR/ui" && npm run build)
(cd "$APP_DIR" && swift build)
BUILD_DIR="$(cd "$APP_DIR" && swift build --show-bin-path)"
BUILD_BINARY="$BUILD_DIR/$APP_NAME"

rm -rf "$APP_BUNDLE"
mkdir -p "$APP_MACOS" "$APP_RESOURCES"
cp "$BUILD_BINARY" "$APP_BINARY"
chmod +x "$APP_BINARY"
cp "$ROOT_DIR/target/debug/shirabe" "$SERVER_BINARY"
chmod +x "$SERVER_BINARY"
cp "$SOURCE_RESOURCES/AppIcon.icns" "$APP_RESOURCES/AppIcon.icns"
cp "$SOURCE_RESOURCES/MenuBarIconTemplate.png" "$APP_RESOURCES/MenuBarIconTemplate.png"
cp -R "$ROOT_DIR/ui/dist" "$APP_RESOURCES/ui"

cat >"$INFO_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleExecutable</key>
  <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>LSMinimumSystemVersion</key>
  <string>$MIN_SYSTEM_VERSION</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST

open_app() {
  if ! curl -fs "http://127.0.0.1:7778/api/health" >/dev/null 2>&1; then
    echo "info: no external shirabe server at http://127.0.0.1:7778"
    echo "      ShirabeBar will start the bundled server if available"
  fi
  /usr/bin/open -n "$APP_BUNDLE"
}

case "$MODE" in
  --build-only|build-only)
    echo "built $APP_BUNDLE"
    ;;
  run)
    open_app
    ;;
  --debug|debug)
    lldb -- "$APP_BINARY"
    ;;
  --logs|logs)
    open_app
    /usr/bin/log stream --info --style compact --predicate "process == \"$APP_NAME\""
    ;;
  --telemetry|telemetry)
    open_app
    /usr/bin/log stream --info --style compact --predicate "subsystem == \"$BUNDLE_ID\""
    ;;
  --verify|verify)
    open_app
    sleep 1
    pgrep -x "$APP_NAME" >/dev/null
    ;;
  *)
    echo "usage: $0 [run|--build-only|--debug|--logs|--telemetry|--verify]" >&2
    exit 2
    ;;
esac
