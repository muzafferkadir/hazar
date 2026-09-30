#!/usr/bin/env bash
# Sürümü TEK yerden bump eder, her yerde senkron tutar.
# Kullanım: bash scripts/bump-version.sh [patch|minor|major|X.Y.Z]
#
# Dokunduğu yerler:
#   package.json · Cargo.toml (workspace) · src-tauri/Cargo.toml
#   src-tauri/tauri.conf.json · extension/manifest.json · extension/manifest.firefox.json
#   (Cargo.lock tazelenir)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CURRENT="$(node -p "require('$ROOT/package.json').version")"
ARG="${1:-patch}"

case "$ARG" in
  major | minor | patch)
    IFS=. read -r MA MI PA <<< "$CURRENT"
    case "$ARG" in
      major)
        MA=$((MA + 1))
        MI=0
        PA=0
        ;;
      minor)
        MI=$((MI + 1))
        PA=0
        ;;
      patch) PA=$((PA + 1)) ;;
    esac
    NEW="$MA.$MI.$PA"
    ;;
  *)
    if [[ ! "$ARG" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
      echo "geçersiz sürüm: $ARG (patch|minor|major|X.Y.Z bekleniyor)" >&2
      exit 1
    fi
    NEW="$ARG"
    ;;
esac

if [ "$NEW" = "$CURRENT" ]; then
  echo "not: package.json zaten $NEW — diğer dosyalar senkronlanıyor" >&2
fi

# JSON dosyaları: package.json + tauri.conf.json
node -e "
  const fs = require('fs');
  const set = (file, version) => {
    const data = JSON.parse(fs.readFileSync(file, 'utf8'));
    data.version = version;
    fs.writeFileSync(file, JSON.stringify(data, null, 2) + '\n');
  };
  set('$ROOT/package.json', '$NEW');
  set('$ROOT/src-tauri/tauri.conf.json', '$NEW');
"

# Cargo.toml'ler: dosyanın KENDİ mevcut sürümünü bulup ilk `version = "..."`
# satırını değiştir (macOS sed'de boş RE desteklenmiyor, açık yaz).
for file in "$ROOT/Cargo.toml" "$ROOT/src-tauri/Cargo.toml"; do
  file_current="$(grep -m1 '^version = "' "$file" | sed -E 's/.*"([^"]+)".*/\1/')"
  if [ -n "$file_current" ] && [ "$file_current" != "$NEW" ]; then
    sed -i '' "1,/^version = \"$file_current\"/s/^version = \"$file_current\"/version = \"$NEW\"/" "$file"
  fi
done

# Eklenti manifestleri (app sürümüyle aynı kalsın)
node -e "
  const fs = require('fs');
  for (const name of ['manifest.json', 'manifest.firefox.json']) {
    const path = '$ROOT/extension/' + name;
    if (!fs.existsSync(path)) continue;
    const manifest = JSON.parse(fs.readFileSync(path, 'utf8'));
    manifest.version = '$NEW';
    fs.writeFileSync(path, JSON.stringify(manifest, null, 2) + '\n');
  }
"

# Cargo.lock'a yansıması için metadata tazele
(cd "$ROOT" && cargo metadata --no-deps --format-version 1 >/dev/null 2>&1) || true

echo "sürüm: $CURRENT → $NEW"
grep -Hn '^version = "' "$ROOT/Cargo.toml" "$ROOT/src-tauri/Cargo.toml"
grep -Hn '"version": "' "$ROOT/package.json" "$ROOT/src-tauri/tauri.conf.json" "$ROOT/extension/manifest.json" "$ROOT/extension/manifest.firefox.json"
echo "paketlemek için: bash scripts/package-extension.sh"
