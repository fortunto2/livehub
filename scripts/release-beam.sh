#!/usr/bin/env bash
# Build, sign, notarize and staple beam.app, then emit a distributable zip.
#
# install-beam.sh is the LOCAL flow: it ad-hoc signs so macOS keeps the mic/local-network
# grants between rebuilds. This is the SHIPPING flow — a real Developer ID signature plus
# Apple's notarization ticket, so someone else can download it and just open it.
#
# Needs, once:
#   • a "Developer ID Application" identity in the login keychain (see README)
#   • App Store Connect API credentials for `asc` (asc auth status)
#
# Usage: ./scripts/release-beam.sh [outdir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
APP="$OUT/beam.app"
: "${CARGO_TARGET_DIR:=$HOME/.cargo-target}"
export CARGO_TARGET_DIR
BIN="$CARGO_TARGET_DIR/release/beam"

ID=$(security find-identity -v -p codesigning \
     | awk -F'"' '/Developer ID Application/ {print $2; exit}')
if [ -z "$ID" ]; then
  echo "✗ No 'Developer ID Application' identity in the keychain."
  echo "  Xcode → Settings → Accounts → Manage Certificates → + → Developer ID Application"
  echo "  (only the Account Holder can create one; the API key cannot)"
  exit 1
fi
echo "› signing as: $ID"

echo "› building release…"
cargo build --release -p beam --manifest-path "$ROOT/Cargo.toml" >/dev/null

echo "› assembling $APP"
rm -rf "$APP"; mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/beam"
cp "$ROOT/beam/Info.plist" "$APP/Contents/Info.plist"

echo "› building mp-bridge"
swiftc -O -framework MultipeerConnectivity -framework Foundation \
  "$ROOT/beam/swift/mp-bridge.swift" -o "$APP/Contents/MacOS/mp-bridge"

# Sign inside-out: nested executables first, the bundle last, or the outer signature
# is invalidated by whatever is signed after it.
echo "› codesign (hardened runtime)"
codesign --force --timestamp --options runtime --sign "$ID" \
  "$APP/Contents/MacOS/mp-bridge"
codesign --force --timestamp --options runtime \
  --entitlements "$ROOT/beam/beam.entitlements" --sign "$ID" \
  "$APP/Contents/MacOS/beam"
codesign --force --timestamp --options runtime \
  --entitlements "$ROOT/beam/beam.entitlements" --sign "$ID" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

ZIP="$OUT/beam.zip"
echo "› zipping for notarization"
# ditto, not `zip`: it is the only one that preserves the bundle's symlinks and metadata
# the way the notary service expects.
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"

echo "› notarizing (this takes a few minutes)…"
asc notarization submit --file "$ZIP" --wait --timeout 30m

echo "› stapling the ticket into the app"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"

echo "› re-zipping the stapled app"
rm -f "$ZIP"
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"

echo "› Gatekeeper verdict:"
spctl -a -vvv -t install "$APP" || true

echo "✓ $ZIP — ready to upload to a GitHub release"
