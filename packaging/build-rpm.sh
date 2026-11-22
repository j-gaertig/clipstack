#!/usr/bin/env bash
# Builds Clipstack-<arch>.rpm (Fedora, openSUSE, RHEL, ...) in the repository
# root from a release build. Needs `rpmbuild`.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
APP_ID=com.github.jgaertig.CosmicClipboard
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)
TOP=$ROOT/build/rpm
OUT=$ROOT/Clipstack-$ARCH.rpm

cargo build --release --locked
TARGET=$(realpath "${CARGO_TARGET_DIR:-target}")

rm -rf "$TOP"
mkdir -p "$TOP"/{BUILD,RPMS,SPECS}
# RPM versions can't contain "-" (pre-releases like 1.0.0-beta.1).
cat > "$TOP/SPECS/clipstack.spec" <<EOF
Name:           clipstack
Version:        ${VERSION//-/~}
Release:        1
Summary:        Super+V clipboard history with iPad drawing
License:        MIT
URL:            https://github.com/j-gaertig/clipstack
Requires:       libxkbcommon
Recommends:     libnotify
%global debug_package %{nil}
%global __strip /bin/true

%description
Remembers copied text, images and files and shows them at the text cursor
or mouse pointer with Super+V. Entries can be pinned, searched and deleted;
images can be sent to an iPad to draw on them with the Apple Pencil.

%install
install -Dm755 $TARGET/release/cosmic-clipboard %{buildroot}%{_bindir}/cosmic-clipboard
install -Dm644 $ROOT/packaging/$APP_ID.desktop %{buildroot}%{_datadir}/applications/$APP_ID.desktop
install -Dm644 $ROOT/packaging/$APP_ID.metainfo.xml %{buildroot}%{_datadir}/metainfo/$APP_ID.metainfo.xml
install -Dm644 $ROOT/assets/icon.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/$APP_ID.svg
install -Dm644 $ROOT/LICENSE %{buildroot}%{_datadir}/licenses/clipstack/LICENSE

%files
%{_bindir}/cosmic-clipboard
%{_datadir}/applications/$APP_ID.desktop
%{_datadir}/metainfo/$APP_ID.metainfo.xml
%{_datadir}/icons/hicolor/scalable/apps/$APP_ID.svg
%license %{_datadir}/licenses/clipstack/LICENSE
EOF

rpmbuild --define "_topdir $TOP" -bb "$TOP/SPECS/clipstack.spec"
cp "$TOP"/RPMS/*/clipstack-*.rpm "$OUT"
echo "Done: $OUT"
