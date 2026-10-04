#!/usr/bin/env bash
# Wraps dari-check in an app bundle so macOS lets it record system audio.
#
# macOS 14.6+ asks before a process records what the system plays, and only if the process
# belongs to an app that declares NSAudioCaptureUsageDescription; a bare binary run from a
# terminal is silently given silence. The bundle is ad-hoc signed so the permission sticks to it.
#
# Usage: scripts/crosscheck/mac-check-app.sh [PATH/TO/dari-check] [OUT_DIR]
set -euo pipefail
binary=${1:-target/debug/dari-check}
out=${2:-target/crosscheck}
app="$out/DariCheck.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$binary" "$app/Contents/MacOS/dari-check"
cat >"$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>dev.dari.check</string>
	<key>CFBundleName</key>
	<string>DariCheck</string>
	<key>CFBundleExecutable</key>
	<string>dari-check</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>LSUIElement</key>
	<true/>
	<key>NSAudioCaptureUsageDescription</key>
	<string>dari-check records this Mac's sound to check that a remote viewer hears it.</string>
	<key>NSLocalNetworkUsageDescription</key>
	<string>dari-check talks to the other side of a cross-device check.</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$app" >/dev/null
echo "$app"
