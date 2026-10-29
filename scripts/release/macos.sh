#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo"
: "${MACOS_SIGNING_IDENTITY:?Developer ID Application identity required}"
: "${MACOS_INSTALLER_IDENTITY:?Developer ID Installer identity required}"
: "${MACOS_PROVISIONING_PROFILE:?Developer ID provisioning profile with push capability required}"
: "${APPLE_NOTARY_PROFILE:?Existing notarytool Keychain profile required}"
: "${CONTROL_PLANE_UPDATE_PUBLIC_KEY:?Pinned Ed25519 public key required}"
: "${CONTROL_PLANE_UPDATE_FEED:?HTTPS feed URL required}"
: "${CONTROL_PLANE_UPDATE_SIGNING_KEY:?Existing Ed25519 private key path required}"
: "${CONTROL_PLANE_ARTIFACT_BASE_URL:?HTTPS artifact base URL required}"
: "${BUILD_NUMBER:?Monotonic numeric release sequence required}"
: "${NODE_RUNTIME_PATH:?Approved Node runtime path required}"
: "${NODE_SHA256:?Approved Node SHA-256 required}"
version=$(python3 -c 'import tomllib;print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')
case "$BUILD_NUMBER" in ''|*[!0-9]*) exit 1;; esac
security find-identity -v -p codesigning | /usr/bin/grep -F -- "$MACOS_SIGNING_IDENTITY" >/dev/null
cargo test --workspace --locked
swift test --package-path apple
cargo build --release --locked --target aarch64-apple-darwin
SIGNING_IDENTITY="$MACOS_SIGNING_IDENTITY" sh helpers/macos/build.sh
python3 apple/generate_project.py
xcodebuild -skipPackagePluginValidation -quiet -project apple/ControlPlane.xcodeproj -scheme ControlPlane -configuration Release -destination 'generic/platform=macOS' -derivedDataPath build/release-macos CODE_SIGNING_ALLOWED=NO MARKETING_VERSION="$version" CURRENT_PROJECT_VERSION="$BUILD_NUMBER" build
app="$repo/build/release-macos/Build/Products/Release/ControlPlane.app"
tools="$app/Contents/Resources/bin"
mkdir -p "$tools"
for name in control-plane-host control-plane-broker control-plane-computer-tools control-plane-update;do
 cp "target/aarch64-apple-darwin/release/$name" "$tools/$name"
 codesign --force --options runtime --timestamp --sign "$MACOS_SIGNING_IDENTITY" "$tools/$name"
done
cp helpers/macos/build/control-plane-desktop "$tools/control-plane-desktop"
python3 scripts/release/stage_runtime.py --platform macos --destination "$repo/build/release-macos/runtime"
cp "$repo/build/release-macos/runtime/node" "$tools/node"
cp -R "$repo/build/release-macos/runtime/claude" "$app/Contents/Resources/claude"
python3 scripts/release/sign_macos_payload.py "$app/Contents/Resources/claude" "$MACOS_SIGNING_IDENTITY"
codesign --force --options runtime --timestamp --entitlements scripts/release/node.entitlements --sign "$MACOS_SIGNING_IDENTITY" "$tools/node"
cp "$MACOS_PROVISIONING_PROFILE" "$app/Contents/embedded.provisionprofile"
security cms -D -i "$MACOS_PROVISIONING_PROFILE" > build/release-macos/profile.plist
python3 - <<'PY'
import plistlib,datetime
from pathlib import Path
profile=plistlib.loads(Path('build/release-macos/profile.plist').read_bytes())
if profile['ExpirationDate']<=datetime.datetime.now():raise SystemExit('Provisioning profile expired')
ent=profile['Entitlements']
if ent.get('com.apple.developer.aps-environment')!='production':raise SystemExit('Production push provisioning required')
bundle=ent.get('com.apple.application-identifier','')
if not bundle.endswith('.dev.controlplane.app'):raise SystemExit('Provisioning profile does not match dev.controlplane.app')
result={'com.apple.security.network.client':True,'com.apple.developer.aps-environment':'production','com.apple.application-identifier':bundle,'com.apple.developer.team-identifier':ent['com.apple.developer.team-identifier']}
Path('build/release-macos/release.entitlements').write_bytes(plistlib.dumps(result))
PY
if [ -d "$app/Contents/Frameworks" ]; then
 python3 scripts/release/sign_macos_payload.py "$app/Contents/Frameworks" "$MACOS_SIGNING_IDENTITY"
fi
codesign --force --options runtime --timestamp --entitlements build/release-macos/release.entitlements --sign "$MACOS_SIGNING_IDENTITY" "$app"
codesign --verify --deep --strict --verbose=2 "$app"
mkdir -p dist
archive="$repo/build/release-macos/ControlPlane-notary.zip"
ditto -c -k --keepParent "$app" "$archive"
xcrun notarytool submit "$archive" --keychain-profile "$APPLE_NOTARY_PROFILE" --wait --timeout 1h
xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose=2 "$app"
package="$repo/dist/ControlPlane-$version-macos-arm64.pkg"
test ! -e "$package"
mkdir -p build/release-macos/installer-scripts
cat > build/release-macos/installer-scripts/preinstall <<'SCRIPT'
#!/bin/sh
set -eu
# Refuse manual replacement while an installed broker/worker still executes.
# The app updater drains its own worker; shared installations must drain every user.
if /bin/ps -axo comm= | /usr/bin/grep -E '^/Applications/ControlPlane\.app/Contents/Resources/bin/control-plane-(host|broker)$' >/dev/null; then
 echo 'Stop the installed Control Plane workers and broker before replacing this installation.' >&2
 exit 1
fi
SCRIPT
chmod 755 build/release-macos/installer-scripts/preinstall
pkgbuild --scripts build/release-macos/installer-scripts --component "$app" --install-location /Applications --identifier dev.controlplane.app --version "$version" --sign "$MACOS_INSTALLER_IDENTITY" "$package"
xcrun notarytool submit "$package" --keychain-profile "$APPLE_NOTARY_PROFILE" --wait --timeout 1h
xcrun stapler staple "$package"
xcrun stapler validate "$package"
spctl --assess --type install --verbose=2 "$package"
python3 scripts/release_manifest.py --artifact "$package" --url "${CONTROL_PLANE_ARTIFACT_BASE_URL%/}/$(basename "$package")" --feed-url "$CONTROL_PLANE_UPDATE_FEED" --version "$version" --platform macos-arm64 --sequence "$BUILD_NUMBER" --signing-key "$CONTROL_PLANE_UPDATE_SIGNING_KEY" --output "$repo/dist/stable-macos-arm64-$BUILD_NUMBER.json"
echo 'Signed/notarized package and publisher manifest prepared. Nothing published.'
