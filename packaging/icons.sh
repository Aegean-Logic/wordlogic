#!/usr/bin/env bash
# Regenerate every app icon from assets/app-icon/wordcraft.svg (48 px and up) and
# assets/app-icon/wordcraft-small.svg (the simplified glyph, 32 px and below).
#
# Needs: resvg (brew install resvg / cargo install resvg). The .ico and .icns are packed with
# `cargo xtask ico` / `cargo xtask icns`, so this runs on any platform. The outputs are committed,
# so builds and packaging never need these tools.
#
#   packaging/icons.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="$ROOT/assets/app-icon"
SVG="$DIR/wordcraft.svg"
SMALL_SVG="$DIR/wordcraft-small.svg"
ID="uk.co.aegeanlogic.wordlogic"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v resvg >/dev/null || { echo "error: resvg not found (brew install resvg)" >&2; exit 1; }

# The artwork is a full-bleed 512 tile (rx=112). Windows and Linux use it as is. macOS icons sit on
# Apple's grid: an 824/1024 body with a transparent margin, so widen the viewBox to 512*1024/824.
mac() {
  sed 's/viewBox="0 0 512 512"/viewBox="-62.136 -62.136 636.272 636.272"/' "$1" >"$2"
  grep -q 'viewBox="-62.136' "$2" || { echo "error: expected viewBox=\"0 0 512 512\" in $1" >&2; exit 1; }
}
MAC="$TMP/mac.svg"
MAC_SMALL="$TMP/mac-small.svg"
mac "$SVG" "$MAC"
mac "$SMALL_SVG" "$MAC_SMALL"

# render <full.svg> <small.svg> <px> <out.png>: the small glyph at 32 px and below, else the full art.
render() {
  local src="$1"
  [ "$3" -le 32 ] && src="$2"
  resvg -w "$3" -h "$3" "$src" "$4" </dev/null
}

render "$SVG" "$SMALL_SVG" 1024 "$DIR/wordcraft-1024.png"
# Runtime window/Dock icon on macOS (embedded by apps/wordcraft); other platforms use hicolor 256.
render "$MAC" "$MAC_SMALL" 512 "$DIR/wordcraft-macos-512.png"

# Linux hicolor theme.
for s in 16 24 32 48 64 128 256 512; do
  mkdir -p "$DIR/hicolor/${s}x${s}/apps"
  render "$SVG" "$SMALL_SVG" "$s" "$DIR/hicolor/${s}x${s}/apps/$ID.png"
done
mkdir -p "$DIR/hicolor/scalable/apps"
cp "$SVG" "$DIR/hicolor/scalable/apps/$ID.svg"

# Windows .ico.
ICO_PNGS=()
for s in 16 20 24 32 40 48 64 128 256; do
  render "$SVG" "$SMALL_SVG" "$s" "$TMP/ico-$s.png"
  ICO_PNGS+=("$TMP/ico-$s.png")
done
(cd "$ROOT" && cargo run -q -p xtask -- ico "$DIR/wordcraft.ico" "${ICO_PNGS[@]}")

# macOS .icns: one PNG per pixel size; xtask files each under its 1x and @2x slots.
ICNS_PNGS=()
for s in 16 32 64 128 256 512 1024; do
  render "$MAC" "$MAC_SMALL" "$s" "$TMP/icns-$s.png"
  ICNS_PNGS+=("$TMP/icns-$s.png")
done
(cd "$ROOT" && cargo run -q -p xtask -- icns "$DIR/wordcraft.icns" "${ICNS_PNGS[@]}")
echo "icons written to $DIR"
