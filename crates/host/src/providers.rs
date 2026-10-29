//! Structured, host-local agent adapters. Provider stdout is never a terminal UI.
use crate::models::ExecutionTarget;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

pub async fn discover_target(target: &ExecutionTarget) -> Result<Vec<Value>> {
    crate::wsl::discover_target(target).await
}

pub async fn discover() -> Vec<Value> {
    let mut found = Vec::new();
    for (name, executable) in [("codex", "codex"), ("claude", "node"), ("kiro", "kiro-cli")] {
        let output = tokio::time::timeout(
            Duration::from_secs(3),
            Command::new(executable)
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let version = match output {
            Ok(Ok(o)) if o.status.success() => {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_owned())
            }
            _ => None,
        };
        let sdk_ready = name != "claude"
            || (sidecar_path().is_file()
                && sidecar_path()
                    .parent()
                    .and_then(Path::parent)
                    .is_some_and(|p| {
                        p.join("node_modules/@anthropic-ai/claude-agent-sdk/package.json")
                            .is_file()
                    }));
        let credentials_ready = name != "claude"
            || std::env::var("ANTHROPIC_API_KEY").is_ok_and(|key| !key.trim().is_empty());
        found.push(json!({"name":name,"available":version.is_some() && sdk_ready,"version":version,
            "credentials_ready":credentials_ready,
            "detail":match name { "codex"=>"App Server stdio; authenticate on this host", "kiro"=>"Requires CLI V3 ACP; authenticate on this host", _ if !sdk_ready=>"Install the Claude SDK sidecar with npm ci", _ if !credentials_ready=>"Set ANTHROPIC_API_KEY in the worker environment before starting a Claude turn", _=>"Agent SDK sidecar ready" }}));
    }
    found
}

pub(crate) fn sidecar_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("CONTROL_PLANE_CLAUDE_SIDECAR") {
        return path.into();
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        for relative in [
            "../Resources/claude/dist/index.mjs",
            "../claude/dist/index.mjs",
            "claude/dist/index.mjs",
            "../lib/control-plane/claude/dist/index.mjs",
            "../claude/src/index.mjs",
        ] {
            let path = dir.join(relative);
            if path.is_file() {
                return path;
            }
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sidecars/claude/src/index.mjs")
}
pub(crate) fn node_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("CONTROL_PLANE_NODE_BIN") {
        return path.into();
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        for relative in [
            if cfg!(windows) { "node.exe" } else { "node" },
            "../MacOS/node",
            "../Resources/bin/node",
            "../lib/control-plane/node",
        ] {
            let path = dir.join(relative);
            if path.is_file() {
                return path;
            }
        }
    }
    "node".into()
}

// Keep the adapter boundary explicit: invocation, cancellation, events, decisions,
// and optional computer tools have independent ownership/lifetimes.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    provider: &str,
    workspace: &Path,
    prompt: &str,
    resume_id: Option<&str>,
    trusted: bool,
    cancel: CancellationToken,
    tx: mpsc::Sender<Value>,
    rx: mpsc::Receiver<Value>,
    mcp_config: Option<Value>,
) -> Result<()> {
    run_target(
        &ExecutionTarget::Native,
        provider,
        workspace,
        prompt,
        resume_id,
        trusted,
        cancel,
        tx,
        rx,
        mcp_config,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_target(
    target: &ExecutionTarget,
    provider: &str,
    workspace: &Path,
    prompt: &str,
    resume_id: Option<&str>,
    trusted: bool,
    cancel: CancellationToken,
    tx: mpsc::Sender<Value>,
    mut rx: mpsc::Receiver<Value>,
    mcp_config: Option<Value>,
) -> Result<()> {
    let mcp_config = crate::wsl::map_mcp_config(target, mcp_config)?;
    match provider {
        "codex" => {
            codex(
                workspace, prompt, resume_id, trusted, cancel, tx, &mut rx, mcp_config, target,
            )
            .await
        }
        "kiro" => {
            kiro(
                workspace, prompt, resume_id, trusted, cancel, tx, &mut rx, mcp_config, target,
            )
            .await
        }
        "claude" => {
            claude(
                workspace, prompt, resume_id, trusted, cancel, tx, &mut rx, mcp_config, target,
            )
            .await
        }
        _ => bail!("unsupported provider: {provider}"),
    }
}

async fn emit(tx: &mpsc::Sender<Value>, kind: &str, payload: Value) -> Result<()> {
    tx.send(json!({"kind":kind,"payload":payload}))
        .await
        .context("session consumer disconnected")
}

struct Rpc {
    child: Child,
    stdin: ChildStdin,
    output: BufReader<ChildStdout>,
    frame: Vec<u8>,
    cancel: CancellationToken,
    tx: mpsc::Sender<Value>,
    next_id: u64,
    trusted: bool,
    provider: &'static str,
    wsl_lease: Option<crate::wsl::WslLease>,
}

impl Drop for Rpc {
    fn drop(&mut self) {
        if let Some(lease) = &self.wsl_lease {
            lease.request_cancel();
        }
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // All subprocesses inherit a dedicated process group.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        #[cfg(windows)]
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        let _ = self.child.start_kill();
    }
}

impl Rpc {
    #[cfg(test)]
    fn start(
        executable: &str,
        args: &[&str],
        cwd: &Path,
        provider: &'static str,
        trusted: bool,
        cancel: CancellationToken,
        tx: mpsc::Sender<Value>,
    ) -> Result<Self> {
        Self::start_target(
            executable,
            args,
            cwd,
            provider,
            trusted,
            cancel,
            tx,
            &ExecutionTarget::Native,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn start_target(
        executable: &str,
        args: &[&str],
        cwd: &Path,
        provider: &'static str,
        trusted: bool,
        cancel: CancellationToken,
        tx: mpsc::Sender<Value>,
        target: &ExecutionTarget,
    ) -> Result<Self> {
        let spec = match target {
            ExecutionTarget::Native => None,
            _ => Some(crate::wsl::provider_spec(
                target,
                cwd.to_str().context("invalid Linux workspace path")?,
                provider,
            )?),
        };
        let mut command = if let Some(spec) = &spec {
            let mut command = Command::new(&spec.program);
            command.args(&spec.args);
            command
        } else {
            let mut command = Command::new(executable);
            command.args(args).current_dir(cwd);
            command
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child=command.spawn().with_context(||format!("cannot start {provider}; install and authenticate its runtime on the execution host"))?;
        let stdin = child.stdin.take().context("missing provider input")?;
        let output = BufReader::new(child.stdout.take().context("missing provider output")?);
        let wsl_lease = spec.map(|spec| spec.lease);
        if let Some(lease) = &wsl_lease {
            lease.start_renewal();
        }
        Ok(Self {
            child,
            stdin,
            output,
            frame: Vec::new(),
            cancel,
            tx,
            next_id: 1,
            trusted,
            provider,
            wsl_lease,
        })
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        tokio::select! { _=self.cancel.cancelled()=>bail!("cancelled"), result=self.stdin.write_all(&bytes)=>result.context("provider input closed") }
    }
    async fn next(&mut self) -> Result<Value> {
        let line = tokio::select! { _=self.cancel.cancelled()=>bail!("cancelled"), line=read_json_line(&mut self.output,&mut self.frame)=>line? };
        serde_json::from_slice(&line).context("provider emitted invalid JSON protocol output")
    }
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        rx: &mut mpsc::Receiver<Value>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let message = self.next().await?;
            if message["id"] == id && message.get("method").is_none() {
                if let Some(error) = message.get("error") {
                    bail!("{method}: {error}");
                }
                return Ok(message["result"].clone());
            }
            self.handle(message, rx).await?;
        }
    }
    async fn handle(&mut self, message: Value, rx: &mut mpsc::Receiver<Value>) -> Result<()> {
        let Some(method) = message["method"].as_str() else {
            return Ok(());
        };
        if let Some(id) = message.get("id") {
            if (self.provider == "codex"
                && matches!(
                    method,
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                ))
                || (self.provider == "kiro" && method == "session/request_permission")
            {
                let result = if self.trusted {
                    if self.provider == "codex" {
                        json!({"decision":"accept"})
                    } else {
                        kiro_choice(&message["params"], true)?
                    }
                } else {
                    emit(&self.tx,"approval_requested",json!({"request_id":request_key(id),"method":method,"params":message["params"]})).await?;
                    loop {
                        let response = tokio::select! {_=self.cancel.cancelled()=>bail!("cancelled"),response=rx.recv()=>response.context("approval channel closed")?};
                        if request_key(&response["request_id"]) != request_key(id) {
                            continue;
                        }
                        let allow =
                            matches!(response["decision"].as_str(), Some("accept" | "allow"));
                        break if self.provider == "codex" {
                            json!({"decision":if allow {"accept"} else {"decline"}})
                        } else {
                            kiro_choice(&message["params"], allow)?
                        };
                    }
                };
                self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
                    .await?;
                emit(
                    &self.tx,
                    "approval_resolved",
                    json!({"request_id":request_key(id)}),
                )
                .await?;
            } else {
                // Unknown requests must never be accepted implicitly. Preserve them
                // in the timeline and give the provider an explicit unsupported error.
                emit(&self.tx, "unsupported_request", message.clone()).await?;
                self.send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Control Plane does not implement this client method"}})).await?;
            }
        } else {
            let params = message["params"].clone();
            if method == "item/agentMessage/delta" {
                emit(
                    &self.tx,
                    "message_delta",
                    json!({"text":params["delta"],"item_id":params["itemId"]}),
                )
                .await?;
            } else if method == "session/update"
                && params["update"]["sessionUpdate"] == "agent_message_chunk"
            {
                emit(
                    &self.tx,
                    "message_delta",
                    json!({"text":params["update"]["content"]["text"]}),
                )
                .await?;
            } else {
                emit(&self.tx, "provider_event", message).await?;
            }
        }
        Ok(())
    }
}

pub(crate) async fn read_json_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            bail!("provider exited before reporting turn completion");
        }
        let end = buffer.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(buffer.len(), |end| end + 1);
        if line.len() + count > 16 * 1024 * 1024 {
            bail!("provider event exceeds 16 MiB");
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(std::mem::take(line));
        }
    }
}

fn kiro_choice(params: &Value, allow: bool) -> Result<Value> {
    let kind = if allow { "allow_once" } else { "reject_once" };
    let option = params["options"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["kind"] == kind));
    Ok(match option {
        Some(option) => json!({"outcome":{"outcome":"selected","optionId":option["optionId"]}}),
        None if !allow => json!({"outcome":{"outcome":"cancelled"}}),
        None => {
            return Err(anyhow!(
                "provider did not offer a one-time permission option"
            ))
        }
    })
}

fn request_key(id: &Value) -> String {
    id.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn codex(
    cwd: &Path,
    prompt: &str,
    resume: Option<&str>,
    trusted: bool,
    cancel: CancellationToken,
    tx: mpsc::Sender<Value>,
    rx: &mut mpsc::Receiver<Value>,
    mcp: Option<Value>,
    target: &ExecutionTarget,
) -> Result<()> {
    let mut rpc = Rpc::start_target(
        "codex",
        &["app-server"],
        cwd,
        "codex",
        trusted,
        cancel,
        tx,
        target,
    )?;
    tokio::time::timeout(Duration::from_secs(30),rpc.call("initialize",json!({"clientInfo":{"name":"control_plane","title":"Control Plane","version":env!("CARGO_PKG_VERSION")}}),rx)).await.context("Codex initialize timed out")??;
    rpc.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}))
        .await?;
    let mut params = codex_thread_params(cwd, trusted);
    if let Some(config) = mcp {
        params["config"] = json!({"mcp_servers":{"control_plane":config}});
    }
    let method = if let Some(id) = resume {
        params["threadId"] = json!(id);
        "thread/resume"
    } else {
        "thread/start"
    };
    let thread = tokio::time::timeout(Duration::from_secs(60), rpc.call(method, params, rx))
        .await
        .context("Codex thread initialization timed out")??;
    let id = thread["thread"]["id"]
        .as_str()
        .context("Codex did not return a thread id")?
        .to_owned();
    emit(
        &rpc.tx,
        "provider_session",
        json!({"provider_session_id":id}),
    )
    .await?;
    rpc.call(
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":prompt}]}),
        rx,
    )
    .await?;
    loop {
        let event = rpc.next().await?;
        if event["method"] == "turn/completed" {
            let status = event["params"]["turn"]["status"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned();
            rpc.handle(event.clone(), rx).await?;
            if status != "completed" {
                bail!("Codex turn {status}: {}", event["params"]["turn"]["error"]);
            }
            return Ok(());
        }
        rpc.handle(event, rx).await?;
    }
}

fn codex_thread_params(cwd: &Path, trusted: bool) -> Value {
    json!({"cwd":cwd,"approvalPolicy":if trusted {"never"} else {"untrusted"},"sandbox":if trusted {"danger-full-access"} else {"workspace-write"}})
}

#[allow(clippy::too_many_arguments)]
async fn kiro(
    cwd: &Path,
    prompt: &str,
    resume: Option<&str>,
    trusted: bool,
    cancel: CancellationToken,
    tx: mpsc::Sender<Value>,
    rx: &mut mpsc::Receiver<Value>,
    mcp: Option<Value>,
    target: &ExecutionTarget,
) -> Result<()> {
    let mut rpc = Rpc::start_target(
        "kiro-cli",
        &["acp", "--agent-engine=v3"],
        cwd,
        "kiro",
        trusted,
        cancel,
        tx,
        target,
    )?;
    let init=tokio::time::timeout(Duration::from_secs(30),rpc.call("initialize",json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"control-plane","version":env!("CARGO_PKG_VERSION")}}),rx)).await.context("Kiro initialize timed out")??;
    emit(&rpc.tx, "provider_capabilities", init).await?;
    let servers = acp_mcp_servers(mcp);
    let (method, params) = if let Some(id) = resume {
        (
            "session/load",
            json!({"sessionId":id,"cwd":cwd,"mcpServers":servers}),
        )
    } else {
        ("session/new", json!({"cwd":cwd,"mcpServers":servers}))
    };
    let session = tokio::time::timeout(Duration::from_secs(60), rpc.call(method, params, rx))
        .await
        .context("Kiro session initialization timed out")??;
    let id = session["sessionId"]
        .as_str()
        .or(resume)
        .context("Kiro did not return a session id")?
        .to_owned();
    emit(
        &rpc.tx,
        "provider_session",
        json!({"provider_session_id":id}),
    )
    .await?;
    let result = rpc
        .call(
            "session/prompt",
            json!({"sessionId":id,"prompt":[{"type":"text","text":prompt}]}),
            rx,
        )
        .await?;
    emit(
        &rpc.tx,
        "provider_event",
        json!({"method":"session/prompt/result","params":result}),
    )
    .await?;
    if result["stopReason"] == "cancelled" {
        bail!("Kiro turn cancelled");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn claude(
    cwd: &Path,
    prompt: &str,
    resume: Option<&str>,
    trusted: bool,
    cancel: CancellationToken,
    tx: mpsc::Sender<Value>,
    rx: &mut mpsc::Receiver<Value>,
    mcp: Option<Value>,
    target: &ExecutionTarget,
) -> Result<()> {
    if matches!(target, ExecutionTarget::Native) && std::env::var_os("ANTHROPIC_API_KEY").is_none()
    {
        bail!("Claude requires ANTHROPIC_API_KEY in the host worker environment");
    }
    let script = sidecar_path();
    let mut rpc = Rpc::start_target(
        "node",
        &[script.to_str().context("invalid Claude sidecar path")?],
        cwd,
        "claude",
        trusted,
        cancel,
        tx,
        target,
    )?;
    rpc.send(json!({"prompt":prompt,"cwd":cwd,"resume":resume,"trusted":trusted,"mcpServers":mcp.map(|value|json!({"control_plane":value})).unwrap_or(json!({}))})).await?;
    let mut pending: HashMap<String, Value> = HashMap::new();
    loop {
        let value = tokio::select! {
            value=rpc.next()=>value?,
            answer=rx.recv()=> {
                let answer=answer.context("approval channel closed")?;
                let key=answer["request_id"].to_string();
                if pending.remove(&key).is_some() {rpc.send(json!({"request_id":answer["request_id"],"decision":answer["decision"]})).await?;}
                continue;
            }
        };
        let kind = value["kind"].as_str().unwrap_or("provider_event");
        if kind == "approval_requested" {
            pending.insert(value["payload"]["request_id"].to_string(), value.clone());
        }
        if kind == "error" {
            bail!("Claude sidecar: {}", value["payload"]);
        }
        if kind == "completed" {
            return Ok(());
        }
        emit(&rpc.tx, kind, value["payload"].clone()).await?;
    }
}

fn acp_mcp_servers(config: Option<Value>) -> Value {
    match config {
        Some(config) => {
            let env: Vec<Value> = config["env"]
                .as_object()
                .map(|env| {
                    env.iter()
                        .map(|(name, value)| json!({"name":name,"value":value}))
                        .collect()
                })
                .unwrap_or_default();
            json!([{"name":"control_plane","command":config["command"],"args":config.get("args").cloned().unwrap_or(json!([])),"env":env}])
        }
        None => json!([]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kiro_never_synthesizes_permission_options() {
        let p = json!({"options":[{"optionId":"yes","kind":"allow_once"},{"optionId":"no","kind":"reject_once"}]});
        assert_eq!(kiro_choice(&p, true).unwrap()["outcome"]["optionId"], "yes");
        assert_eq!(kiro_choice(&p, false).unwrap()["outcome"]["optionId"], "no");
        assert!(kiro_choice(&json!({"options":[]}), true).is_err());
    }
    #[tokio::test]
    async fn rpc_preserves_events_and_maps_operator_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let (tx, mut events) = mpsc::channel(16);
        let (decision_tx, mut decision_rx) = mpsc::channel(4);
        decision_tx
            .send(json!({"request_id":"42","decision":"deny"}))
            .await
            .unwrap();
        let script = r#"import sys,json
r=json.loads(sys.stdin.readline())
print(json.dumps({'method':'item/agentMessage/delta','params':{'delta':'hello','itemId':'m1'}}),flush=True)
print(json.dumps({'id':42,'method':'item/commandExecution/requestApproval','params':{'command':'test'}}),flush=True)
a=json.loads(sys.stdin.readline())
assert a['id']==42 and a['result']['decision']=='decline'
print(json.dumps({'id':r['id'],'result':{'ok':True}}),flush=True)
"#;
        let mut rpc = Rpc::start(
            "python3",
            &["-u", "-c", script],
            temp.path(),
            "codex",
            false,
            CancellationToken::new(),
            tx,
        )
        .unwrap();
        let result = rpc
            .call("fixture/run", json!({}), &mut decision_rx)
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(events.recv().await.unwrap()["kind"], "message_delta");
        let approval = events.recv().await.unwrap();
        assert_eq!(approval["payload"]["request_id"], "42");
        assert_eq!(events.recv().await.unwrap()["kind"], "approval_resolved");
    }
    #[tokio::test]
    async fn bounded_framing_handles_split_lines_and_eof() {
        let mut reader = BufReader::with_capacity(2, &b"{\"ok\":true}\n"[..]);
        let mut frame = Vec::new();
        let bytes = read_json_line(&mut reader, &mut frame).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["ok"], true);
        assert!(read_json_line(&mut reader, &mut frame).await.is_err());
    }
    #[tokio::test]
    async fn cancellation_stops_blocked_provider() {
        let temp = tempfile::tempdir().unwrap();
        let (tx, _) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let mut rpc = Rpc::start(
            "python3",
            &["-u", "-c", "import time;time.sleep(60)"],
            temp.path(),
            "codex",
            false,
            cancel.clone(),
            tx,
        )
        .unwrap();
        cancel.cancel();
        assert!(rpc
            .next()
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }
    #[test]
    fn acp_maps_mcp_environment_without_exposing_it_as_events() {
        let servers = acp_mcp_servers(Some(
            json!({"command":"tool","args":[],"env":{"TOKEN":"secret"}}),
        ));
        assert_eq!(servers[0]["name"], "control_plane");
        assert_eq!(servers[0]["env"][0]["name"], "TOKEN");
    }
    /// Exercises the installed provider schema without inference, user sessions,
    /// credentials, or persistent provider configuration. Explicit opt-in only.
    #[tokio::test]
    #[ignore = "requires installed Codex; no inference is performed"]
    async fn installed_codex_accepts_thread_configuration_without_inference() {
        let temp = tempfile::tempdir().unwrap();
        let (tx, _events) = mpsc::channel(32);
        let (_sender, mut decisions) = mpsc::channel(4);
        let executable =
            std::env::var("CONTROL_PLANE_CODEX_BIN").unwrap_or_else(|_| "codex".into());
        let mut command = Command::new(executable);
        command
            .arg("app-server")
            .current_dir(temp.path())
            .env("CODEX_HOME", temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut rpc = Rpc {
            child,
            stdin,
            output,
            frame: Vec::new(),
            cancel: CancellationToken::new(),
            tx,
            next_id: 1,
            trusted: false,
            provider: "codex",
            wsl_lease: None,
        };
        let initialize=rpc.call("initialize",json!({"clientInfo":{"name":"control_plane_test","title":"Control Plane Test","version":"0.1.0"}}),&mut decisions);
        tokio::time::timeout(Duration::from_secs(30), initialize)
            .await
            .unwrap()
            .unwrap();
        rpc.send(json!({"method":"initialized","params":{}}))
            .await
            .unwrap();
        for trusted in [false, true] {
            let mut params = codex_thread_params(temp.path(), trusted);
            params["ephemeral"] = json!(true);
            let response = tokio::time::timeout(
                Duration::from_secs(30),
                rpc.call("thread/start", params, &mut decisions),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(response["thread"]["id"].as_str().is_some());
        }
    }
}
