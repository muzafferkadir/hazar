#!/usr/bin/env bash
# Tarayıcı eklentisini "unpacked" zip olarak paketler.
#
# Store yayını yok: kullanıcı zip'i indirir, klasöre çıkarır ve tarayıcısına
# "paketlenmemiş öğe yükle" der. Kullanım:
#   bash scripts/package-extension.sh [version]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:-$(node -p "require('$ROOT/package.json').version")}"
# `git tag` v-prefix'li gelir (v0.1.1) → tek "v" kalacak şekilde normalize et.
VERSION="${VERSION#v}"
OUT_DIR="$ROOT/dist"
OUT="$OUT_DIR/hazar-extension-v${VERSION}.zip"

mkdir -p "$OUT_DIR"
rm -f "$OUT"

cd "$ROOT/extension"
# manifest.json zip'in kökünde olmalı (tarayıcılar bunu bekler).
zip -q -r "$OUT" . \
  -x "*.DS_Store" \
  -x "__MACOSX/*" \
  -x "test/*" \
  -x "*.log"

echo "paketlendi: $OUT"
unzip -l "$OUT" | tail -4
