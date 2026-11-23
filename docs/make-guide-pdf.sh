#!/usr/bin/env bash
# Renders PAPER-GUIDE.md to PAPER-GUIDE.pdf (needs pandoc and Google Chrome).
set -euo pipefail
cd "$(dirname "$0")/.."
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
pandoc PAPER-GUIDE.md -f gfm -t html5 --standalone --embed-resources \
  --css docs/guide.css --metadata pagetitle="Nebula: guide for the paper and demo" -o "$TMP/guide.html"
"$CHROME" --headless=new --disable-gpu --no-pdf-header-footer \
  --print-to-pdf="$PWD/PAPER-GUIDE.pdf" "file://$TMP/guide.html" 2>/dev/null
echo "wrote PAPER-GUIDE.pdf"
