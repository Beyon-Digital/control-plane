# Main orchestrator and models

The native Mac and iOS clients run the main orchestrator on the viewing device.
Open **Models** to add an OpenAI-compatible provider, or choose **Continue with
ChatGPT**. Select a model profile, then open **Orchestrator** to chat and coordinate
agents on the currently connected machine/account. Plain chat also works without
a connected machine.

Custom profiles include a name, API base URL, model ID, and either the Chat
Completions or Responses protocol. Discover models from the endpoint or enter a
model ID manually. Discovery is a catalog request, not an inference test. Remote
endpoints require HTTPS; HTTP is limited to loopback. Credentials in URLs and
redirects are rejected. Changing the endpoint's origin clears the previous API
key unless a replacement is explicitly supplied.

Requests allow at most 8,192 output tokens. Chat Completions profiles can select
the output-limit field supported by their server: max_completion_tokens or
max_tokens. The app does not retry inference with a different field or provider.

Profile metadata is stored in preferences. API keys and ChatGPT credentials are
stored in this device's Keychain, separately from worker enrollment credentials.
They are never sent to a worker, machine broker, coding agent, or another model
provider. Orchestrator prompts, selected file contents, diffs, and tool results
are sent to the selected inference provider. Choose a provider appropriate for
the repositories you connect.

## ChatGPT plan sign-in

This implements OpenAI's documented public-client dynamic registration flow:
initial authorization uses `dynamic_agent_client`, and subsequent authorization
and refresh use the issued client ID. Every attempt has fresh PKCE S256, state,
and OIDC nonce values. The host identifier remains stable on this device. The
loopback listener starts before opening the authorization browser; callbacks use
`127.0.0.1` and the exact registered path. ID-token signatures and issuer,
audience, nonce, expiry, and returning-account identity are validated before
credentials become active.

Each account registration has its own client/identity binding and rotating token
set. ChatGPT inference uses the official Responses API endpoint and requires the
granted `chatgpt.tokens.use.direct` scope. Tokens cannot be assigned to custom
endpoints. Refreshes are serialized per renewable session. Signing out attempts
remote revocation and clears local tokens; an unconfirmed revocation is reported
so the user can disconnect access in ChatGPT Settings.

ChatGPT plan access depends on OpenAI's current client and account eligibility.
A completed login or model listing alone does not prove inference entitlement.
The app requires a completed Responses stream and reports permission, usage-limit,
incomplete-response, and interrupted-stream errors even when text has already
arrived. It never silently switches accounts or falls back to API billing.
Usage belongs to the selected ChatGPT account; it is not pooled across devices,
OS accounts, or other users. Deployment of this repository does not itself
establish OpenAI preview eligibility.

On macOS authorization opens the system browser. On iOS it uses a system Safari
view while the app remains active for the required HTTP loopback callback.
Physical-device sign-in, suspension, and network transitions require acceptance
testing; no alternate redirect or token-copy workaround is used.

Token refresh coordination currently belongs to the app process. Use one running
app instance per device; windows in the same process share the coordinator.

## Agent coordination

The orchestrator can inspect host capabilities, execution targets, installed
providers, accessible sessions, events, workspace files, and Git diffs. Turn on
**Allow agent management** to expose session creation, prompts, and interruption.
The permission is enforced in both the tool catalog and dispatcher. New sessions
use normal agent approval policies. Provider permission decisions and desktop
input remain in their dedicated interfaces.

Each turn captures its model and host/account scope. Switching or editing that
scope cancels the turn and clears conversational context. Tool calls use the
existing authenticated worker API, including WSL routing and broker account base
paths. The worker still enforces role, ownership, workspace, and device checks.
The orchestrator has no arbitrary HTTP, shell, credential, or permission-approval
tool. Source files and agent output are treated as untrusted data.

The tool loop allows at most 10 model rounds, 40 calls per user turn, and eight
calls per response. Arguments, tool results, and conversation context are bounded.
Tool calls run sequentially and never execute until the model response has
completed successfully. Duplicate call IDs are rejected. Cancellation ends the
orchestrator turn; already accepted agent tasks continue on their host. Review
sessions before retrying a failed or canceled turn because completed side effects
cannot be undone. Chat history currently lives in memory on the viewing device.

Protocol references:
[registration](https://developers.openai.com/siwc/token-sharing-open-source/sign-in),
[accounts and renewal](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions),
[models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference),
and [preview eligibility](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations).

Automated verification uses synthetic OAuth keys/tokens and model HTTP/SSE
fixtures. It does not sign into a real account or consume inference quota.
