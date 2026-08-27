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

hdiutil detach "$mount_dir" -quiet
mounted=false
hdiutil convert "$rw_dmg" -format UDZO -imagekey zlib-level=9 -o "$output_dmg" -quiet
mv "$output_dmg" "$dmg_path"
