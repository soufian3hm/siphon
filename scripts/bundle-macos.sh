#!/bin/zsh
# Builds dist/Siphon.app: the application, its browser helper, its icon and
# a copy of the extension.
#
#   scripts/bundle-macos.sh
#
# The bundle runs on the processor it was built on. With Rust's other macOS
# target installed (`rustup target add x86_64-apple-darwin` on Apple silicon)
# it is built for both and runs on Intel Macs too.
#
# Signing is ad hoc, which is enough for the Mac that built it. For other
# Macs, set SIPHON_SIGN_IDENTITY to a "Developer ID Application: …" identity
# from your keychain; the bundle is then signed with the hardened runtime
# and a timestamp, as notarisation requires.
set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

APP="dist/Siphon.app"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BINARIES=(siphon siphon-host)

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

INSTALLED=$(rustup target list --installed 2>/dev/null || true)
if print -r -- "$INSTALLED" | grep -qx 'aarch64-apple-darwin' && print -r -- "$INSTALLED" | grep -qx 'x86_64-apple-darwin'; then
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --target "$target" -p siphon -p siphon-host
  done
  for binary in $BINARIES; do
    lipo -create "target/aarch64-apple-darwin/release/$binary" "target/x86_64-apple-darwin/release/$binary" -output "$APP/Contents/MacOS/$binary"
  done
else
  cargo build --release -p siphon -p siphon-host
  for binary in $BINARIES; do
    cp "target/release/$binary" "$APP/Contents/MacOS/"
  done
fi
cp -R extension "$APP/Contents/Resources/extension"

# The icon is drawn by the application itself.
ICONSET="$(mktemp -d)/Siphon.iconset"
"$APP/Contents/MacOS/siphon" --write-icon "$ICONSET"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Siphon.icns"
rm -rf "$(dirname "$ICONSET")"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Siphon</string>
  <key>CFBundleDisplayName</key><string>Siphon</string>
  <key>CFBundleIdentifier</key><string>app.siphon.Siphon</string>
  <key>CFBundleExecutable</key><string>siphon</string>
  <key>CFBundleIconFile</key><string>Siphon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

if [[ -n "${SIPHON_SIGN_IDENTITY:-}" ]]; then
  # Inside out: the helper first, then the bundle that contains it.
  codesign --force --options runtime --timestamp --sign "$SIPHON_SIGN_IDENTITY" "$APP/Contents/MacOS/siphon-host"
  codesign --force --options runtime --timestamp --sign "$SIPHON_SIGN_IDENTITY" "$APP"
else
  codesign --force --deep --sign - "$APP" 2>/dev/null
fi
codesign --verify --deep --strict "$APP"
echo "built $APP ($(du -sh "$APP" | cut -f1), $(lipo -archs "$APP/Contents/MacOS/siphon"))"
