#!/bin/zsh
# Builds the installer: dist/Siphon-<version>.dmg, one disk image holding
# Siphon.app and a shortcut to the Applications folder.
#
#   scripts/make-dmg.sh              build the application, then the image
#   scripts/make-dmg.sh --no-build   wrap the dist/Siphon.app that is there
#
# Opened on another Mac, an image that is not notarised is stopped by
# Gatekeeper. With a Developer ID identity in SIPHON_SIGN_IDENTITY (see
# bundle-macos.sh) and a notarytool keychain profile in SIPHON_NOTARY_PROFILE
# (`xcrun notarytool store-credentials <name>`), the image is signed, sent to
# Apple, and the ticket stapled to it.
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "${1:-}" != "--no-build" ]]; then
  scripts/bundle-macos.sh
fi

APP="dist/Siphon.app"
[[ -d "$APP" ]] || { echo "no $APP: run scripts/bundle-macos.sh first" >&2; exit 1; }
VERSION=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")
DMG="dist/Siphon-$VERSION.dmg"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
# ditto keeps the signature, attributes and symbolic links of a bundle whole.
ditto "$APP" "$STAGE/Siphon.app"
ln -s /Applications "$STAGE/Applications"

rm -f "$DMG"
hdiutil create -volname "Siphon" -srcfolder "$STAGE" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov -quiet "$DMG"
hdiutil verify -quiet "$DMG"

if [[ -n "${SIPHON_SIGN_IDENTITY:-}" ]]; then
  codesign --force --timestamp --sign "$SIPHON_SIGN_IDENTITY" "$DMG"
  if [[ -n "${SIPHON_NOTARY_PROFILE:-}" ]]; then
    xcrun notarytool submit "$DMG" --keychain-profile "$SIPHON_NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
  fi
fi

echo "built $DMG ($(du -h "$DMG" | cut -f1))"
