#!/usr/bin/env bash
# Render checked-in terminal frames for local visual review (never committed).
set -euo pipefail
cd "$(dirname "$0")/../.."
out=target/tui-frame-svg
rm -rf "$out" && mkdir -p "$out"
for frame in bin/plak/tests/goldens/*.frame; do
  tools/tui/frame_to_svg.py --input "$frame" \
    --output "$out/$(basename "${frame%.frame}").svg"
done
echo "SVG review set: $out"
