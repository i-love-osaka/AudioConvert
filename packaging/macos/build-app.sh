#!/bin/bash
# Build AudioConvert.app for macOS.
# Usage: bash packaging/macos/build-app.sh
set -euo pipefail

cd "$(dirname "$0")/../.."

# Icon binaries ride along as base64 text (icon-b64/part-*) because
# this project's sync channel carries text only; rebuild the real
# PNG/icns files from that master copy first, so the icon embedded
# in the binary and the bundle icon are both current.
if [ -d "packaging/macos/icon-b64" ]; then
    cat packaging/macos/icon-b64/part-* | base64 -D > "packaging/macos/AppIcon-1024.png"
    cp "packaging/macos/AppIcon-1024.png" "packaging/macos/Icon.icon/Assets/icon.png"
    sips -z 512 512 "packaging/macos/AppIcon-1024.png" --out "crates/audioconvert-app/assets/icon.png" >/dev/null
    ICONSET_TMP="$(mktemp -d)/AppIcon.iconset"
    mkdir -p "${ICONSET_TMP}"
    sips -z 16 16 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_16x16.png" >/dev/null
    sips -z 32 32 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_16x16@2x.png" >/dev/null
    sips -z 32 32 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_32x32.png" >/dev/null
    sips -z 64 64 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_32x32@2x.png" >/dev/null
    sips -z 128 128 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_128x128.png" >/dev/null
    sips -z 256 256 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_128x128@2x.png" >/dev/null
    sips -z 256 256 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_256x256.png" >/dev/null
    sips -z 512 512 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_256x256@2x.png" >/dev/null
    sips -z 512 512 "packaging/macos/AppIcon-1024.png" --out "${ICONSET_TMP}/icon_512x512.png" >/dev/null
    cp "packaging/macos/AppIcon-1024.png" "${ICONSET_TMP}/icon_512x512@2x.png"
    iconutil -c icns "${ICONSET_TMP}" -o "packaging/macos/AppIcon.icns"
    echo "== icon materialized from icon-b64 =="
fi

APP_NAME="AudioConvert"
BIN_NAME="AudioConvert"

echo "== cargo build --release =="
cargo build --release -p audioconvert-app

APP="${APP_NAME}.app"
echo "== assembling ${APP} =="
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
cp "target/release/${BIN_NAME}" "${APP}/Contents/MacOS/${BIN_NAME}"

# If ffmpeg arrived in parts (workaround for large-file sync), join
# the parts back into the single binary first.
if [ ! -f "packaging/macos/ffmpeg" ] && [ -d "packaging/macos/ffmpeg.parts" ]; then
    cat packaging/macos/ffmpeg.parts/part-* > "packaging/macos/ffmpeg"
    chmod +x "packaging/macos/ffmpeg"
    echo "== ffmpeg assembled from parts =="
fi

# Localizations: their mere presence tells macOS this app speaks
# Chinese and English, so system-provided UI (the Quit menu item,
# the folder picker's buttons) follows the system language instead
# of staying in English.
for loc in zh-Hans en; do
    mkdir -p "${APP}/Contents/Resources/${loc}.lproj"
    printf '"CFBundleDisplayName" = "AudioConvert";\n' > "${APP}/Contents/Resources/${loc}.lproj/InfoPlist.strings"
done
echo "== localizations: zh-Hans, en =="

# Bundle ffmpeg (powers every conversion) when the static
# binary has been placed at packaging/macos/ffmpeg.
if [ -f "packaging/macos/ffmpeg" ]; then
    cp "packaging/macos/ffmpeg" "${APP}/Contents/Resources/ffmpeg"
    chmod +x "${APP}/Contents/Resources/ffmpeg"
    echo "== bundled ffmpeg =="
fi

# App icon: prefer the Icon Composer asset. actool compiles
# packaging/macos/Icon.icon into
# Assets.car + Icon.icns, which current macOS renders natively (proper
# squircle, no fallback plate). Without actool/Xcode, fall back to the
# classic .icns under the same name so the build always ships an icon.
ICON_SRC="packaging/macos/Icon.icon"
ACTOOL=""
if command -v xcrun >/dev/null 2>&1; then
    ACTOOL="$(xcrun --find actool 2>/dev/null || true)"
fi
if [ -n "${ACTOOL}" ] && [ -d "${ICON_SRC}" ]; then
    if "${ACTOOL}" "${ICON_SRC}" \
        --compile "${APP}/Contents/Resources" \
        --app-icon Icon \
        --enable-on-demand-resources NO \
        --development-region en \
        --target-device mac \
        --minimum-deployment-target 26.0 \
        --platform macosx \
        --output-partial-info-plist "${TMPDIR:-/tmp}/audioconvert-icon-partial.plist" \
        > "icon-build.log" 2>&1; then
        echo "== icon: Icon Composer (actool) =="
    else
        echo "== icon: actool failed, falling back to classic icns (see icon-build.log) =="
        rm -f "${APP}/Contents/Resources/Assets.car"
        cp "packaging/macos/AppIcon.icns" "${APP}/Contents/Resources/Icon.icns"
    fi
else
    cp "packaging/macos/AppIcon.icns" "${APP}/Contents/Resources/Icon.icns"
fi

# Single source of truth for the version: the app crate's Cargo.toml.
VERSION="$(grep -m1 '^version = ' crates/audioconvert-app/Cargo.toml | sed 's/.*\"\(.*\)\".*/\1/')"
echo "== version ${VERSION} =="

cat > "${APP}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>AudioConvert</string>
    <key>CFBundleIdentifier</key>
    <string>net.mrben.audioconvert</string>
    <key>CFBundleName</key>
    <string>AudioConvert</string>
    <key>CFBundleDisplayName</key>
    <string>AudioConvert</string>
    <key>CFBundleIconFile</key>
    <string>Icon</string>
    <key>CFBundleIconName</key>
    <string>Icon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleLocalizations</key>
    <array>
        <string>en</string>
        <string>zh-Hans</string>
    </array>
    <key>CFBundleShortVersionString</key>
    <string>__VERSION__</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

sed -i '' "s/__VERSION__/${VERSION}/" "${APP}/Contents/Info.plist" 2>/dev/null \
    || sed -i "s/__VERSION__/${VERSION}/" "${APP}/Contents/Info.plist"

# Ad-hoc signature so Gatekeeper lets it run locally.
codesign --force --deep --sign - "${APP}" || echo "(codesign skipped)"

LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
if [ -x "${LSREGISTER}" ]; then
    "${LSREGISTER}" -f "${APP}" || true
fi

echo "== done: ${APP} =="
