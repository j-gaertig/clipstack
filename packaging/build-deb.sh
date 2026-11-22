#!/usr/bin/env bash
# Builds Clipstack-<arch>.deb (Debian, Ubuntu, Mint, Pop!_OS, ...) in the
# repository root from a release build.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
APP_ID=com.github.jgaertig.CosmicClipboard
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(dpkg --print-architecture)
PKG=$ROOT/build/deb
OUT=$ROOT/Clipstack-$ARCH.deb

cargo build --release --locked
TARGET=${CARGO_TARGET_DIR:-target}

rm -rf "$PKG"
install -Dm755 "$TARGET/release/cosmic-clipboard" "$PKG/usr/bin/cosmic-clipboard"
install -Dm644 packaging/$APP_ID.desktop "$PKG/usr/share/applications/$APP_ID.desktop"
install -Dm644 packaging/$APP_ID.metainfo.xml "$PKG/usr/share/metainfo/$APP_ID.metainfo.xml"
install -Dm644 assets/icon.svg "$PKG/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"
install -Dm644 LICENSE "$PKG/usr/share/doc/clipstack/copyright"

mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<EOF
Package: clipstack
Version: $VERSION
Architecture: $ARCH
Maintainer: j-gaertig <262263607+j-gaertig@users.noreply.github.com>
Installed-Size: $(du -sk "$PKG/usr" | cut -f1)
Depends: libc6, libgcc-s1, libxkbcommon0
Recommends: libnotify-bin, xdg-desktop-portal
Section: utils
Priority: optional
Homepage: https://github.com/j-gaertig/clipstack
Description: Super+V clipboard history with iPad drawing
 Remembers copied text, images and files and shows them at the text cursor
 or mouse pointer with Super+V. Entries can be pinned, searched and deleted;
 images can be sent to an iPad to draw on them with the Apple Pencil.
EOF

dpkg-deb --root-owner-group --build "$PKG" "$OUT"
echo "Done: $OUT"
