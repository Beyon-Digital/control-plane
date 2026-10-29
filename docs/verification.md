# Verification record

Development host: Apple Silicon macOS, Xcode 27.1, Swift 6.4, Rust 1.99.1.

Latest implementation run on October 29, 2026: **77 Rust tests passed** (one
installed-Codex test is opt-in), **51 Swift tests passed with no skips**, **26
Linux desktop contract tests**, **4 WSL transfer tests**, **5 release-tool tests**,
**3 Windows video framing tests**, **2 service configuration tests**, and **3 push
gateway tests passed**. Strict Clippy and rustfmt checks passed. The current
desktop contracts ran on macOS with mocked platform interfaces.

The refreshed ad-hoc signed Mac bundle is `dist/ControlPlane-v1-preview.app`. It
includes the worker, broker, desktop helper, computer-tools bridge, updater, and
Claude sidecar/dependencies. Node.js 22+ is required unless explicitly bundled.
Mac Release and iOS Simulator builds succeeded. Bundle signature verification
uses `codesign --verify --deep --strict`. This is a development preview; no
Developer ID notarization or physical-device iOS distribution is claimed.

Additional device acceptance on October 29, 2026:

- A dedicated iPhone 16 Plus simulator ran **47 iOS logic tests and two UI tests**
  with no failures or skips. The logic suite included native gRPC event replay
  and binary transfer against a disposable Rust host on loopback.
- The paired physical iPhone 16 Plus ran **46 logic tests successfully**
  with the one Mac-host-dependent gRPC test explicitly skipped. Both UI tests
  passed on the phone: connection cancellation and manual model-profile creation,
  persistence, and relaunch. The test app uses a separate acceptance bundle ID
  and normal Apple Development signing. No provider request or real ChatGPT
  login was made. The successful physical UI rerun is recorded separately from
  an earlier failed test assertion about the prefilled model name.
- Five acceptance-evidence tests passed for the new CI gates, including rejection
  of non-IDR/truncated video, exposed password values, wrong-OS runners, and WSL
  distribution-prefix/version ambiguity. Workflow YAML and generated project
  syntax checks passed. No GitHub Actions result is claimed until the code is
  pushed to a configured repository and jobs execute.

Local Xcode results are under `build/ios-simulator-20261029/result.xcresult`,
`build/iphone-20261029/result.xcresult` (logic pass and earlier UI assertion
failure), and `build/iphone-ui-20261029/result.xcresult` (successful UI rerun).
The iOS runner disables verbose test diagnostics after Xcode's simulator
diagnostic collection stalled following successful tests; XCTest results and
failures are still recorded normally. See [ci.md](ci.md) for the repeatable commands.

Earlier platform evidence from isolated containers, before the latest feature
additions:

- Linux full Rust suite passed (45 tests at that checkpoint), followed by the
  final IPC, desktop, broker, video transport, and WSL regression subsets with
  networking disabled.
- The machine broker ran against **two distinct unprivileged Linux OS UIDs**.
  Owner-only IPC, separate token stores, pairing, and cross-account denial passed.
- Linux WSL bridge tests passed for descendant cancellation, expired leases,
  PTY resize, and EOF cleanup (3 tests). Mock-launcher tests also exercised a full
  provider protocol turn and cancellation without invoking a paid provider.
- The Wayland contract suite passed all 20 tests in a read-only Linux container.
- The Windows named-pipe/registry security module and its four Windows-only
  tests cross-compiled cleanly for `x86_64-pc-windows-gnu`. They were not executed
  on Windows.

New Linux container attempts did not start, supplying no current Linux runtime
verification. The updated Windows IPC module, including private-directory DACL
validation and its four Windows-only tests, passed an isolated harness using the
same IPC source with `cargo check --target x86_64-pc-windows-gnu --tests`. This checks compilation,
not execution or the full Windows host build. Earlier checkpoint results above
must not be read as current platform acceptance.

Verified locally:

- Rust unit/integration tests, including real gRPC and WebSocket listeners.
- Real-process mTLS smoke test using a temporary test CA: signed CSR enrollment,
  proof of private-key possession, certificate/token binding, and rejection of
  missing or mismatched device proofs.
- Single-use enrollment, authenticated routes, device revocation, idempotent
  session creation, ordered event replay, and restart uncertainty.
- Optimistic file saves and workspace traversal/symlink rejection.
- Team invitations, distinct member identities, additional-device enrollment,
  session ACLs, role caps, revocation, and last-administrator protection.
- Atomic binary transfer, conditional replacement, hashes, bounded reservations,
  crash reconciliation, quota enforcement, retention, and explicit replay gaps.
- Provider transcript normalization, bounded paging, import provenance, and
  refusal to execute client tools while reading history. Live provider catalogs
  and histories still require acceptance with the installed providers.
- Real PTY input, reconnect, and background-child termination tests.
- Codex CLI 0.165.0: isolated initialization and ephemeral thread creation in
  trusted/untrusted modes. No inference call was made.
- Claude SDK 0.2.141: locked npm installation, syntax checks, query import and
  SDK option type inspection. API authentication and paid turns are untested.
- macOS helper compilation and read-only permission status. Screen capture
  permission exists; Accessibility input permission is absent. No desktop input
  was injected during development verification.
- Native Swift package tests and Xcode macOS/iOS Simulator builds.
- Native HTTP/2 gRPC requests and event replay against a real disposable Rust
  host, including event cursors beyond Double's exact integer range. The same
  fixture verifies native binary upload/download and conditional replacement.
- Client CSR encoding, certificate matching, scoped trust, SwiftTerm behavior,
  native provider-model forms, update parsing, and notification routing through
  automated tests. iOS UI automation now covers connection navigation and model
  persistence; the remaining new screens still need a full manual UI pass.
- Real VideoToolbox H.264 encode/decode using synthetic pixels, with authenticated
  video transport through both direct workers and the account broker. Invalid
  tokens/viewers, viewer limits, revocation, emergency stop, and helper process
  cleanup passed. No actual screen image was read by the automated video tests.
- Account base paths preserved for event, terminal, and video WebSockets; WSL
  target identity and Linux workspace paths decoded correctly by native clients.
- Main orchestrator tool permissions, immutable host scope, bounded tool loops,
  cancellation, WSL session creation with normal approval policy, and opaque
  Responses context replay.
- ChatGPT dynamic registration using synthetic signed tokens: PKCE, state,
  issuer/audience/nonce/signature checks, account binding, granted-scope gating,
  issued-client recovery, refresh single-flight, local logout persistence, latest
  refresh-token revocation, and cancellation/logout races.
- Real loopback OAuth callback listener rejects wrong state and HTTP method;
  custom model HTTP/SSE fixtures verify discovery, both inference protocols,
  terminal errors, completion requirements, credential isolation, and explicit
  compatibility with either Chat Completions output-limit field.
- Native Mac UI smoke check in an isolated application identity: custom model
  creation with a manual ID, persistence, default selection, and selection in the
  orchestrator. No model request or real login was made in that UI check.
- Push gateway authentication, input validation, and payload privacy tests.
- Exact-byte Ed25519 release-manifest signing/verification with a temporary test
  key, bad runtime hash rejection, missing release credential gates, and complete
  Windows payload harvesting. These tests do not create trusted release media.
- A real live event WebSocket is closed during update drain; the worker exits,
  restart of the same version stays blocked, and explicit updater recovery clears
  maintenance. Updater tests cover expiry, signature, platform, origin, sequence,
  hash, and private staging validation.

Not verified locally:

- Live Kiro sessions (Kiro is not installed on the development machine).
- Real Windows named-pipe execution, WSL2/ConPTY, and desktop capture/input.
- Real GNOME portal/PipeWire/GStreamer negotiation in a compositor session.
- Physical iPhone suspension, APNs delivery, VPN changes, Safari sign-in, or
  distribution release signing. The development-signed device tests above do
  not establish those behaviors.
- Developer ID/Windows publisher signing, notarization, App Store export,
  production push provisioning, and installation of signed release artifacts.
- Real provider inference, long-running load, and the target latency envelope.
- Real ChatGPT registration, account/client eligibility, quota-backed Responses
  inference, and physical iPhone Safari/loopback sign-in.

Run `sh scripts/verify.sh` for deterministic checks. After
`cargo build --workspace`, run `python3 scripts/smoke.py` for mTLS enrollment and
`python3 scripts/drain_smoke.py` for maintenance/recovery. On macOS, run
`python3 scripts/native-grpc-smoke.py --all-tests` to execute the Swift suite with
the disposable live host fixture. Set `DEVELOPER_DIR` to the full Xcode developer
directory when the active developer directory points to Command Line Tools.
The installed-Codex smoke test is deliberately opt-in:

```sh
CONTROL_PLANE_CODEX_BIN=/absolute/path/to/codex cargo test \
  -p control-plane-host installed_codex_accepts_thread_configuration_without_inference \
  -- --ignored
```

The CI matrix includes the real two-UID Linux broker test and bridge/portal
contracts. No remote CI result is claimed here. See
[architecture.md](architecture.md) and [releases.md](releases.md) for remaining
release acceptance and signing requirements.
