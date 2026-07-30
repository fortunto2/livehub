#!/usr/bin/env bash
# Build beam.app (the small windowed audio broadcaster) and install it to
# /Applications. Re-run after code changes. Launch from Spotlight: ‘beam’.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${CARGO_TARGET_DIR:=$HOME/.cargo-target}"
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

echo "› building mp-bridge (Multipeer/AWDL helper)"
swiftc -O -framework MultipeerConnectivity -framework Foundation \
  "$ROOT/beam/swift/mp-bridge.swift" -o "$APP/Contents/MacOS/mp-bridge" \
  && echo "  ✓ mp-bridge" || echo "  ⚠ mp-bridge build failed (Multipeer disabled)"

# Ad-hoc sign for a STABLE identity so mic/local-network permissions persist.
echo "› codesign (ad-hoc)"
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "✓ installed: $APP   (launch from Spotlight: ‘beam’ — a small always-on-top window)"
