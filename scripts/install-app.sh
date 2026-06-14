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

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>livehub</string>
	<key>CFBundleDisplayName</key><string>livehub</string>
	<key>CFBundleIdentifier</key><string>co.superduperai.livehub</string>
	<key>CFBundleExecutable</key><string>livehub</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>0.1</string>
	<key>CFBundleVersion</key><string>1</string>
	<key>LSMinimumSystemVersion</key><string>12.0</string>
	<key>NSHighResolutionCapable</key><true/>
	<key>LSApplicationCategoryType</key><string>public.app-category.music</string>
	<key>NSMicrophoneUsageDescription</key><string>livehub mixes a live microphone into the master.</string>
	<key>NSCameraUsageDescription</key><string>livehub can use a camera as a video-deck source.</string>
	<key>NSLocalNetworkUsageDescription</key><string>livehub broadcasts audio to your phone (reelcam) over the local network.</string>
</dict>
</plist>
PLIST

# Ad-hoc sign so macOS grants a STABLE identity → mic/camera/network permissions
# persist across launches instead of re-prompting.
echo "› codesign (ad-hoc)"
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true

echo "✓ installed: $APP   (launch from Spotlight: ‘livehub’)"
