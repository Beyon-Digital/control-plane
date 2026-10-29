# Control Plane

Native macOS and iOS clients for coding agents running on your machines. A Rust
worker provides durable sessions, structured provider adapters, workspace tools,
PTY terminals, desktop tools, gRPC, and an HTTPS/WebSocket gateway.

The native **Orchestrator** coordinates agents on the selected host. **Models**
supports custom OpenAI-compatible endpoints and **Continue with ChatGPT** dynamic
registration for eligible ChatGPT plan inference. Model credentials stay on the
viewing device in Keychain. See [orchestrator setup](docs/orchestrator.md) for
account eligibility, supported protocols, permissions, and verification limits.

**Status: broker, WSL routing, Wayland control, and desktop video are implemented;
target-platform acceptance and broader production release gates remain.** See
[architecture](docs/architecture.md). Workers run as separate OS users; the
machine broker routes authenticated connections to those workers over private IPC.

## Build and run

Requirements: Rust stable, Node.js 22+, Git; Xcode for Apple apps and helper.
Install and authenticate the provider CLIs separately on the execution host.
Claude uses an API key with the official Agent SDK, not subscription login.

```sh
cargo build --workspace
npm --prefix sidecars/claude ci
npm --prefix sidecars/claude run build
swift build --package-path apple
sh helpers/macos/build.sh
```

Start a loopback development worker. The workspace root is an explicit absolute
directory containing repositories this worker may expose through its file API:

On macOS, you can instead launch the client and choose **Start local host…**,
then select your workspace folder. The app starts and pairs with its own worker.
The packaged Mac app includes the required binaries; development builds find
them in this repository. The local worker keeps running when the app closes.

```sh
cargo run --bin control-plane-host -- serve \
  --workspace-root /absolute/path/to/repositories
```

In another terminal, create an enrollment secret using the same data directory:

```sh
cargo run --bin control-plane-host -- enroll --role administrator
```

Launch the Mac client with `swift run --package-path apple ControlPlaneApp`, add
the host, and enroll with that secret. Secrets expire after five minutes and work
once. Subsequent devices can be enrolled using an operator secret. Each device
stores its token in Keychain.

## Connect a phone or remote Mac

Use a reachable private DNS name on your LAN/VPN. The remote gateway requires TLS
and the Apple client requires verified server-certificate trust. For testing:

```sh
sh scripts/dev-cert.sh .control-plane/tls my-machine.example.ts.net
target/debug/control-plane-host serve \
  --workspace-root /absolute/path/to/repositories \
  --listen 0.0.0.0:9847 --grpc-listen 0.0.0.0:9848 \
  --tls-cert .control-plane/tls/server.pem \
  --tls-key .control-plane/tls/server.key
```

In the connection screen, paste the server CA PEM and independently verified
SHA-256 fingerprint to establish trust scoped to this app and host. Alternatively,
install the CA through your normal managed-device trust process. Use
`https://my-machine.example.ts.net:9847` in the app. Keep the CA key on the host.
For normal deployments use your organization's trusted certificates.

The gRPC port uses the device token in `x-device-token` metadata and the same
bound client certificate as HTTPS. Enter the optional same-host gRPC URL in the
native connection screen to use HTTP/2 requests and event subscriptions; binary
transfers, desktop video, and PTYs use authenticated HTTPS/WSS. Pairing creates
the protected client key and enrolls its certificate before session access.

## Execution and control

Create a session with a provider, existing workspace path, and title. Enable
trusted execution only when you intend to grant the agent unattended access under
that OS account. This option is required for the interactive terminal.

- Provider processes and terminals continue when a client disconnects.
- Host restart marks interrupted work uncertain; it does not automatically rerun
  accepted prompts.
- Workspace saves require the last-read SHA-256 to avoid overwriting stale edits.
- Commit operates on explicitly staged changes. Push is a separate action.
- Device revocation closes authenticated streams and cancels runs initiated by
  the revoked device.
- Emergency stop requests cancellation; it cannot undo completed side effects.

Use the [multi-user broker](docs/broker.md) for one network listener backed by
separate OS-account workers. Devices enrolled in one worker share that worker's
OS authority. The native clients preserve account base paths such as
`https://machine:9847/accounts/alice` for every API and video/terminal WebSocket.

On Windows, sessions can choose an explicit configured WSL distribution for
agents, files, Git, and terminals while desktop control stays native. See
[WSL setup](docs/wsl.md). GNOME Wayland control uses the persistent
[portal/PipeWire helper](docs/wayland.md). [Desktop video](docs/video.md) uses
authenticated H.264 streams and native Apple decoding, with snapshot fallback.

## Computer-use MCP bridge

Build the platform helper and set `CONTROL_PLANE_DESKTOP_HELPER` to its absolute
executable path before starting the worker. Grant capture and input permissions
locally on the host. Desktop status explains missing permissions/backends.

Register `target/debug/control-plane-computer-tools` as a stdio MCP server in a
provider's own MCP configuration, with these environment variables:

```
CONTROL_PLANE_URL=https://your-host:9847
CONTROL_PLANE_DEVICE_TOKEN=<a separately enrolled operator token>
```

Use a dedicated device token so automation can be revoked separately. Do not
commit tokens to repository MCP configuration. The tools expose desktop status,
screenshots, and input actions through the same exclusive lease service as the
native clients. Capture a screenshot immediately before acting and supply its
geometry version. Never bypass TLS validation.

## Optional notifications

The self-hostable `sidecars/push` gateway uses APNs HTTP/2 and ES256 provider
tokens. It accepts only generic completion, approval, and failure alerts.

Set `APNS_KEY_FILE`, `APNS_KEY_ID`, `APNS_TEAM_ID`, `APNS_TOPIC`, and a random
`CONTROL_PLANE_PUSH_TOKEN` of at least 32 characters, then run:

```sh
node sidecars/push/server.mjs
```

Set the worker's `CONTROL_PLANE_PUSH_URL` and matching
`CONTROL_PLANE_PUSH_TOKEN`. Register a device's APNs token through
`POST /v1/notifications/register`. Remote gateway listeners require
`TLS_CERT_FILE`/`TLS_KEY_FILE`; loopback defaults to port 9849. APNs requires Apple
provisioning and device entitlements; local unsigned builds cannot validate real
delivery. Notification failures do not affect session event persistence.

## Verification

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
swift test --package-path apple
node --test sidecars/push/server.test.mjs
```

CI includes macOS/Linux/Windows Rust jobs, Apple package tests, and the Claude
sidecar build. Paid provider turns, host GUI permissions, physical iPhone
behavior, release signing, and remote network performance require separate
integration acceptance tests. No claim of passing those tests is implied by a
successful unit build.

Local verification results and untested environments are recorded in
[verification.md](docs/verification.md). To package an Apple Silicon Mac bundle,
run `sh scripts/package-macos.sh`. It builds the app, host, broker, MCP bridge,
updater, desktop helper, and Claude sidecar;
it uses ad-hoc signing unless `SIGNING_IDENTITY` is provided. This does not
notarize or publish a release. See [Apple builds](apple/README.md) for the iOS
Simulator and device project.

The latest built preview from this implementation pass is
`dist/ControlPlane-v1-preview.app`. Quit the earlier preview before opening the updated
bundle. The current preview is ad-hoc signed for local use; its worker and broker
are separately configurable. Use the broker, WSL, Wayland, and video guides above
for remote host setup. The preview requires Node.js 22+ for Claude unless
`CONTROL_PLANE_NODE_BINARY` is provided while packaging.

Protocol: [control_plane.proto](proto/control_plane.proto). Architecture and trust
boundaries: [architecture.md](docs/architecture.md).

## Native management features

The Mac and iOS clients now include local-network discovery, certificate-bound
pairing and scoped CA trust, optional native HTTP/2 gRPC, a full terminal emulator,
team invitations and per-session permissions, worktrees, binary file transfers,
provider transcript import, resource/retention settings, display selection, and
opt-in notifications. The Mac app includes a verified-update screen.

TLS deployments use per-device certificates plus tokens. Brokers must load the
public device CAs of their workers. See [broker setup](docs/broker.md),
[architecture](docs/architecture.md), [release preparation](docs/releases.md), and
[verification evidence and outstanding acceptance gates](docs/verification.md).
See [CI and device acceptance](docs/ci.md) for hosted checks, real iPhone tests,
and protected desktop/WSL runners.
The generated Mac preview uses ad-hoc signing. It is not a notarized distribution.
