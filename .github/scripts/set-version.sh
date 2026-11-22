#!/usr/bin/env bash
# Sets the version (without "v") in Cargo.toml, Cargo.lock and the AppStream
# metadata. Portable across GNU/BSD tools (Linux, macOS, Git Bash on Windows).
# Usage: set-version.sh <version>
set -euo pipefail

VERSION=$1
cd "$(dirname "$0")/../.."
DATE=$(date -u +%F)
VERSION=$VERSION perl -0pi -e 's/^version = ".*?"/version = "$ENV{VERSION}"/m' Cargo.toml
VERSION=$VERSION perl -0pi -e 's/(name = "cosmic-clipboard"\nversion = )".*?"/$1"$ENV{VERSION}"/' Cargo.lock
VERSION=$VERSION DATE=$DATE perl -pi -e 's|<release version="[^"]*" date="[^"]*"/>|<release version="$ENV{VERSION}" date="$ENV{DATE}"/>|' packaging/*.metainfo.xml
