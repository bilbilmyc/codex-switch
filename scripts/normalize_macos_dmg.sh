#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <dmg-path>" >&2
  exit 2
fi

dmg_path="$1"
if [[ ! -f "$dmg_path" ]]; then
  echo "DMG not found: $dmg_path" >&2
  exit 2
fi

work_dir="$(mktemp -d -t codex-switch-dmg)"
rw_dmg="$work_dir/normalized-rw.dmg"
output_dmg="$work_dir/normalized.dmg"
mount_dir="$work_dir/mount"
mounted=false

cleanup() {
  if [[ "$mounted" == true ]]; then
    hdiutil detach "$mount_dir" -quiet || true
  fi
  rm -rf "$work_dir"
}
trap cleanup EXIT

hdiutil convert "$dmg_path" -format UDRW -o "$rw_dmg" -quiet
mkdir "$mount_dir"
hdiutil attach "$rw_dmg" -readwrite -nobrowse -mountpoint "$mount_dir" -quiet
mounted=true

# cargo-packager uses the application icon as the disk-volume icon. Finder then
# overlays volume state on it, which makes the application mark look corrupted.
rm -f "$mount_dir/.VolumeIcon.icns"
SetFile -a c "$mount_dir"

app_path="$(find "$mount_dir" -maxdepth 1 -type d -name '*.app' -print -quit)"
if [[ -z "$app_path" ]]; then
  echo "No application bundle found in DMG: $dmg_path" >&2
  exit 1
fi

# cargo-packager signs before it finishes copying bundle resources. Re-signing
# here makes the packaged app's resource seal match its final contents.
codesign --force --deep --sign - "$app_path"
codesign --verify --deep --strict --verbose=2 "$app_path"

hdiutil detach "$mount_dir" -quiet
mounted=false
hdiutil convert "$rw_dmg" -format UDZO -imagekey zlib-level=9 -o "$output_dmg" -quiet
mv "$output_dmg" "$dmg_path"
