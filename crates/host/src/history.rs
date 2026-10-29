//! Provider-supported metadata inspection; never drives an existing live process.
use crate::{
    api::ApiError,
    core::Core,
    models::{now, Device, ExecutionTarget, Session},
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{Query, State},
    routing::{get, post},
    Extension, Json, Router,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};
struct HistoryProcess {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    frame: Vec<u8>,
    lease: Option<crate::wsl::WslLease>,
}
impl Drop for HistoryProcess {
    fn drop(&mut self) {
        if let Some(lease) = &self.lease {
            lease.request_cancel();
        }
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
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
impl HistoryProcess {
    fn start(provider: &str, target: &ExecutionTarget, workspace: &str) -> Result<Self> {
        let mut spec = match target {
            ExecutionTarget::Native => None,
            _ => Some(crate::wsl::provider_spec(target, workspace, provider)?),
        };
        let mut command = if let Some(spec) = &mut spec {
            if provider == "claude" {
                let index = spec
                    .args
                    .iter()
                    .position(|v| v == "--config")
                    .context("invalid WSL launch configuration")?
                    + 1;
                let bytes = base64::engine::general_purpose::URL_SAFE.decode(&spec.args[index])?;
                let mut config: Value = serde_json::from_slice(&bytes)?;
                let sidecar = config["argv"][1]
                    .as_str()
                    .context("WSL sidecar path missing")?;
                let parent = sidecar
                    .rsplit_once('/')
                    .context("invalid Linux sidecar path")?
                    .0;
                config["argv"][1] = json!(format!("{parent}/history.mjs"));
                spec.args[index] =
                    base64::engine::general_purpose::URL_SAFE.encode(serde_json::to_vec(&config)?);
            }
            let mut command = Command::new(&spec.program);
            command.args(&spec.args);
            command
        } else {
            let mut command = if provider == "claude" {
                Command::new(crate::providers::node_path())
            } else {
                Command::new(match provider {
                    "codex" => "codex",
                    "kiro" => "kiro-cli",
                    "claude" => "node",
                    _ => bail!("unsupported history provider"),
                })
            };
            match provider {
                "codex" => {
                    command.arg("app-server");
                }
                "kiro" => {
                    command.args(["acp", "--agent-engine=v3"]);
                }
                "claude" => {
                    command.arg(crate::providers::sidecar_path().with_file_name("history.mjs"));
                }
                _ => unreachable!(),
            };
            command.current_dir(workspace);
            command
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context(
            "history runtime or sidecar is unavailable on the selected execution target",
        )?;
        let input = child.stdin.take().context("history input unavailable")?;
        let output = BufReader::new(child.stdout.take().context("history output unavailable")?);
        let lease = spec.map(|spec| spec.lease);
        if let Some(lease) = &lease {
            lease.start_renewal();
        }
        Ok(Self {
            child,
            input: Some(input),
            output,
            frame: Vec::new(),
            lease,
        })
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.input
            .as_mut()
            .context("history input closed")?
            .write_all(&bytes)
            .await?;
        Ok(())
    }
    async fn next(&mut self) -> Result<Value> {
        let bytes = crate::providers::read_json_line(&mut self.output, &mut self.frame).await?;
        serde_json::from_slice(&bytes).context("invalid structured history response")
    }
    async fn call(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let value = self.next().await?;
            if value["id"] == id && value.get("method").is_none() {
                if value.get("error").is_some() {
                    bail!("provider rejected history access; authenticate or upgrade its runtime");
                }
                return Ok(value["result"].clone());
            }
            if value.get("id").is_some() && value.get("method").is_some() {
                self.send(json!({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32601,"message":"History inspection cannot execute client tools"}})).await?;
            }
        }
    }
}
fn authorize_external(c: &Core, d: &Device) -> Result<()> {
    Core::authorize(d, false)?;
    let conn = c.store.0.lock().unwrap();
    let primary: String = conn.query_row(
        "SELECT value FROM metadata WHERE key='member_id'",
        [],
        |r| r.get(0),
    )?;
    if d.member_id != primary {
        bail!("forbidden: external history belongs to this OS execution account; use explicitly shared managed sessions");
    }
    Ok(())
}
fn same_workspace(target: &ExecutionTarget, left: &str, right: &str) -> bool {
    match target {
        ExecutionTarget::Native => match (
            Path::new(left).canonicalize(),
            Path::new(right).canonicalize(),
        ) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        },
        _ => left.trim_end_matches('/') == right.trim_end_matches('/'),
    }
}
async fn catalog(provider: &str, target: &ExecutionTarget, workspace: &str) -> Result<Vec<Value>> {
    let task = async {
        let mut process = HistoryProcess::start(provider, target, workspace)?;
        let rows = if provider == "claude" {
            process
                .send(json!({"workspace":workspace,"operation":"list"}))
                .await?;
            let result = process.next().await?;
            if result["ok"] != true {
                bail!("Claude local history inspection failed; install the history sidecar");
            }
            result["sessions"]
                .as_array()
                .cloned()
                .context("invalid Claude history response")?
        } else {
            let init = if provider == "codex" {
                json!({"clientInfo":{"name":"control_plane_history","title":"Control Plane History","version":env!("CARGO_PKG_VERSION")}})
            } else {
                json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"control-plane-history","version":env!("CARGO_PKG_VERSION")}})
            };
            let capabilities = process.call(1, "initialize", init).await?;
            if provider == "kiro" && !supports_replay(&capabilities) {
                bail!("this Kiro runtime does not advertise supported history loading/replay");
            }
            if provider == "codex" {
                process
                    .send(json!({"method":"initialized","params":{}}))
                    .await?;
            }
            let result = process
                .call(
                    2,
                    if provider == "codex" {
                        "thread/list"
                    } else {
                        "session/list"
                    },
                    if provider == "codex" {
                        json!({"limit":100,"cwd":workspace})
                    } else {
                        json!({"cwd":workspace})
                    },
                )
                .await?;
            result
                .get("data")
                .or_else(|| result.get("sessions"))
                .and_then(Value::as_array)
                .cloned()
                .context("provider did not return a history catalog")?
        };
        Ok::<_, anyhow::Error>(normalize(rows, provider, target, workspace))
    };
    tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .context("history catalog timed out")?
}
fn normalize(
    rows: Vec<Value>,
    provider: &str,
    target: &ExecutionTarget,
    workspace: &str,
) -> Vec<Value> {
    let mut sessions = Vec::new();
    for row in rows.into_iter().take(100) {
        let id = row
            .get("id")
            .or_else(|| row.get("sessionId"))
            .or_else(|| row.get("session_id"))
            .and_then(Value::as_str);
        let cwd = row
            .get("cwd")
            .or_else(|| row.get("workspace"))
            .and_then(Value::as_str);
        let (Some(id), Some(cwd)) = (id, cwd) else {
            continue;
        };
        if id.len() > 256 || !same_workspace(target, cwd, workspace) {
            continue;
        }
        let title = row
            .get("title")
            .or_else(|| row.get("preview"))
            .or_else(|| row.get("summary"))
            .and_then(Value::as_str)
            .unwrap_or("Imported session")
            .chars()
            .take(128)
            .collect::<String>();
        sessions.push(json!({"id":id,"title":title,"provider":provider,"workspace":workspace,"updated_at":row.get("updatedAt").or_else(||row.get("updated_at")).cloned().unwrap_or(Value::Null),"can_import":true}));
    }
    sessions
}
#[derive(Deserialize)]
struct HistoryQuery {
    provider: String,
    workspace: String,
    execution_target: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct ImportRequest {
    provider: String,
    workspace: String,
    provider_session_id: String,
    title: String,
    request_id: String,
    #[serde(default)]
    trusted: bool,
    #[serde(default)]
    execution_target: ExecutionTarget,
}
pub fn router() -> Router<Arc<Core>> {
    Router::new()
        .route("/v1/history", get(list))
        .route("/v1/history/import", post(import))
        .route("/v1/history/messages", get(messages))
        .route("/v1/sessions/{id}/history", get(managed_messages))
}
async fn list(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(v): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize_external(&c, &d)?;
    let target = v
        .execution_target
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    let workspace = c.workspace_for(&target, &v.workspace).await?;
    let sessions = catalog(&v.provider, &target, &workspace).await?;
    if !c.store.device_active(&d.id) {
        return Err(anyhow::anyhow!("unauthorized: access revoked").into());
    }
    Ok(Json(
        json!({"sessions":sessions,"limit":100,"scope":"execution_account","live_takeover":false}),
    ))
}
async fn import(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<ImportRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize_external(&c, &d)?;
    Core::authorize(&d, true)?;
    let body = serde_json::to_value(&v)?;
    if let Some(result) = c
        .store
        .previous(&d.id, &v.request_id, "history.import", &body)?
    {
        return Ok(Json(result));
    }
    if v.title.trim().is_empty()
        || v.title.len() > 256
        || v.provider_session_id.is_empty()
        || v.provider_session_id.len() > 256
        || v.request_id.is_empty()
        || v.request_id.len() > 128
    {
        return Err(anyhow::anyhow!("invalid history import metadata").into());
    }
    let workspace = c.workspace_for(&v.execution_target, &v.workspace).await?;
    let rows = catalog(&v.provider, &v.execution_target, &workspace).await?;
    if !rows
        .iter()
        .any(|row| row["id"] == v.provider_session_id && row["can_import"] == true)
    {
        return Err(anyhow::anyhow!(
            "history session was not found in this execution account and workspace catalog"
        )
        .into());
    }
    let transcript_page = transcript(
        &v.provider,
        &v.execution_target,
        &workspace,
        &v.provider_session_id,
        0,
    )
    .await?;
    let _guard = c.mutation.lock().await;
    if !c.store.device_active(&d.id) {
        return Err(
            anyhow::anyhow!("unauthorized: access was revoked during history inspection").into(),
        );
    }
    if let Some(result) = c
        .store
        .previous(&d.id, &v.request_id, "history.import", &body)?
    {
        return Ok(Json(result));
    }
    let s = Session {
        id: uuid::Uuid::new_v4().to_string(),
        provider: v.provider,
        workspace,
        title: v.title,
        state: "idle".into(),
        owner: d.member_id.clone(),
        created_at: now(),
        provider_session_id: Some(v.provider_session_id),
        shared: false,
        trusted: v.trusted,
        execution_target: v.execution_target,
    };
    let result = json!(s);
    let event=c.store.accept(&d.id,&v.request_id,"history.import",&body,&result,&s,"history_imported",json!({"provider":s.provider,"provider_session_id":s.provider_session_id,"text":"Imported stored provider transcript. Subsequent prompts resume its context.","messages":transcript_page["messages"],"next_offset":transcript_page["next_offset"],"truncated":transcript_page["truncated"]}))?;
    let _ = c.events.send(event);
    Ok(Json(result))
}

const TRANSCRIPT_PAGE_BYTES: usize = 2 * 1024 * 1024 - 4096;
struct TranscriptPage {
    offset: usize,
    seen: usize,
    bytes: usize,
    messages: Vec<Value>,
    more: bool,
    truncated: bool,
}
impl TranscriptPage {
    fn new(offset: usize) -> Self {
        Self {
            offset,
            seen: 0,
            bytes: 0,
            messages: Vec::new(),
            more: false,
            truncated: false,
        }
    }
    fn offer(&mut self, role: &str, text: &str, id: Option<&str>, kind: &str) -> Result<bool> {
        if self.more {
            return Ok(false);
        }
        if self.seen < self.offset {
            self.seen += 1;
            return Ok(true);
        }
        if self.messages.len() >= 100 {
            self.more = true;
            return Ok(false);
        }
        let mut limit = text.len().min(512 * 1024);
        while !text.is_char_boundary(limit) {
            limit -= 1
        }
        let id = id.unwrap_or("").chars().take(256).collect::<String>();
        let kind = kind.chars().take(128).collect::<String>();
        let (record, length) = loop {
            let record = json!({"role":role,"text":&text[..limit],"id":id,"kind":kind,"truncated":limit<text.len()});
            let length = serde_json::to_vec(&record)?.len() + 1;
            if length <= 512 * 1024 || limit == 0 {
                break (record, length);
            }
            limit /= 2;
            while !text.is_char_boundary(limit) {
                limit -= 1
            }
        };
        if self.bytes + length > TRANSCRIPT_PAGE_BYTES {
            self.more = true;
            return Ok(false);
        }
        self.truncated |= record["truncated"] == true;
        self.bytes += length;
        self.seen += 1;
        self.messages.push(record);
        Ok(true)
    }
    fn finish(self) -> Value {
        let next = if self.more {
            Some(self.offset + self.messages.len())
        } else {
            None
        };
        json!({"messages":self.messages,"next_offset":next,"truncated":self.truncated})
    }
}
fn content_text(value: &Value) -> String {
    match value{
 Value::String(text)=>text.clone(),Value::Array(items)=>items.iter().filter_map(|item|{if let Some(text)=item.as_str(){return Some(text.to_owned())}match item["type"].as_str(){Some("text"|"input_text"|"output_text")=>item["text"].as_str().map(str::to_owned),Some("tool_use")=>Some(json!({"tool":item["name"],"input":item["input"],"id":item["id"]}).to_string()),Some("tool_result")=>Some(json!({"tool_result":item["tool_use_id"],"content":item["content"],"is_error":item["is_error"]}).to_string()),_=>None}}).collect::<Vec<_>>().join("\n"),_=>String::new()}
}
fn codex_page(result: &Value, offset: usize) -> Result<Value> {
    let thread = result.get("thread").unwrap_or(result);
    let turns = thread["turns"].as_array().context(
        "this Codex runtime did not return transcript turns; upgrade to a supported App Server",
    )?;
    let mut page = TranscriptPage::new(offset);
    'turns: for turn in turns {
        let Some(items) = turn["items"].as_array() else {
            continue;
        };
        for item in items {
            let kind = item["type"].as_str().unwrap_or("unknown");
            let(role,text)=match kind{"userMessage"=>("user",content_text(&item["content"])),"agentMessage"=>("assistant",item["text"].as_str().unwrap_or("").to_owned()),"commandExecution"=>("tool",json!({"command":item["command"],"output":item["aggregatedOutput"],"exit_code":item["exitCode"]}).to_string()),"reasoning"=>("assistant",content_text(&item["summary"])),_=>("tool",item.to_string())};
            if !page.offer(role, &text, item["id"].as_str(), kind)? {
                break 'turns;
            }
        }
    }
    Ok(page.finish())
}
impl HistoryProcess {
    async fn call_with_replay<F>(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        mut on_notification: F,
    ) -> Result<Value>
    where
        F: FnMut(&Value) -> Result<()>,
    {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        loop {
            let value = self.next().await?;
            if value["id"] == id && value.get("method").is_none() {
                if value.get("error").is_some() {
                    bail!("provider rejected transcript access")
                }
                return Ok(value["result"].clone());
            }
            if value.get("id").is_some() && value.get("method").is_some() {
                self.send(json!({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32601,"message":"Transcript inspection cannot execute client tools"}})).await?
            } else {
                on_notification(&value)?
            }
        }
    }
}
struct KiroTranscript {
    session_id: String,
    page: TranscriptPage,
    pending_role: String,
    pending_text: String,
    pending_id: Option<String>,
}
impl KiroTranscript {
    fn flush(&mut self) -> Result<()> {
        if !self.pending_role.is_empty() {
            let text = std::mem::take(&mut self.pending_text);
            let role = std::mem::take(&mut self.pending_role);
            self.page
                .offer(&role, &text, self.pending_id.as_deref(), "message")?;
            self.pending_id = None
        }
        Ok(())
    }
    fn notification(&mut self, value: &Value) -> Result<()> {
        if self.page.more
            || value["method"] != "session/update"
            || value["params"]["sessionId"] != self.session_id
        {
            return Ok(());
        }
        let update = &value["params"]["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or("");
        match kind {
            "user_message_chunk" | "agent_message_chunk" => {
                let role = if kind == "user_message_chunk" {
                    "user"
                } else {
                    "assistant"
                };
                if !self.pending_role.is_empty() && self.pending_role != role {
                    self.flush()?
                }
                if self.page.more {
                    return Ok(());
                }
                self.pending_role = role.into();
                let content = &update["content"];
                let text = content["text"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| content_text(content));
                self.pending_text.push_str(&text);
                if self.pending_id.is_none() {
                    self.pending_id = update["messageId"].as_str().map(str::to_owned)
                }
                if self.pending_text.len() >= 512 * 1024 {
                    self.flush()?
                }
            }
            "tool_call" | "tool_call_update" => {
                self.flush()?;
                if !self.page.more {
                    self.page.offer(
                        "tool",
                        &update.to_string(),
                        update["toolCallId"].as_str(),
                        kind,
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
async fn transcript(
    provider: &str,
    target: &ExecutionTarget,
    workspace: &str,
    session_id: &str,
    offset: usize,
) -> Result<Value> {
    let task = async {
        let mut process = HistoryProcess::start(provider, target, workspace)?;
        if provider == "claude" {
            process.send(json!({"operation":"messages","workspace":workspace,"session_id":session_id,"offset":offset})).await?;
            let result = process.next().await?;
            if result["ok"] != true {
                bail!("Claude local transcript inspection failed")
            }
            let rows = result["messages"]
                .as_array()
                .context("Claude transcript API did not return messages")?;
            let mut page = TranscriptPage::new(offset);
            page.seen = offset;
            for row in rows.iter().take(100) {
                let message = row.get("message").unwrap_or(row);
                let role = message["role"]
                    .as_str()
                    .or_else(|| row["type"].as_str())
                    .unwrap_or("assistant");
                let role = if ["user", "assistant", "tool"].contains(&role) {
                    role
                } else {
                    "assistant"
                };
                let text = content_text(&message["content"]);
                if !page.offer(
                    role,
                    &text,
                    row["uuid"].as_str().or_else(|| row["id"].as_str()),
                    row["type"].as_str().unwrap_or("message"),
                )? {
                    break;
                }
            }
            let full = rows.len() >= 100;
            let mut reply = page.finish();
            if reply["next_offset"].is_null() && full {
                reply["next_offset"] = json!(offset + 100)
            }
            return Ok::<_, anyhow::Error>(reply);
        }
        let init = if provider == "codex" {
            json!({"clientInfo":{"name":"control_plane_history","title":"Control Plane History","version":env!("CARGO_PKG_VERSION")}})
        } else {
            json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"control-plane-history","version":env!("CARGO_PKG_VERSION")}})
        };
        let capabilities = process.call(1, "initialize", init).await?;
        if provider == "codex" {
            process
                .send(json!({"method":"initialized","params":{}}))
                .await?;
            let result = process
                .call(
                    2,
                    "thread/read",
                    json!({"threadId":session_id,"includeTurns":true}),
                )
                .await?;
            if let Some(cwd) = result["thread"]["cwd"].as_str() {
                if !same_workspace(target, cwd, workspace) {
                    bail!("forbidden: transcript workspace does not match")
                }
            }
            return codex_page(&result, offset);
        }
        if !supports_replay(&capabilities) {
            bail!("transcript access is unsupported: this Kiro runtime does not advertise session loading/replay")
        }
        let mut replay = KiroTranscript {
            session_id: session_id.into(),
            page: TranscriptPage::new(offset),
            pending_role: String::new(),
            pending_text: String::new(),
            pending_id: None,
        };
        process
            .call_with_replay(
                2,
                "session/load",
                json!({"sessionId":session_id,"cwd":workspace,"mcpServers":[]}),
                |value| replay.notification(value),
            )
            .await?;
        replay.flush()?;
        Ok(replay.page.finish())
    };
    tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .context("bounded transcript inspection timed out")?
}
fn supports_replay(capabilities: &Value) -> bool {
    let agent = &capabilities["agentCapabilities"];
    agent["loadSession"] == true
        || agent["sessionCapabilities"]["load"].is_object()
        || agent["sessionCapabilities"]["load"] == true
}
#[derive(Deserialize)]
struct MessagesQuery {
    provider: String,
    workspace: String,
    provider_session_id: String,
    offset: Option<usize>,
    execution_target: Option<String>,
}
#[derive(Deserialize)]
struct ManagedMessagesQuery {
    offset: Option<usize>,
}
async fn messages(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(v): Query<MessagesQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize_external(&c, &d)?;
    let offset = v.offset.unwrap_or(0);
    if offset > 1_000_000 || v.provider_session_id.is_empty() || v.provider_session_id.len() > 256 {
        return Err(anyhow::anyhow!("invalid history cursor or session").into());
    }
    let target = v
        .execution_target
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    let workspace = c.workspace_for(&target, &v.workspace).await?;
    let rows = catalog(&v.provider, &target, &workspace).await?;
    if !rows.iter().any(|row| row["id"] == v.provider_session_id) {
        return Err(anyhow::anyhow!("history session not found in this workspace catalog").into());
    }
    let result = transcript(
        &v.provider,
        &target,
        &workspace,
        &v.provider_session_id,
        offset,
    )
    .await?;
    if !c.store.device_active(&d.id) {
        return Err(anyhow::anyhow!("unauthorized: access revoked").into());
    }
    Ok(Json(result))
}
async fn managed_messages(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(v): Query<ManagedMessagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let s = c.session(&d, &id, false)?;
    let offset = v.offset.unwrap_or(0);
    if offset > 1_000_000 {
        return Err(anyhow::anyhow!("invalid history cursor").into());
    }
    if ["running", "interrupting"].contains(&s.state.as_str()) {
        return Err(anyhow::anyhow!(
            "wait for the active provider turn before loading its stored transcript"
        )
        .into());
    }
    let provider_id = s
        .provider_session_id
        .as_deref()
        .context("session has no stored provider history yet")?;
    let workspace = c.workspace_for(&s.execution_target, &s.workspace).await?;
    let result = transcript(
        &s.provider,
        &s.execution_target,
        &workspace,
        provider_id,
        offset,
    )
    .await?;
    c.session(&d, &id, false)?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_requires_matching_workspace() {
        let target = ExecutionTarget::Wsl {
            distribution: "Ubuntu".into(),
        };
        let rows = normalize(
            vec![
                json!({"id":"own","cwd":"/project"}),
                json!({"id":"other","cwd":"/other"}),
                json!({"id":"missing"}),
            ],
            "codex",
            &target,
            "/project",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "own");
    }
    #[test]
    fn imported_permissions_default_supervised() {
        let request:ImportRequest=serde_json::from_value(json!({"provider":"codex","workspace":"/project","provider_session_id":"thread","title":"Imported","request_id":"id"})).unwrap();
        assert!(!request.trusted);
        assert_eq!(request.execution_target, ExecutionTarget::Native);
    }
}
#[cfg(test)]
mod transcript_tests {
    use super::*;
    #[test]
    fn ordered_user_assistant_and_tool_output() {
        let result = json!({"thread":{"turns":[{"items":[{"type":"userMessage","id":"u","content":[{"type":"text","text":"Run tests"}]},{"type":"agentMessage","id":"a","text":"Running them."},{"type":"commandExecution","id":"t","command":"cargo test","aggregatedOutput":"3 passed","exitCode":0}]}]}});
        let page = codex_page(&result, 0).unwrap();
        let rows = page["messages"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["role"], "user");
        assert_eq!(rows[0]["text"], "Run tests");
        assert_eq!(rows[1]["role"], "assistant");
        assert!(rows[2]["text"].as_str().unwrap().contains("3 passed"));
        assert!(page["next_offset"].is_null());
    }
    #[test]
    fn pages_do_not_repeat_or_skip_rows() {
        let items=(0..101).map(|i|json!({"type":"agentMessage","id":format!("m{i}"),"text":format!("message {i}")})).collect::<Vec<_>>();
        let result = json!({"thread":{"turns":[{"items":items}]}});
        let first = codex_page(&result, 0).unwrap();
        assert_eq!(first["messages"].as_array().unwrap().len(), 100);
        assert_eq!(first["next_offset"], 100);
        let last = codex_page(&result, 100).unwrap();
        assert_eq!(last["messages"].as_array().unwrap().len(), 1);
        assert_eq!(last["messages"][0]["id"], "m100");
        assert!(last["next_offset"].is_null());
    }
    #[test]
    fn byte_limits_preserve_utf8_and_report_truncation() {
        let mut page = TranscriptPage::new(0);
        let text = "λ\"\n".repeat(400000);
        for i in 0..100 {
            if !page
                .offer("assistant", &text, Some(&format!("m{i}")), "message")
                .unwrap()
            {
                break;
            }
        }
        let result = page.finish();
        assert_eq!(result["truncated"], true);
        assert!(serde_json::to_vec(&result).unwrap().len() <= 2 * 1024 * 1024);
        assert!(result["next_offset"].as_u64().unwrap() > 0);
    }
    #[test]
    fn kiro_replay_coalesces_only_the_selected_session() {
        let mut replay = KiroTranscript {
            session_id: "owned".into(),
            page: TranscriptPage::new(0),
            pending_role: String::new(),
            pending_text: String::new(),
            pending_id: None,
        };
        for (session, kind, text) in [
            ("other", "user_message_chunk", "private"),
            ("owned", "user_message_chunk", "hello "),
            ("owned", "user_message_chunk", "world"),
            ("owned", "agent_message_chunk", "answer"),
        ] {
            replay.notification(&json!({"method":"session/update","params":{"sessionId":session,"update":{"sessionUpdate":kind,"content":{"type":"text","text":text}}}})).unwrap()
        }
        replay.flush().unwrap();
        let result = replay.page.finish();
        assert_eq!(result["messages"].as_array().unwrap().len(), 2);
        assert_eq!(result["messages"][0]["text"], "hello world");
        assert_eq!(result["messages"][1]["text"], "answer");
        assert!(!result.to_string().contains("private"));
    }
    #[test]
    fn kiro_requires_advertised_replay() {
        for value in [
            json!({}),
            json!({"agentCapabilities":{"loadSession":false}}),
            json!({"agentCapabilities":{"loadSession":"true"}}),
        ] {
            assert!(!supports_replay(&value))
        }
        assert!(supports_replay(
            &json!({"agentCapabilities":{"loadSession":true}})
        ));
    }
    #[tokio::test]
    async fn transcript_replay_denies_client_tool_execution() {
        let script = r#"import sys,json
request=json.loads(sys.stdin.readline())
print(json.dumps({'id':99,'method':'tools/call','params':{'name':'dangerous'}}),flush=True)
response=json.loads(sys.stdin.readline())
assert response['id']==99 and response['error']['code']==-32601
print(json.dumps({'method':'session/update','params':{'sessionId':'owned','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'stored'}}}}),flush=True)
print(json.dumps({'id':request['id'],'result':{}}),flush=True)
"#;
        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command
            .args(["-u", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut process = HistoryProcess {
            child,
            input,
            output,
            frame: Vec::new(),
            lease: None,
        };
        let mut count = 0;
        process
            .call_with_replay(1, "session/load", json!({}), |value| {
                assert_eq!(value["method"], "session/update");
                count += 1;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
