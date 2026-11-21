#!/usr/bin/env bash
# Builds Clipstack-x86_64.AppImage in the repository root.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
APP_ID=com.github.jgaertig.CosmicClipboard
TOOLS=$ROOT/build/tools
APPDIR=$ROOT/build/AppDir
OUT=$ROOT/Clipstack-x86_64.AppImage

# Pinned, well-established releases (not the moving "continuous" tags).
APPIMAGETOOL_URL=https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage
RUNTIME_URL=https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-x86_64

mkdir -p "$TOOLS"
[ -x "$TOOLS/appimagetool" ] || { curl -fL -o "$TOOLS/appimagetool" "$APPIMAGETOOL_URL"; chmod +x "$TOOLS/appimagetool"; }
[ -f "$TOOLS/runtime-x86_64" ] || curl -fL -o "$TOOLS/runtime-x86_64" "$RUNTIME_URL"

cargo build --release --locked
TARGET=${CARGO_TARGET_DIR:-target}

rm -rf "$APPDIR"
install -Dm755 "$TARGET/release/cosmic-clipboard" "$APPDIR/usr/bin/cosmic-clipboard"
install -Dm644 packaging/$APP_ID.desktop "$APPDIR/usr/share/applications/$APP_ID.desktop"
install -Dm644 packaging/$APP_ID.metainfo.xml "$APPDIR/usr/share/metainfo/$APP_ID.appdata.xml"
install -Dm644 assets/icon.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"
ln -s usr/share/applications/$APP_ID.desktop "$APPDIR/$APP_ID.desktop"
ln -s usr/share/icons/hicolor/scalable/apps/$APP_ID.svg "$APPDIR/$APP_ID.svg"
ln -s $APP_ID.svg "$APPDIR/.DirIcon"
ln -s usr/bin/cosmic-clipboard "$APPDIR/AppRun"

ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$TOOLS/appimagetool" --no-appstream \
    --runtime-file "$TOOLS/runtime-x86_64" "$APPDIR" "$OUT"
echo "Done: $OUT"
