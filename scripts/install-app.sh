#!/usr/bin/env bash
# Build livehub.app and install it to /Applications so it launches from Spotlight
# and the Dock (no terminal / cargo). Re-run after code changes.
#
# Note: the binary links the system (Homebrew) ffmpeg dylibs by absolute path, so
# this .app runs on THIS Mac (where ffmpeg is installed), not as a portable bundle.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${CARGO_TARGET_DIR:=/Users/rustam/.cargo-target}"
export CARGO_TARGET_DIR
BIN="$CARGO_TARGET_DIR/release/livehub"
APP="${1:-/Applications}/livehub.app"

echo "› building release…"
cargo build --release --manifest-path "$ROOT/Cargo.toml" >/dev/null

echo "› assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/livehub"
[ -f "$ROOT/assets/livehub.icns" ] && cp "$ROOT/assets/livehub.icns" "$APP/Contents/Resources/livehub.icns"
cp "$ROOT/assets/Info.plist" "$APP/Contents/Info.plist"

# Ad-hoc sign so macOS grants a STABLE identity → mic/camera/network permissions
# persist across launches instead of re-prompting.
echo "› codesign (ad-hoc)"
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "✓ installed: $APP   (launch from Spotlight: ‘livehub’)"
