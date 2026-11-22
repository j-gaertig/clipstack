#!/usr/bin/env bash
# Builds Clipstack.app and Clipstack-macos.dmg in the repository root, as a
# universal binary for Apple Silicon and Intel Macs. Runs on macOS.
# TARGETS="aarch64-apple-darwin" limits the build to one architecture.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
APP_ID=com.github.jgaertig.CosmicClipboard
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGETS=${TARGETS:-aarch64-apple-darwin x86_64-apple-darwin}
WORK=$ROOT/build/macos
APP=$WORK/Clipstack.app
OUT=$ROOT/Clipstack-macos.dmg
export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}

TARGET_DIR=${CARGO_TARGET_DIR:-target}
BINS=()
for target in $TARGETS; do
    rustup target add "$target" >/dev/null 2>&1 || true
    cargo build --release --locked --target "$target"
    BINS+=("$TARGET_DIR/$target/release/cosmic-clipboard")
done

rm -rf "$WORK"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/Clipstack" "${BINS[@]}"

ICONSET=$WORK/Clipstack.iconset
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    sips -z $size $size packaging/macos/icon.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) packaging/macos/icon.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns -o "$APP/Contents/Resources/Clipstack.icns" "$ICONSET"

# LSUIElement: a background app without Dock icon or menu bar.
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Clipstack</string>
    <key>CFBundleDisplayName</key>
    <string>Clipstack</string>
    <key>CFBundleIdentifier</key>
    <string>$APP_ID</string>
    <key>CFBundleExecutable</key>
    <string>Clipstack</string>
    <key>CFBundleIconFile</key>
    <string>Clipstack</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MACOSX_DEPLOYMENT_TARGET</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>MIT License</string>
</dict>
</plist>
EOF

# Ad-hoc signature (required on Apple Silicon); not notarized.
codesign --force --deep --sign - "$APP"

STAGE=$WORK/dmg
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT"
hdiutil create -volname Clipstack -srcfolder "$STAGE" -ov -format UDZO "$OUT" >/dev/null
echo "Done: $OUT"
