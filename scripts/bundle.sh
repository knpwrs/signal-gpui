#!/bin/sh
# Wraps a built ziggy binary in "Ziggy.app" next to it and signs it. macOS
# only allows notifications from an app bundle.
#
#   scripts/bundle.sh                 # target/release/ziggy
#   scripts/bundle.sh path/to/ziggy
#
# Prints the bundled executable's path. Signs with, in order:
#   - $SIGNAL_GPUI_SIGN_IDENTITY, if set;
#   - an "Apple Development" certificate (free with an Apple ID in Xcode),
#     whose team ID lets one "Always Allow" cover every later build;
#   - the self-signed "signal-gpui dev" certificate;
#   - ad hoc.
# Only a team ID stops the Keychain prompt after each rebuild: macOS lists
# apps without one by their build hash (see scripts/setup-dev-signing.sh).
set -eu
BIN="${1:-target/release/ziggy}"
[ -x "$BIN" ] || { echo "no binary at $BIN; build it first" >&2; exit 1; }
APP="$(dirname "$BIN")/Ziggy.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The app icon, from its PNGs (made by scripts/app-icon.py).
ICONS="$(cd "$(dirname "$0")/.." && pwd)/crates/app/assets/app-icon"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
for n in 16 32 128 256 512; do
    cp "$ICONS/${n}x${n}.png" "$ICONSET/icon_${n}x${n}.png"
    cp "$ICONS/$((n * 2))x$((n * 2)).png" "$ICONSET/icon_${n}x${n}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"
# A copy-on-write clone where the file system supports it.
cp -c "$BIN" "$APP/Contents/MacOS/ziggy" 2>/dev/null || cp "$BIN" "$APP/Contents/MacOS/ziggy"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>io.github.knpwrs.ziggy</string>
    <key>CFBundleName</key><string>Ziggy</string>
    <key>CFBundleDisplayName</key><string>Ziggy</string>
    <key>CFBundleExecutable</key><string>ziggy</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSCameraUsageDescription</key><string>Ziggy uses your camera for video calls.</string>
    <key>NSMicrophoneUsageDescription</key><string>Ziggy uses your microphone for calls and voice messages.</string>
    <!-- sgnl:// links (Signal Desktop registers the scheme the same way:
         app.setAsDefaultProtocolClient('sgnl') in app/main.main.ts), and
         signalcaptcha:// for a solved captcha (setAsDefaultProtocolClient('signalcaptcha')). -->
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key><string>io.github.knpwrs.ziggy.sgnl</string>
            <key>CFBundleURLSchemes</key><array><string>sgnl</string></array>
        </dict>
        <dict>
            <key>CFBundleURLName</key><string>io.github.knpwrs.ziggy.captcha</string>
            <key>CFBundleURLSchemes</key><array><string>signalcaptcha</string></array>
        </dict>
    </array>
</dict>
</plist>
PLIST
IDENTITIES=$(security find-identity -v -p codesigning 2>/dev/null)
APPLE_DEV=$(printf '%s\n' "$IDENTITIES" | sed -n 's/.*"\(Apple Development: [^"]*\)".*/\1/p' | head -1)
IDENTITY="${SIGNAL_GPUI_SIGN_IDENTITY:-${APPLE_DEV:-signal-gpui dev}}"
if printf '%s\n' "$IDENTITIES" | grep -q "\"$IDENTITY\""; then
    codesign --force --sign "$IDENTITY" --identifier io.github.knpwrs.ziggy "$APP" >/dev/null 2>&1 \
        || echo "warning: couldn't sign $APP" >&2
else
    codesign --force --sign - --identifier io.github.knpwrs.ziggy "$APP" >/dev/null 2>&1 \
        || echo "warning: couldn't sign $APP" >&2
fi
echo "$APP/Contents/MacOS/ziggy"
