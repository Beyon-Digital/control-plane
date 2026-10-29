# WSL execution

Windows desktop capture/input stays native. A session can separately choose a
configured Linux distribution for agents, terminals, files, and Git. The target
is persisted with the session and cannot silently fall back to native execution.

Microsoft documents explicit `--distribution`/`--user` selection and states that
Linux command paths must remain in WSL format:
[commands](https://learn.microsoft.com/en-us/windows/wsl/basic-commands),
[filesystems](https://learn.microsoft.com/en-us/windows/wsl/filesystems).

## Explicit setup

Install WSL2 and the desired distribution yourself. Create a Linux user, Python
3.10+, Git, and the desired provider runtimes inside that distribution. Authenticate
each provider in that Linux account. No app request installs or changes WSL.

From PowerShell, install only this repository's bridge into the chosen account:

```powershell
./scripts/wsl_install_bridge.ps1 -Distribution Ubuntu-24.04 -LinuxUser jai -Destination /home/jai/.local/lib/control-plane/wsl_bridge.py
```

Install the Claude sidecar inside Linux with its locked `npm ci --ignore-scripts`.
API credentials can be provided through a Linux-account file at
`~/.config/control-plane/environment.json` with mode `0600`, containing string
environment variables such as `ANTHROPIC_API_KEY`. These credentials are read
inside Linux, never put on Windows command lines or returned by discovery.

Set `CONTROL_PLANE_WSL_CONFIG` on the user-owned Windows worker to this JSON:

```json
{
  "Ubuntu-24.04": {
    "user": "jai",
    "roots": ["/home/jai/projects"],
    "bridge": "/home/jai/.local/lib/control-plane/wsl_bridge.py",
    "claude_sidecar": "/home/jai/control-plane/sidecars/claude/src/index.mjs",
    "computer_tools": "/home/jai/.local/bin/control-plane-computer-tools"
  }
}
```

`claude_sidecar` and `computer_tools` are optional. Without the Linux computer
tools executable, coding remains available but app-provided GUI tools are not
injected into WSL agents. To use them, build/install the Rust computer-tools binary
inside Linux and set the worker's `CONTROL_PLANE_URL` to a Windows host address
reachable from WSL. A Windows loopback address may not be reachable under WSL NAT.
The normal device certificate/token and TLS requirements still apply.

The allowlist is scoped to the current Windows worker and its WSL user. Roots are
canonicalized inside Linux. `/mnt/*` Windows-mounted workspaces, Windows paths,
parent traversal, and file-API symlinks are rejected. Use Linux filesystems for
the repository and all worktrees.

## Client requests

Old clients omit `execution_target` and get native execution. New sessions choose:

```json
{"kind":"wsl","distribution":"Ubuntu-24.04"}
```

`GET /v1/execution-targets` lists configured targets. `POST /v1/providers/discover`
with `{"execution_target":{...}}` probes providers in that exact distro/user.
Missing distributions, users, bridges, roots, or runtimes return errors.

## Process ownership

Every provider and terminal has its own Linux process session and random lease.
The Windows worker renews the lease every five seconds. Cancellation invokes a
separate, explicitly routed WSL command to kill the Linux process group. An EOF,
broken output pipe, or lease age over twenty seconds also terminates the group.
Cancellation never runs `wsl --terminate` or affects unrelated distro processes.
PID-start checks protect cancellation from killing a reused PID.

Terminals use a Linux PTY and propagate rows/columns through the same lease.
No shell command strings are built from paths, prompts, distribution names, or
Git arguments. Provider instructions still run with the selected Linux user's
authority, as with native trusted mode.

## Verification limits

Mock-launcher tests exercise argv boundaries, target isolation, file revisions,
and explicit failure without invoking WSL. Linux CI also tests bridge process
group cancellation and lease expiry. These tests do not replace a Windows+WSL2
installation test; run that matrix before distributing Windows installers.
