#!/bin/sh
# Creates a local arm64 app bundle. Distribution signing/notarization is separate.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$ROOT"
cargo build --release --workspace
npm --prefix sidecars/claude ci --ignore-scripts
npm --prefix sidecars/claude run build
sh helpers/macos/build.sh
python3 apple/generate_project.py
xcodebuild -skipPackagePluginValidation -quiet -project apple/ControlPlane.xcodeproj -scheme ControlPlane \
  -configuration Release -destination 'generic/platform=macOS' \
  -derivedDataPath apple/.build/package-macos ARCHS=arm64 CODE_SIGNING_ALLOWED=NO build
mkdir -p dist
APP=${CONTROL_PLANE_APP_OUTPUT:-dist/ControlPlane.app}
if [ -e "$APP" ] && [ "${CONTROL_PLANE_REPLACE_GENERATED_APP:-0}" != 1 ]; then
  echo "$APP exists; move it aside before packaging again" >&2
  exit 1
fi
ditto apple/.build/package-macos/Build/Products/Release/ControlPlane.app "$APP"
RESOURCES="$APP/Contents/Resources"
mkdir -p "$RESOURCES/bin" "$RESOURCES/claude"
cp target/release/control-plane-host target/release/control-plane-broker target/release/control-plane-computer-tools target/release/control-plane-update \
  helpers/macos/build/control-plane-desktop "$RESOURCES/bin/"
cp sidecars/claude/package.json sidecars/claude/package-lock.json "$RESOURCES/claude/"
ditto sidecars/claude/src "$RESOURCES/claude/src"
ditto sidecars/claude/dist "$RESOURCES/claude/dist"
ditto sidecars/claude/node_modules "$RESOURCES/claude/node_modules"
if [ -n "${CONTROL_PLANE_NODE_BINARY:-}" ]; then
  cp "$CONTROL_PLANE_NODE_BINARY" "$RESOURCES/bin/node"
fi
IDENTITY=${SIGNING_IDENTITY:--}
for EXECUTABLE in "$RESOURCES/bin/"*; do
  codesign --force --options runtime --sign "$IDENTITY" "$EXECUTABLE"
done
if [ -d "$APP/Contents/Frameworks" ]; then
  for LIBRARY in "$APP/Contents/Frameworks/"*.dylib; do
    if [ -f "$LIBRARY" ]; then codesign --force --options runtime --sign "$IDENTITY" "$LIBRARY"; fi
  done
fi
codesign --force --options runtime --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict "$APP"
echo "Created $APP. Node.js 22+ is required for Claude unless bundled explicitly."
echo "Notarization and distribution validation have not been performed."
