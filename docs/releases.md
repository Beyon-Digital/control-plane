# Release preparation

`scripts/release/` prepares reviewable artifacts and signed update manifests. It
does not publish or upload releases. Actual release signing has not been run in
this workspace: Apple distribution identities, profiles, notary credentials,
Windows signing credentials, APNs credentials, and release publisher keys were
not supplied.

## Publisher and runtime configuration

Create and protect the production Ed25519 publisher key outside this repository
using your release key-management process. Set `CONTROL_PLANE_UPDATE_PUBLIC_KEY`
to its base64-encoded 32-byte public key **when compiling**. Set
`CONTROL_PLANE_UPDATE_FEED` to the platform's HTTPS feed. The app's updater never
accepts a user-entered replacement key. `CONTROL_PLANE_UPDATE_SIGNING_KEY` is the
existing private-key path used only by the release manifest signer; an encrypted
key may use `CONTROL_PLANE_UPDATE_KEY_PASSWORD` from your secret store.

Set `CONTROL_PLANE_ARTIFACT_BASE_URL` to the same HTTPS origin as the feed and
`BUILD_NUMBER` to a monotonically increasing release sequence. Bump the Cargo
workspace version before releasing a newer version. Manifests authenticate exact
UTF-8 bytes, expire within 31 days, and include platform, version, sequence, final
artifact size, and SHA-256. They are generated after package signing/stapling.
The signer checks that the private key matches the compiled public-key pin.

Provide `NODE_RUNTIME_PATH` and the approved `NODE_SHA256`. The builder checks the
hash before executing Node, requires Node 22+, installs the locked Claude SDK
without install scripts, builds both runtime and history helpers, and includes
production dependencies. Node is bundled with the signed/hashed artifact.
Python 3.11+, Rust, current Xcode for Apple builds, and OpenSSL with Ed25519 support
are build prerequisites. Never place private signing credentials in `dist/`.

## Platform commands

- macOS: `sh scripts/release/macos.sh`. Requires Developer ID Application and
  Installer identities (`MACOS_SIGNING_IDENTITY`, `MACOS_INSTALLER_IDENTITY`), a
  matching production push-capable `MACOS_PROVISIONING_PROFILE`, and an existing
  `APPLE_NOTARY_PROFILE`. Builds/tests, signs nested native code and Node JIT
  entitlements, embeds provisioning, notarizes/staples the app and PKG, checks
  Gatekeeper, then signs the final update manifest. No credentials are generated.
- iOS: `sh scripts/release/ios.sh`. Requires `IOS_TEAM_ID`,
  `IOS_SIGNING_IDENTITY`, `IOS_PROFILE_SPECIFIER`, registered `IOS_BUNDLE_ID`, and
  reviewed `IOS_EXPORT_OPTIONS_PLIST` for manual App Store Connect export. Produces
  an archive and signed export. Upload and App Store/TestFlight review are separate.
- Windows: run `scripts/release/windows.ps1` with an existing trusted code-signing
  certificate (`WINDOWS_CERT_THUMBPRINT`), `CONTROL_PLANE_PUBLISHER`, WiX 4,
  SignTool, Python/PyInstaller, and an approved FFmpeg binary/hash/license
  (`FFMPEG_PATH`, `FFMPEG_SHA256`, `FFMPEG_LICENSE_FILE`). The complete payload,
  including nested SDK code, is signed and harvested into the MSI.
- Linux: `sh scripts/release/linux.sh` on Ubuntu 24.04 x86_64 with
  `PACKAGE_MAINTAINER`. Builds a DEB with user service definitions, desktop
  dependencies, and verified Node/Claude runtime. Services are not enabled
  automatically; configure each OS user's roots, provider auth, and broker first.

The manual `Prepare signed release` workflow uses a protected `release`
environment and preconfigured self-hosted signing runners. Configure its variables
and signer/keychain access in your release infrastructure. Artifacts remain for
review; the workflow does not publish them. A workflow file is not evidence of a
completed signing or notarization run.

## Installation and recovery

The updater checks signatures and policy before downloading, rechecks artifacts
before installation, writes owner-only maintenance records, and waits for explicit
local workers to release their locks. Cancellation does not replay accepted work.
Pass every affected local worker using repeated `--worker-data` arguments. The Mac
UI manages its own `~/Library/Application Support/ControlPlane/Host` worker.
Shared-machine operators must drain every affected user's worker and stop the
broker before replacing shared binaries. PKG/DEB manual upgrades reject active
installed worker/broker binaries; Windows uses the normal MSI installation flow.

The same installed version refuses to restart while maintenance is pending. A new
version consumes the marker. If installation is cancelled, use the Mac update
screen's recovery action or `control-plane-update resume --worker-data <dir>`
(`control-plane-host --data <dir> resume` also works). This only clears maintenance;
it never executes an old request again.

Distribution is still gated on real signed-package installation/rollback tests,
notarization and provisioning results, APNs delivery, provider authentication and
quota eligibility, and the physical-device/platform matrix in verification.md.
