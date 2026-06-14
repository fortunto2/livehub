#!/usr/bin/env bash
# Build beam.app (the menu-bar audio broadcaster) and install it to /Applications.
# LSUIElement → it shows ONLY in the status bar (no Dock icon). Re-run after code
# changes. Launch from Spotlight: ‘beam’ (or add to Login Items).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${CARGO_TARGET_DIR:=/Users/rustam/.cargo-target}"
export CARGO_TARGET_DIR
BIN="$CARGO_TARGET_DIR/release/beam"
APP="${1:-/Applications}/beam.app"

echo "› building release…"
cargo build --release -p beam --manifest-path "$ROOT/Cargo.toml" >/dev/null

echo "› assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/beam"
cp "$ROOT/beam/Info.plist" "$APP/Contents/Info.plist"

# Ad-hoc sign for a STABLE identity so mic/local-network permissions persist.
echo "› codesign (ad-hoc)"
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "✓ installed: $APP   (launch from Spotlight: ‘beam’ — it lives in the menu bar)"
