use crate::{desktop::Desktop, models::*, providers, store::Store};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;

pub struct Run {
    pub cancel: CancellationToken,
    pub decisions: mpsc::Sender<Value>,
    pub device_id: String,
}
pub struct Core {
    pub draining: std::sync::atomic::AtomicBool,
    pub identity: std::sync::OnceLock<crate::certificates::Authority>,
    pub store: Store,
    pub events: broadcast::Sender<Event>,
    pub runs: Mutex<HashMap<String, Run>>,
    pub mutation: Mutex<()>,
    pub roots: Vec<PathBuf>,
    pub desktop: Desktop,
    pub name: String,
    pub terminals: crate::terminal::Terminals,
    pub terminal_devices: Mutex<HashMap<String, String>>,
}
impl Core {
    pub fn new(store: Store, roots: Vec<PathBuf>, name: String) -> Result<Arc<Self>> {
        store.revoke_abandoned_automation()?;
        let (events, _) = broadcast::channel(2048);
        for mut s in store.list()? {
            if ["running", "interrupting"].contains(&s.state.as_str()) {
                s.state = "uncertain".into();
                store.save(&s)?;
                store.append(&s.id,"state",json!({"state":"uncertain","reason":"Host restarted. Review effects before resuming; work was not replayed."}))?;
            }
        }
        Ok(Arc::new(Self {
            draining: std::sync::atomic::AtomicBool::new(false),
            identity: std::sync::OnceLock::new(),
            store,
            events,
            runs: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
            roots,
            desktop: Desktop::new(),
            name,
            terminals: Default::default(),
            terminal_devices: Mutex::new(HashMap::new()),
        }))
    }
    pub fn authorize(device: &Device, write: bool) -> Result<()> {
        if device.revoked || (write && device.role == "viewer") {
            bail!("forbidden");
        }
        Ok(())
    }
    pub fn session(&self, device: &Device, id: &str, write: bool) -> Result<Session> {
        if write && self.draining.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("worker is draining for installation");
        }
        Self::authorize(device, write)?;
        let s = self.store.get(id)?;
        if !self.store.device_active(&device.id) {
            bail!("unauthorized: device or member revoked");
        }
        let granted = self.store.session_grant(id, &device.member_id)?;
        if s.owner != device.member_id
            && !(granted.as_deref() == Some("operator")
                || (!write && granted.as_deref() == Some("viewer")))
        {
            bail!("forbidden: session belongs to another member");
        }
        Ok(s)
    }
    pub fn workspace(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path).canonicalize()?;
        if !path.is_dir() || !self.roots.iter().any(|r| path.starts_with(r)) {
            bail!("workspace is outside configured roots");
        }
        Ok(path)
    }
    pub async fn workspace_for(&self, target: &ExecutionTarget, path: &str) -> Result<String> {
        match target {
            ExecutionTarget::Native => Ok(self.workspace(path)?.to_string_lossy().into_owned()),
            ExecutionTarget::Wsl { .. } => crate::wsl::validate_workspace(target, path).await,
        }
    }
    pub fn list(&self, device: &Device) -> Result<Value> {
        Ok(json!(self
            .store
            .list()?
            .into_iter()
            .filter(|s| s.owner == device.member_id
                || self
                    .store
                    .session_grant(&s.id, &device.member_id)
                    .ok()
                    .flatten()
                    .is_some())
            .collect::<Vec<_>>()))
    }
    pub async fn host(&self) -> Result<Value> {
        let desktop = self.desktop.status().await;
        Ok(
            json!({"id":self.store.host_id()?,"name":self.name,"os":std::env::consts::OS,"protocol_version":1,"workspace_roots":self.roots,"providers":providers::discover().await,"desktop":desktop,"capabilities":{"multi_os_user_broker":false,"device_roles":true,"grpc":true,"websocket_events":true,"desktop_video":desktop["video_streaming"].as_bool().unwrap_or(false),"terminal":true}}),
        )
    }
    pub async fn create(&self, device: &Device, input: CreateSession) -> Result<Value> {
        if self.draining.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("worker is draining for installation");
        }
        Self::authorize(device, true)?;
        let _guard = self.mutation.lock().await;
        let body = serde_json::to_value(&input)?;
        if let Some(result) = self
            .store
            .previous(&device.id, &input.request_id, "create", &body)?
        {
            return Ok(result);
        }
        if !["codex", "claude", "kiro"].contains(&input.provider.as_str()) {
            bail!("unsupported provider");
        }
        if input.title.trim().is_empty() || input.title.len() > 256 {
            bail!("title must be 1–256 characters");
        }
        let workspace = self
            .workspace_for(&input.execution_target, &input.workspace)
            .await?;
        let s = Session {
            id: uuid::Uuid::new_v4().to_string(),
            provider: input.provider,
            workspace,
            title: input.title,
            state: "idle".into(),
            owner: device.member_id.clone(),
            created_at: now(),
            provider_session_id: None,
            shared: false,
            trusted: input.trusted,
            execution_target: input.execution_target,
        };
        let result = json!(s);
        let e = self.store.accept(
            &device.id,
            &input.request_id,
            "create",
            &body,
            &result,
            &s,
            "state",
            json!({"state":"idle"}),
        )?;
        let _ = self.events.send(e);
        Ok(result)
    }
    pub async fn prompt(
        self: &Arc<Self>,
        device: &Device,
        id: &str,
        input: Prompt,
    ) -> Result<Value> {
        let _guard = self.mutation.lock().await;
        let mut s = self.session(device, id, true)?;
        let body = json!({"session_id":id,"text":input.text});
        if let Some(result) = self
            .store
            .previous(&device.id, &input.request_id, "prompt", &body)?
        {
            return Ok(result);
        }
        if input.text.trim().is_empty() || input.text.len() > 256 * 1024 {
            bail!("prompt must be 1–262144 bytes");
        }
        if s.state == "archived" || self.runs.lock().await.contains_key(id) {
            bail!("session is archived or already running");
        }
        self.workspace_for(&s.execution_target, &s.workspace)
            .await?;
        s.state = "running".into();
        let result = json!({"accepted":true,"id":id,"request_id":input.request_id});
        let e = self.store.accept(
            &device.id,
            &input.request_id,
            "prompt",
            &body,
            &result,
            &s,
            "user_message",
            json!({"text":input.text}),
        )?;
        let _ = self.events.send(e);
        let cancel = CancellationToken::new();
        let (decisions, rx) = mpsc::channel(32);
        self.runs.lock().await.insert(
            id.into(),
            Run {
                cancel: cancel.clone(),
                decisions,
                device_id: device.id.clone(),
            },
        );
        self.emit(id, "state", json!({"state":"running"}))?;
        let core = self.clone();
        tokio::spawn(async move {
            let (mcp, automation_device) =
                core.computer_tools(s.trusted, &s.id)
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error,"computer tools unavailable");
                        (None, None)
                    });
            let (tx, mut events) = mpsc::channel::<Value>(256);
            let sink = core.clone();
            let sid = s.id.clone();
            let reader_cancel = cancel.clone();
            let reader = tokio::spawn(async move {
                while let Some(v) = events.recv().await {
                    let kind = v
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("provider_event");
                    let payload = v.get("payload").cloned().unwrap_or(v.clone());
                    if kind == "provider_session" {
                        if let Some(provider_id) = payload
                            .get("provider_session_id")
                            .or_else(|| payload.get("id"))
                            .and_then(Value::as_str)
                        {
                            let _guard = sink.mutation.lock().await;
                            if let Ok(mut session) = sink.store.get(&sid) {
                                session.provider_session_id = Some(provider_id.into());
                                let _ = sink.store.save(&session);
                            }
                        }
                    }
                    if let Err(e) = sink.emit(&sid, kind, payload) {
                        tracing::error!(error=%e,"event persistence failed; cancelling execution");
                        reader_cancel.cancel();
                    }
                }
            });
            let outcome = providers::run_target(
                &s.execution_target,
                &s.provider,
                Path::new(&s.workspace),
                &input.text,
                s.provider_session_id.as_deref(),
                s.trusted,
                cancel.clone(),
                tx,
                rx,
                mcp,
            )
            .await;
            let _ = reader.await;
            if let Some(device) = automation_device {
                let _ = core.store.revoke("automation-lifecycle", &device);
            }
            // Serialize terminal state with interrupt/archive/share mutations.
            let _guard = core.mutation.lock().await;
            if let Ok(mut current) = core.store.get(&s.id) {
                current.state = if cancel.is_cancelled() {
                    "interrupted"
                } else if outcome.is_ok() {
                    "idle"
                } else {
                    "failed"
                }
                .into();
                if let Err(error) = outcome {
                    let _ = core.emit(&s.id, "error", json!({"message":error.to_string()}));
                }
                let _ = core.store.save(&current);
                let _ = core.emit(&s.id, "state", json!({"state":current.state}));
            }
            core.runs.lock().await.remove(&s.id);
        });
        Ok(result)
    }
    fn computer_tools(
        &self,
        trusted: bool,
        session: &str,
    ) -> Result<(Option<Value>, Option<String>)> {
        if !trusted {
            return Ok((None, None));
        }
        let Ok(url) = std::env::var("CONTROL_PLANE_URL") else {
            return Ok((None, None));
        };
        let command = std::env::var_os("CONTROL_PLANE_COMPUTER_TOOLS")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe().ok().map(|p| {
                    p.with_file_name(if cfg!(windows) {
                        "control-plane-computer-tools.exe"
                    } else {
                        "control-plane-computer-tools"
                    })
                })
            })
            .ok_or_else(|| anyhow::anyhow!("computer tools executable unavailable"))?;
        if !command.is_file() {
            bail!("computer tools executable does not exist");
        }
        let secret = self.store.issue_pairing("operator")?;
        let device = self.store.pair(&secret, "Ephemeral computer-use agent")?;
        self.store.mark_automation(
            device["device_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing automation device id"))?,
            session,
        )?;
        let id = device["device_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("automation device identity missing"))?;
        let mut environment =
            json!({"CONTROL_PLANE_URL":url,"CONTROL_PLANE_DEVICE_TOKEN":device["token"]});
        let result = (|| -> Result<()> {
            if reqwest::Url::parse(&url)?.scheme() == "https" {
                let authority = self.identity.get().ok_or_else(|| {
                    anyhow::anyhow!("TLS computer tools require this worker's device issuer")
                })?;
                let key = rcgen::KeyPair::generate()?;
                let csr = rcgen::CertificateParams::new(Vec::<String>::new())?
                    .serialize_request(&key)?
                    .pem()?;
                let certificate = authority.issue(id, &csr)?;
                self.store.bind_certificate(
                    id,
                    certificate["fingerprint"].as_str().unwrap(),
                    certificate["expires_at"].as_i64().unwrap(),
                )?;
                environment["CONTROL_PLANE_CLIENT_CERT_PEM"] =
                    certificate["certificate_pem"].clone();
                environment["CONTROL_PLANE_CLIENT_KEY_PEM"] = json!(key.serialize_pem());
                if let Ok(ca) = std::env::var("CONTROL_PLANE_SERVER_CA_PEM") {
                    if ca.len() > 65536 {
                        bail!("server CA exceeds limit");
                    }
                    environment["CONTROL_PLANE_SERVER_CA_PEM"] = json!(ca);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.store.revoke("failed-automation-enrollment", id);
            return Err(error);
        }
        Ok((
            Some(json!({"command":command,"args":[],"env":environment})),
            Some(id.to_owned()),
        ))
    }
    pub fn emit(&self, id: &str, kind: &str, payload: Value) -> Result<()> {
        let notification = if kind == "state" && payload["state"] == "idle" {
            Some("completed")
        } else if kind == "error" {
            Some("failed")
        } else if [
            "approval_requested",
            "approval_request",
            "permission_request",
        ]
        .contains(&kind)
        {
            Some("approval")
        } else {
            None
        };
        let e = self.store.append(id, kind, payload)?;
        let _ = self.events.send(e);
        if let Some(kind) = notification {
            self.notify(kind);
        }
        Ok(())
    }
    fn notify(&self, kind: &'static str) {
        let Ok(url) = std::env::var("CONTROL_PLANE_PUSH_URL") else {
            return;
        };
        let Ok(token) = std::env::var("CONTROL_PLANE_PUSH_TOKEN") else {
            return;
        };
        let Ok(parsed) = reqwest::Url::parse(&url) else {
            return;
        };
        if parsed.scheme() != "https"
            && !(parsed.scheme() == "http"
                && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
        {
            return;
        }
        let Ok(targets) = self.store.push_targets() else {
            return;
        };
        tokio::spawn(async move {
            let Ok(client) = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
            else {
                return;
            };
            for mut target in targets {
                target["kind"] = json!(kind);
                target["event_id"] = json!(uuid::Uuid::new_v4().to_string());
                match client
                    .post(format!("{}/v1/notify", url.trim_end_matches('/')))
                    .bearer_auth(&token)
                    .json(&target)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {}
                    _ => tracing::warn!(
                        "optional push notification failed; session history remains available"
                    ),
                }
            }
        });
    }
    pub async fn interrupt(&self, device: &Device, id: &str) -> Result<Value> {
        let _guard = self.mutation.lock().await;
        let mut s = self.session(device, id, true)?;
        if let Some(run) = self.runs.lock().await.get(id) {
            run.cancel.cancel();
            s.state = "interrupting".into();
            self.store.save(&s)?;
            self.emit(id, "state", json!({"state":"interrupting"}))?;
        }
        Ok(json!({"accepted":true}))
    }
    pub async fn archive(&self, device: &Device, id: &str) -> Result<Value> {
        let _guard = self.mutation.lock().await;
        let mut s = self.session(device, id, true)?;
        if self.runs.lock().await.contains_key(id) {
            bail!("interrupt and wait for execution to stop before archiving");
        }
        self.terminals.stop(id);
        s.state = "archived".into();
        self.store.save(&s)?;
        self.emit(id, "state", json!({"state":"archived"}))?;
        Ok(json!(s))
    }
    pub async fn decide(&self, device: &Device, id: &str, decision: Value) -> Result<Value> {
        self.session(device, id, true)?;
        let runs = self.runs.lock().await;
        let run = runs
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("session is not running"))?;
        run.decisions
            .try_send(decision)
            .map_err(|_| anyhow::anyhow!("decision queue unavailable"))?;
        Ok(json!({"accepted":true}))
    }
    pub async fn emergency_stop(&self, device: &Device) -> Result<Value> {
        Self::authorize(device, true)?;
        let runs = self.runs.lock().await;
        for (id, run) in runs.iter() {
            if device.role == "administrator" || self.store.get(id)?.owner == device.member_id {
                run.cancel.cancel();
            }
        }
        for s in self.store.list()? {
            if device.role == "administrator" || s.owner == device.member_id {
                self.terminals.stop(&s.id);
            }
        }
        self.desktop.stop();
        Ok(
            json!({"accepted":true,"message":"Cancellation requested; completed effects are not undone."}),
        )
    }
    pub async fn revoke_inactive_devices(&self) {
        for run in self.runs.lock().await.values() {
            if !self.store.device_active(&run.device_id) {
                run.cancel.cancel();
            }
        }
        let mut owners = self.terminal_devices.lock().await;
        owners.retain(|session, device| {
            let active = self.store.device_active(device);
            if !active {
                self.terminals.stop(session);
            }
            active
        });
    }
}
