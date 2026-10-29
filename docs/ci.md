# CI and device acceptance

`Verify` runs on pushes, pull requests, and manual dispatch. Hosted macOS,
Ubuntu, and Windows jobs run Rust checks and platform-independent contracts.
The Apple job starts a disposable Rust host, runs the Swift suite against native
gRPC and binary transfers, checks mTLS and update drain/recovery, then creates its
own iPhone 16 Plus simulator for iOS logic and UI tests. It never changes or
shuts down a developer's existing simulators. Simulator result bundles are kept
for 14 days. No hosted job requests paid inference.

See [GitHub Actions](https://github.com/Beyon-Digital/control-plane/actions) for
the current results. Local workflow files and passing local checks do not by
themselves establish a successful hosted run.

## iPhone tests

The generated project has a separate `ControlPlane-iOSTests` scheme containing
`ControlPlaneTests` and `ControlPlaneUITests`. The same client logic tests run
inside the actual iOS app; the UI tests exercise connection cancellation,
manual model creation/persistence, and connected session navigation against a
disposable loopback host. They use the isolated bundle identity
`dev.controlplane.acceptance`, preserving a normal installation's preferences.
They do not sign into ChatGPT, call inference, enable push, or operate a desktop.

```sh
cargo build --workspace --locked
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer python3 scripts/test-ios.py \
  --preview-host --result-path build/ios-acceptance/result.xcresult
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer python3 scripts/test-ios.py \
  --device "$IOS_TEST_DEVICE_UDID" --team "$IOS_TEAM_ID" \
  --result-path build/iphone-acceptance/result.xcresult
```

Use a fresh result path for every run. `--simulator <UDID>` selects an explicit
already-booted simulator; omitting it creates, boots, and removes only the
script's own simulator. Physical testing requires the exact paired phone to be
available, unlocked, trusted by Xcode, and in Developer Mode. Normal Apple
development signing and provisioning are used. It never falls back to a
different phone. The host-dependent gRPC test is skipped on a physical phone;
the simulator uses a loopback fixture and runs it. Phone-to-host connectivity,
APNs, background suspension, and Safari sign-in remain separate acceptance work.

Simulator apps use normal ad-hoc signing so Keychain behaves as it does in an
installed app. No Apple account or signing secrets are needed for that path.
`--preview-host` adds a deterministic local adapter and labeled sample transcript
for visual review; it never contacts a model provider. Physical UI-only runs skip
connected-loopback navigation. An optional `--host-ip` private Mac address enables
the physical gRPC test using a temporary CA and per-device mTLS enrollment; iOS
Local Network permission must be granted on the phone separately.

The `Physical iPhone acceptance` workflow requires a trusted self-hosted Mac
with labels `self-hosted`, `macOS`, `control-plane-iphone`. Configure the protected
`device-acceptance` environment with `IOS_TEAM_ID` and `IOS_TEST_DEVICE_UDID`
variables. Keep signing keys in that runner's Keychain. It runs only from the
default branch on manual dispatch and retains results for seven days.

## Desktop and WSL runners

`Desktop and WSL acceptance` is manually dispatched from the default branch and
uses the protected `platform-acceptance` environment. Register dedicated runners
with these labels:

| Platform | Labels and prerequisites |
| --- | --- |
| macOS | `self-hosted`, `macOS`, `control-plane-desktop`; unlocked graphical account; stable helper signing identity and Screen Recording/Accessibility grants; FFmpeg |
| Windows | `self-hosted`, `Windows`, `control-plane-desktop`; unlocked interactive user session outside Session 0; PowerShell, Python, approved FFmpeg with gdigrab/libx264; configured WSL2 distro and non-root Linux user |
| GNOME | `self-hosted`, `Linux`, `gnome-wayland`, `control-plane-desktop`; logged-in GNOME Wayland account, user bus, portals, PipeWire/GStreamer, AT-SPI enabled, FFmpeg, and a locally authorized portal grant |

Use disposable test accounts and desktops. Keep a suitable accessible foreground
window on the primary selected display. The workflow does not inject mouse/key
events or approve OS permission dialogs. Missing capabilities, permissions,
displays, or runtime dependencies fail the job. An absent runner leaves the job
pending. Hosted unit tests do not substitute for these graphical checks.

For Mac helpers, set `MACOS_DESKTOP_SIGNING_IDENTITY` in the environment to the
runner's existing identity, and grant permissions to that stable helper identity.
For GNOME, deploy the current helper scripts and authorize locally before the
run; the check rejects a stale installed daemon. The default daemon directory is
`~/.local/share/control-plane/linux`; a runner can set
`CONTROL_PLANE_WAYLAND_DAEMON_DIRECTORY` when deployed elsewhere. Normal helper
and FFmpeg environment overrides are supported.

Select one or two minimum displays. The observation script decodes real JPEG
captures, checks display selection and geometry, inspects a bounded foreground
accessibility tree, validates actual IDR NALs, and decodes two H.264 frames. It
uploads only sanitized JSON gate reports. Screen pixels, video, accessibility
text, credentials, and portal restore tokens are not uploaded. Reports explicitly
leave input correctness, lock/revocation transitions, pixel freshness, and a
known-sentinel password fixture untested.

For Windows, supply `wsl_distribution` and set `WSL_TEST_USER` in the environment.
The WSL check never installs a distro or changes its default user. It stages only
the current bridge and tests in a private temporary Linux directory, verifies
Windows host API routing and read/write into that selected WSL workspace, runs
the three Linux process/PTY ownership tests, and removes only its own temporary
resources. Full provider turns and interactive ConPTY remain distinct checks.

## Releases and live providers

The existing protected release workflow prepares signed artifacts without
publishing. See [releases.md](releases.md) for runner setup, signer identities,
runtime hashes, publisher keys, provisioning, and notarization requirements.
Neither signing identities nor live provider/ChatGPT credentials are manufactured
by CI. Live inference, ChatGPT eligibility/interactive sign-in, actual APNs
delivery, latency/load measurement, and desktop input/lock transitions still need
their configured accounts and test environments; these workflows do not mark
those gates as passed.
