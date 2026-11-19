#!/usr/bin/env bash
# Build byob.app and a .dmg with it (and an /Applications link to drag it onto). macOS only:
# uses hdiutil, plutil and otool (and codesign when signing), all on GitHub's macOS runners.
#
#   scripts/macos-bundle.sh BYOB_BIN HELPER_BIN VERSION TARGET [OUT_DIR]
#
# Writes OUT_DIR/byob-VERSION-TARGET.dmg (OUT_DIR defaults to the current directory) and prints
# its path. Run from the repository root (it copies the licenses, README and the helper's NOTICE).
#
# Signing: with MACOS_SIGN_IDENTITY set (a "Developer ID Application: …" identity in the
# keychain), the helper is signed with scripts/macos-artifact-host.entitlements, then byob, then
# the app, all with the hardened runtime and a secure timestamp, then the dmg. Notarizing the
# dmg is a separate step (see .github/workflows/release.yml). Unset: the app is signed ad hoc
# (no identity), which Gatekeeper treats as an unidentified developer's.
set -euo pipefail

if [[ $# -lt 4 || $# -gt 5 ]]; then
  echo "usage: $0 BYOB_BIN HELPER_BIN VERSION TARGET [OUT_DIR]" >&2
  exit 2
fi
bin=$1 helper=$2 version=$3 target=$4 out=${5:-.}
bundle_id=io.github.scronkfinkle.byob

for f in "$bin" "$helper" LICENSE-MIT LICENSE-APACHE README.md host/artifact-host/NOTICE scripts/macos-artifact-host.entitlements; do
  [[ -f "$f" ]] || { echo "$0: $f not found" >&2; exit 1; }
done

# The lowest macOS both executables run on (LC_BUILD_VERSION minos, or the older
# LC_VERSION_MIN_MACOSX), so the Finder refuses the app up front on older systems.
minos() {
  otool -l "$1" | awk '
    $1 == "cmd" { cmd = $2 }
    cmd == "LC_BUILD_VERSION" && $1 == "minos" { print $2; exit }
    cmd == "LC_VERSION_MIN_MACOSX" && $1 == "version" { print $2; exit }'
}
min_os=$( { minos "$bin"; minos "$helper"; } | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)
[[ -n "$min_os" ]] || { echo "$0: couldn't read the minimum macOS version of $bin" >&2; exit 1; }

# CFBundleVersion is numbers and dots only: 0.4.0-rc.1 becomes 0.4.0.
plain_version=${version%%[-+]*}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app=$work/dmg/byob.app
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
# byob looks for the helper next to its own executable.
cp "$bin" "$app/Contents/MacOS/byob"
cp "$helper" "$app/Contents/MacOS/byob-artifact-host"
chmod 755 "$app/Contents/MacOS/byob" "$app/Contents/MacOS/byob-artifact-host"
cp LICENSE-MIT LICENSE-APACHE README.md "$app/Contents/Resources/"
# The helper links Servo/SpiderMonkey (MPL-2.0): NOTICE says where their source is.
cp host/artifact-host/NOTICE "$app/Contents/Resources/NOTICE-byob-artifact-host"
printf 'APPL????' > "$app/Contents/PkgInfo"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>byob</string>
	<key>CFBundleExecutable</key>
	<string>byob</string>
	<key>CFBundleIdentifier</key>
	<string>$bundle_id</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>byob</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$plain_version</string>
	<key>CFBundleVersion</key>
	<string>$plain_version</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key>
	<string>$min_os</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >&2

if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
  sign=(codesign --force --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY")
  "${sign[@]}" --entitlements scripts/macos-artifact-host.entitlements "$app/Contents/MacOS/byob-artifact-host" >&2
  "${sign[@]}" "$app/Contents/MacOS/byob" >&2
  "${sign[@]}" "$app" >&2
else
  # Ad hoc, so the bundle is sealed: an unsealed one downloaded from the web is reported
  # as "damaged", an ad hoc one as from an unidentified developer, which the user can open.
  codesign --force --sign - "$app/Contents/MacOS/byob-artifact-host" >&2
  codesign --force --sign - "$app" >&2
fi
codesign --verify --strict --verbose=2 "$app" >&2

ln -s /Applications "$work/dmg/Applications"
mkdir -p "$out"
dmg=$out/byob-$version-$target.dmg
rm -f "$dmg"
# hdiutil on CI runners now and then fails with "Resource busy"; try again.
for attempt in 1 2 3 4 5; do
  if hdiutil create -volname byob -srcfolder "$work/dmg" -fs HFS+ -format UDZO -ov "$dmg" >&2; then
    break
  fi
  [[ $attempt == 5 ]] && { echo "$0: hdiutil create failed" >&2; exit 1; }
  sleep $((attempt * 5))
done

if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
  codesign --force --timestamp --sign "$MACOS_SIGN_IDENTITY" "$dmg" >&2
fi
echo "$dmg"
