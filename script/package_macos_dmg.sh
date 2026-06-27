#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT_DIR/app/macos/ShirabeBar"
DIST_DIR="$ROOT_DIR/dist/release"
BUILD_ROOT="$DIST_DIR/macos-build"
APP_NAME="ShirabeBar"
BUNDLE_ID="dev.yanfch.ShirabeBar"
MIN_SYSTEM_VERSION="13.0"
VERSION="$(awk -F '"' '/^version = / { print $2; exit }' "$ROOT_DIR/Cargo.toml")"
BUILD_NUMBER="${BUILD_NUMBER:-$VERSION}"
SIGN_IDENTITY="${SIGN_IDENTITY:-}"
NOTARIZE="${NOTARIZE:-0}"
NOTARY_PROFILE="${NOTARY_PROFILE:-}"

case "$(uname -m)" in
  arm64) ARCH="aarch64" ;;
  x86_64) ARCH="x86_64" ;;
  *) ARCH="$(uname -m)" ;;
esac

TARGET="${ARCH}-apple-darwin"
APP_BUNDLE="$BUILD_ROOT/$APP_NAME.app"
APP_CONTENTS="$APP_BUNDLE/Contents"
APP_MACOS="$APP_CONTENTS/MacOS"
APP_RESOURCES="$APP_CONTENTS/Resources"
APP_BINARY="$APP_MACOS/$APP_NAME"
SERVER_BINARY="$APP_MACOS/ShirabeServer"
INFO_PLIST="$APP_CONTENTS/Info.plist"
SOURCE_RESOURCES="$APP_DIR/Sources/ShirabeBar/Resources"
DMG_ROOT="$BUILD_ROOT/dmg-root"
DMG_PATH="$DIST_DIR/$APP_NAME-v$VERSION-$TARGET.dmg"

cd "$ROOT_DIR"

cargo build --release

if [ -f "$ROOT_DIR/ui/package-lock.json" ]; then
  (cd "$ROOT_DIR/ui" && npm ci && npm run build)
else
  (cd "$ROOT_DIR/ui" && npm install && npm run build)
fi

(cd "$APP_DIR" && swift build -c release)
SWIFT_BUILD_DIR="$(cd "$APP_DIR" && swift build -c release --show-bin-path)"

rm -rf "$BUILD_ROOT" "$DMG_PATH"
mkdir -p "$APP_MACOS" "$APP_RESOURCES" "$DMG_ROOT"

cp "$SWIFT_BUILD_DIR/$APP_NAME" "$APP_BINARY"
cp "$ROOT_DIR/target/release/shirabe" "$SERVER_BINARY"
chmod +x "$APP_BINARY" "$SERVER_BINARY"
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
  <key>CFBundleDisplayName</key>
  <string>Shirabe</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$BUILD_NUMBER</string>
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

if [ -n "$SIGN_IDENTITY" ]; then
  codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$SERVER_BINARY"
  codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$APP_BINARY"
  codesign --force --deep --options runtime --timestamp --sign "$SIGN_IDENTITY" "$APP_BUNDLE"
else
  codesign --force --deep --sign - "$APP_BUNDLE"
fi

cp -R "$APP_BUNDLE" "$DMG_ROOT/$APP_NAME.app"
ln -s /Applications "$DMG_ROOT/Applications"

hdiutil create \
  -volname "$APP_NAME" \
  -srcfolder "$DMG_ROOT" \
  -ov \
  -format UDZO \
  "$DMG_PATH"

if [ -n "$SIGN_IDENTITY" ]; then
  codesign --force --timestamp --sign "$SIGN_IDENTITY" "$DMG_PATH"
fi

if [ "$NOTARIZE" = "1" ]; then
  if [ -z "$NOTARY_PROFILE" ]; then
    echo "NOTARY_PROFILE is required when NOTARIZE=1" >&2
    exit 2
  fi
  xcrun notarytool submit "$DMG_PATH" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$DMG_PATH"
fi

shasum -a 256 "$DMG_PATH" >"$DMG_PATH.sha256"
echo "$DMG_PATH"
