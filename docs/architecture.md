# Implementation architecture

The machine broker and OS-user worker are separate executables. The broker
terminates TLS and routes HTTP, WebSocket, and gRPC to configured user workers
over OS-authenticated Unix sockets or Windows named pipes. It never executes
agents or receives provider credentials. See [broker.md](broker.md) for setup.

Run each worker under the account whose files, credentials, and desktop it will
operate. Each owns one SQLite database with distinct team members, invitations, devices,
and explicit session grants. Additional devices can join the same member identity. The broker preserves the original device token, so a device must
authenticate separately to each account worker. Device roles govern access within
an account; OS identities and private IPC isolate accounts from each other.
Personal/local use can connect directly to a user worker without a broker.

```
SwiftUI macOS / iOS
   │ HTTPS / WSS / HTTP2 gRPC (scoped TLS trust, device certificate + token)
   ▼
Machine broker (/accounts/alice, /accounts/bob)
   │ verified OS peer identity + original device token
   ▼
Rust user worker ─── SQLite WAL (acceptance, events, device hashes, audit)
   ├── Codex App Server stdio
   ├── Kiro ACP stdio
   ├── Claude SDK Node sidecar
   ├── workspace / Git / PTY
   ├── selected WSL distribution/user → Linux bridge → agents / files / Git / PTY
   └── desktop lease → platform helper → interactive user desktop + H.264 stream

Agent MCP stdio bridge → same authenticated desktop APIs
```

## Persistence and recovery

Create and prompt acceptance persist session state, the initial event,
idempotency receipt, and audit entry in one transaction before launching a
provider. Repeated request IDs return the previous result, and changed input is
rejected. Events have database-global increasing cursors; replay filters by
session. HTTP, WebSocket, and gRPC use the same service state.

At startup, running sessions become `uncertain`. No accepted prompt is
automatically executed again. Users must review filesystem effects before
resuming. Provider session IDs are persisted when reported by the adapter.
Provider process crash and provider API failures become explicit error events.

Terminals have a bounded, in-memory sequence buffer. They survive client
disconnects, not worker restart. A slow client gets an explicit gap record.
Session event replay is durable; terminal output is not a session transcript.

## Trust model

The gateway permits plaintext only on loopback. TLS pairing requires a signed
CSR proving possession of the client's private key. The native apps create a
P-256 Keychain key; the worker replaces requested certificate identities and
usages with the authenticated device UUID and ClientAuth usage. Certificate
fingerprints and expirations are bound to device tokens before pairing returns.
Normal remote access requires both proofs. Legacy tokens may only enroll a
certificate before using TLS routes. Invalid supplied certificates fail the TLS
handshake; absent bootstrap certificates never grant session access.

The worker stores its private enrollment CA under its private data directory.
A TLS broker must explicitly load registered workers' public enrollment CAs via
`--client-ca`. Verified peer identity is forwarded only over OS-authenticated IPC;
caller-supplied proof headers are removed. Certificate binding, device/member
revocation, and explicit session ACLs are enforced at the worker.

Native clients validate server certificates through system trust or a host-scoped
CA explicitly verified with an administrator's SHA-256 fingerprint. Discovery is
only an address hint and never establishes trust. Certificate renewal uses the
same protected client key; connecting within seven days of expiry renews it.
After expiry, create a fresh enrollment. Tokens never appear in URL parameters.

Team administrators invite distinct members and can revoke every device belonging
to a member. Session owners grant viewer or operator access explicitly. Sharing
with current members snapshots those grants; future invitations do not inherit
access. Logical members share their worker's OS execution authority: operators
can run code and access that account's files and credentials. Use separate OS-user
workers for mutually untrusted people.

Trusted sessions permit unattended agent tools under the worker's OS identity.
Workspace path checks protect the file API from accidental traversal; they do
not sandbox an agent or shell. A trusted agent can access anything its OS user
can access. Provider sandbox behavior still applies. The UI must make that
authority visible when creating sessions.

Desktop capture uses short-lived exclusive input leases, current geometry, and
recent snapshots or video frames. H.264 streaming uses bounded newest-frame
queues, independent IDR packets, viewer limits, deadlines, and native VideoToolbox
decoding. The persistent Wayland user service owns its portal/PipeWire session;
local consent is explicit and permission loss closes that authority. See
[video.md](video.md) and [wayland.md](wayland.md).

A local permission denial is an error, not a successful operation. Physical
console users can always stop the worker. Emergency stop
requests managed process cancellation and revokes desktop leases; it cannot undo
files, commits, messages, or detached processes already created.

## Native clients, storage, and distribution

The native apps include Bonjour discovery, HTTP/2 gRPC requests and durable event
subscriptions, authenticated WebSockets, opt-in APNs registration, and SwiftTerm
terminal emulation with binary-safe replay, actual cell resizing, and bounded
input queues. Session tools create workspace-scoped Git worktrees, transfer binary
files with SHA-256 and conditional replacement, import provider transcripts, and
manage session grants. Imported transcripts are paged with explicit truncation;
loading history cannot execute client tools or take over an existing live agent.

Worker policies enforce members, sessions, concurrent runs, event storage,
idempotency receipts, transfer size/staging, and retention. Pruning removes old
inactive events and archived summaries while preserving command receipts. Expired
cursors produce an explicit history gap. Interrupted uploads are reconciled from
private destination-filesystem staging before quota reservations are released.
Workspace files are never deleted by retention.

Desktop helpers enumerate displays and preserve selected-display geometry through
screenshots, input, and H.264 streaming. Windows UI Automation and Linux AT-SPI
provide bounded foreground accessibility trees with password redaction and local
permission checks. Wayland selection is restricted to monitors granted by local
portal consent. The X11 fallback explicitly rejects negative-origin capture
layouts instead of capturing a different region.

The updater verifies an exact-byte Ed25519 publisher signature, platform/version,
expiry, monotonic release sequence, artifact origin, size, and hash. It drains
local workers and persists maintenance before opening the OS installer. Restarting
old workers requires explicit recovery; a new installed version consumes the
maintenance marker. Development builds have no publisher key/feed and reject
update installation. Signed packaging workflows prepare Mac PKG, Windows MSI,
Linux DEB, and iOS exports, without publishing. See [releases.md](releases.md).

Code and automated fixtures are implemented; release acceptance remains distinct.
Physical iPhone behavior, APNs delivery, real Windows/WSL and GNOME compositor
operation, live provider inference/quota eligibility, distribution signing,
notarization, and measured load/latency require the corresponding hardware,
credentials, and completed test runs. See [verification.md](verification.md).

Trusted sessions can automatically receive the computer-use MCP server when
`CONTROL_PLANE_URL` names the worker gateway and the computer-tools binary is
available beside the host executable (or via `CONTROL_PLANE_COMPUTER_TOOLS`). The
worker enrolls a dedicated automation device token for each run and revokes it
when the run ends. Tokens from a crashed worker are revoked on restart before
listening. TLS runs receive an ephemeral certificate/key bound to their automation device;
the token is revoked when the run ends. For a private server CA, configure
`CONTROL_PLANE_SERVER_CA_PEM` on the worker; the MCP client validates hostnames,
uses the bound certificate, and refuses redirects. Server CA trust is separate
from the worker enrollment CA. WSL
sessions use a Linux computer-tools executable configured in the selected distro;
they never try to execute a native Windows helper inside Linux. See
[wsl.md](wsl.md) for explicit distribution setup, roots, and lease cancellation.
