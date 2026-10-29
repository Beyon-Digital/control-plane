#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p build
swiftc -parse-as-library -O -target arm64-apple-macosx15.0 DesktopHelper.swift DesktopVideo.swift -o build/control-plane-desktop
if [ -n "${SIGNING_IDENTITY:-}" ]; then
    codesign --force --options runtime --timestamp --sign "$SIGNING_IDENTITY" build/control-plane-desktop
else
    codesign --force --sign - build/control-plane-desktop
fi
echo "Built $(pwd)/build/control-plane-desktop"
