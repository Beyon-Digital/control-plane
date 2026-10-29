#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo"
: "${IOS_TEAM_ID:?Existing Apple Developer team required}"
: "${IOS_SIGNING_IDENTITY:?Existing distribution identity required}"
: "${IOS_PROFILE_SPECIFIER:?Existing App Store provisioning profile required}"
: "${IOS_EXPORT_OPTIONS_PLIST:?Reviewed export options plist required}"
: "${IOS_BUNDLE_ID:?Registered bundle identifier required}"
: "${BUILD_NUMBER:?Numeric App Store build number required}"
case "$BUILD_NUMBER" in ''|*[!0-9]*) exit 1;; esac
security find-identity -v -p codesigning | /usr/bin/grep -F -- "$IOS_SIGNING_IDENTITY" >/dev/null
python3 - "$IOS_EXPORT_OPTIONS_PLIST" "$IOS_TEAM_ID" <<'PY'
import plistlib,sys
with open(sys.argv[1],'rb')as source:value=plistlib.load(source)
if value.get('method')!='app-store-connect'or value.get('teamID')!=sys.argv[2]or value.get('signingStyle')!='manual':raise SystemExit('Export must explicitly select app-store-connect, matching team, manual signing')
PY
version=$(python3 -c 'import tomllib;print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')
python3 apple/generate_project.py
swift test --package-path apple
xcodebuild -skipPackagePluginValidation -quiet -project apple/ControlPlane.xcodeproj -scheme ControlPlane -configuration Release -destination 'generic/platform=iOS' -archivePath build/release-ios/ControlPlane.xcarchive PRODUCT_BUNDLE_IDENTIFIER="$IOS_BUNDLE_ID" MARKETING_VERSION="$version" CURRENT_PROJECT_VERSION="$BUILD_NUMBER" CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM="$IOS_TEAM_ID" CODE_SIGN_IDENTITY="$IOS_SIGNING_IDENTITY" PROVISIONING_PROFILE_SPECIFIER="$IOS_PROFILE_SPECIFIER" archive
xcodebuild -skipPackagePluginValidation -quiet -exportArchive -archivePath build/release-ios/ControlPlane.xcarchive -exportPath dist/ios -exportOptionsPlist "$IOS_EXPORT_OPTIONS_PLIST"
echo 'Signed iOS export prepared. No TestFlight/App Store upload performed.'
