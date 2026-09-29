#!/usr/bin/env bash
# Hazar installer for macOS.
#   curl -fsSL https://raw.githubusercontent.com/muzafferkadir/hazar/main/install.sh | bash
# Downloads the latest .dmg from GitHub Releases and installs Hazar.app into
# /Applications. The build is ad-hoc signed, so the quarantine flag is cleared.
set -euo pipefail

REPO="muzafferkadir/hazar"
APP="Hazar.app"

if [ "$(uname)" != "Darwin" ]; then
  echo "This installer is for macOS. On Windows use install.ps1 (see the README)." >&2
  exit 1
fi

echo "Fetching the latest Hazar release…"
API="https://api.github.com/repos/$REPO/releases/latest"
DMG_URL="$(curl -fsSL "$API" | grep -o 'https://[^"]*\.dmg' | head -1)"
if [ -z "$DMG_URL" ]; then
  echo "No .dmg found in the latest release." >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading $(basename "$DMG_URL")…"
curl -fsSL "$DMG_URL" -o "$TMP/hazar.dmg"

echo "Installing to /Applications…"
hdiutil attach "$TMP/hazar.dmg" -nobrowse -quiet -mountpoint "$TMP/mnt"
rm -rf "/Applications/$APP"
cp -R "$TMP/mnt/$APP" "/Applications/"
hdiutil detach "$TMP/mnt" -quiet

# Ad-hoc signed build: clear the quarantine flag so Gatekeeper does not block it.
xattr -dr com.apple.quarantine "/Applications/$APP" 2>/dev/null || true

echo "Hazar installed to /Applications/$APP"
