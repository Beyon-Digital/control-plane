//! A single interactive desktop controller shared by remote clients and agents.
//! OS permissions are enforced by the separately signed, user-session helper.
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
};

type HelperRequest = (Value, oneshot::Sender<Result<Value>>);
struct HelperSession {
    generation: u64,
    sender: mpsc::Sender<HelperRequest>,
}

const LEASE_TTL: Duration = Duration::from_secs(15);
const FRAME_TTL: Duration = Duration::from_secs(5);
struct Lease {
    id: String,
    controller: String,
    expires: Instant,
}
#[derive(Default)]
struct State {
    display_id: Option<String>,
    lease: Option<Lease>,
    geometry: u64,
    width: u64,
    height: u64,
    last_frame: Option<Instant>,
    generation: u64,
    cancel: tokio_util::sync::CancellationToken,
    video_cancel: tokio_util::sync::CancellationToken,
}
pub struct Desktop {
    state: Mutex<State>,
    helper: Option<PathBuf>,
    video_helper: Option<PathBuf>,
    operation: tokio::sync::Mutex<()>,
    helper_session: tokio::sync::Mutex<Option<HelperSession>>,
    pub video_slots: std::sync::Arc<tokio::sync::Semaphore>,
}
impl Default for Desktop {
    fn default() -> Self {
        Self::new()
    }
}
impl Desktop {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            helper: std::env::var_os("CONTROL_PLANE_DESKTOP_HELPER").map(PathBuf::from),
            video_helper: std::env::var_os("CONTROL_PLANE_VIDEO_HELPER").map(PathBuf::from),
            operation: tokio::sync::Mutex::new(()),
            helper_session: tokio::sync::Mutex::new(None),
            video_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }
    async fn invoke(&self, mut input: Value) -> Result<Value> {
        let (cancel, generation) = {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("desktop state poisoned"))?;
            if input["generation"]
                .as_u64()
                .is_some_and(|value| value != state.generation)
            {
                bail!("desktop stopped");
            }
            if input.get("display_id").is_none() {
                if let Some(id) = &state.display_id {
                    input["display_id"] = json!(id);
                }
            }
            (state.cancel.clone(), state.generation)
        };
        let helper = self.helper.as_ref().ok_or_else(|| anyhow!("desktop unavailable: configure CONTROL_PLANE_DESKTOP_HELPER with the signed macOS helper"))?;
        if helper.extension().and_then(|v| v.to_str()) != Some("ps1") {
            let mut session = self.helper_session.lock().await;
            if session
                .as_ref()
                .is_none_or(|s| s.generation != generation || s.sender.is_closed())
            {
                let mut command = if helper.extension().and_then(|v| v.to_str()) == Some("py") {
                    let mut c = Command::new("python3");
                    c.arg(helper);
                    c
                } else {
                    Command::new(helper)
                };
                let mut child = command
                    .arg("--stdio-session")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .context("start persistent desktop helper")?;
                let mut stdin = child.stdin.take().context("helper stdin")?;
                let mut stdout = BufReader::new(child.stdout.take().context("helper stdout")?);
                let (sender, mut requests) = mpsc::channel::<HelperRequest>(4);
                let stop = cancel.clone();
                tokio::spawn(async move {
                    loop {
                        let request = tokio::select! { biased; _=stop.cancelled()=>break, value=requests.recv()=>match value { Some(v)=>v,None=>break } };
                        let (input, response) = request;
                        let exchange = async {
                            if stop.is_cancelled() {
                                bail!("desktop operation stopped");
                            }
                            let mut bytes = serde_json::to_vec(&input)?;
                            bytes.push(b'\n');
                            stdin.write_all(&bytes).await?;
                            stdin.flush().await?;
                            let mut line = Vec::new();
                            let count = (&mut stdout)
                                .take(32 * 1024 * 1024 + 1)
                                .read_until(b'\n', &mut line)
                                .await?;
                            if count == 0 || count > 32 * 1024 * 1024 || line.last() != Some(&b'\n')
                            {
                                bail!("invalid or oversized desktop helper response");
                            }
                            let value: Value = serde_json::from_slice(&line)
                                .context("invalid desktop helper response")?;
                            if let Some(error) = value.get("error") {
                                bail!("{}", error.as_str().unwrap_or("desktop helper failed"));
                            }
                            Ok::<_, anyhow::Error>(value)
                        };
                        let result = tokio::select! { biased; _=stop.cancelled()=>Err(anyhow!("desktop operation stopped")), value=tokio::time::timeout(Duration::from_secs(10),exchange)=>value.context("desktop helper timed out").and_then(|v|v) };
                        // Any failed exchange resets the helper; no stale response may
                        // be consumed by a later command after timeout/cancellation.
                        let failed = result.is_err();
                        let _ = response.send(result);
                        if failed {
                            break;
                        }
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                });
                *session = Some(HelperSession { generation, sender });
            }
            let sender = session.as_ref().unwrap().sender.clone();
            drop(session);
            let (response, receiver) = oneshot::channel();
            tokio::select! { _=cancel.cancelled()=>bail!("desktop operation stopped"),result=sender.send((input,response))=>result.context("desktop helper unavailable")? }
            return tokio::select! { _=cancel.cancelled()=>Err(anyhow!("desktop operation stopped")),result=receiver=>result.context("desktop helper ended")? };
        }
        let mut command = if helper.extension().and_then(|v| v.to_str()) == Some("ps1") {
            let mut command = Command::new("powershell.exe");
            command
                .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
                .arg(helper);
            command
        } else if helper.extension().and_then(|v| v.to_str()) == Some("py") {
            let mut command = Command::new("python3");
            command.arg(helper);
            command
        } else {
            Command::new(helper)
        };
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start desktop helper")?;
        let mut stdin = child.stdin.take().context("helper stdin")?;
        stdin
            .write_all(serde_json::to_vec(&input)?.as_slice())
            .await?;
        drop(stdin);
        let output = tokio::select! {
            _ = cancel.cancelled() => bail!("desktop operation stopped"),
            result = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()) => result.context("desktop helper timed out")??
        };
        if output.stdout.len() > 32 * 1024 * 1024 {
            bail!("desktop helper output exceeds limit");
        }
        let value: Value =
            serde_json::from_slice(&output.stdout).context("invalid desktop helper response")?;
        if !output.status.success() || value.get("error").is_some() {
            bail!(
                "{}",
                value["error"].as_str().unwrap_or("desktop helper failed")
            );
        }
        Ok(value)
    }
    pub async fn status(&self) -> Value {
        match self.invoke(json!({"command":"status"})).await {
            Ok(mut value) => {
                value["lease_seconds"] = json!(15);
                {
                    let mut state = self.state.lock().unwrap();
                    if state.display_id.is_none() {
                        state.display_id = value["selected_display_id"].as_str().map(str::to_owned);
                    }
                    if let Some(id) = &state.display_id {
                        value["selected_display_id"] = json!(id);
                    }
                }
                if cfg!(target_os = "macos") {
                    value["video_streaming"] = json!(
                        value["capture_available"].as_bool().unwrap_or(false)
                            && !value["locked"].as_bool().unwrap_or(true)
                    );
                }
                if self.video_helper.is_some() {
                    let capability = async {
                        let helper = self.video_helper.as_ref().unwrap();
                        let mut status =
                            if helper.extension().and_then(|v| v.to_str()) == Some("py") {
                                let mut c =
                                    Command::new(if cfg!(windows) { "python" } else { "python3" });
                                c.arg(helper);
                                c
                            } else {
                                Command::new(helper)
                            };
                        let output = status.arg("--status").kill_on_drop(true).output().await?;
                        if output.stdout.len() > 65536 {
                            bail!("invalid video helper status")
                        };
                        Ok::<Value, anyhow::Error>(serde_json::from_slice(&output.stdout)?)
                    };
                    match tokio::time::timeout(Duration::from_secs(8), capability).await {
                        Ok(Ok(status)) => {
                            value["video_streaming"] = status["video_streaming"].clone();
                            value["video_reason"] = status["reason"].clone()
                        }
                        _ => {
                            value["video_streaming"] = json!(false);
                            value["video_reason"] = json!("Video helper unavailable")
                        }
                    }
                }
                value
            }
            Err(error) => {
                json!({"available":false,"reason":error.to_string(),"platform":std::env::consts::OS,"video_streaming":false})
            }
        }
    }
    pub async fn select_display(&self, id: &str) -> Result<Value> {
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            bail!("invalid display identifier");
        }
        let _operation = self.operation.lock().await;
        self.stop();
        let result = self
            .invoke(json!({"command":"select_display","display_id":id}))
            .await?;
        if result["selected_display_id"].as_str() != Some(id) {
            bail!("desktop helper selected an unexpected display");
        }
        self.state.lock().unwrap().display_id = Some(id.to_owned());
        Ok(result)
    }
    pub async fn accessibility(&self) -> Result<Value> {
        let _operation = self.operation.lock().await;
        self.invoke(json!({"command":"accessibility"})).await
    }
    pub fn stop_token(&self) -> tokio_util::sync::CancellationToken {
        self.state.lock().unwrap().video_cancel.clone()
    }
    pub fn video_command(&self) -> Result<Command> {
        let helper = self
            .video_helper
            .as_ref()
            .or(self.helper.as_ref())
            .context("desktop video helper is not configured")?;
        if helper.extension().and_then(|v| v.to_str()) == Some("ps1") {
            bail!("H264 streaming unavailable for this desktop backend; use snapshots");
        }
        let mut command = if helper.extension().and_then(|v| v.to_str()) == Some("py") {
            let mut c = Command::new(if cfg!(windows) { "python" } else { "python3" });
            c.arg(helper);
            c
        } else {
            Command::new(helper)
        };
        command.arg("--video");
        if let Some(id) = &self.state.lock().unwrap().display_id {
            command.arg("--display-id").arg(id);
        }
        Ok(command)
    }
    pub fn observe_video(&self, geometry: u64, width: u64, height: u64) -> Result<()> {
        if width == 0 || height == 0 || width > 32768 || height > 32768 {
            bail!("invalid desktop video geometry");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("desktop state poisoned"))?;
        state.geometry = geometry;
        state.width = width;
        state.height = height;
        state.last_frame = Some(Instant::now());
        Ok(())
    }
    pub fn acquire(&self, controller: &str) -> Result<Value> {
        if controller.is_empty() || controller.len() > 256 {
            bail!("invalid controller identity");
        }
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("desktop state poisoned"))?;
        let now = Instant::now();
        if let Some(lease) = &mut s.lease {
            if lease.expires > now {
                if lease.controller != controller {
                    bail!("desktop already controlled by another device");
                }
                lease.expires = now + LEASE_TTL;
                return Ok(
                    json!({"lease_id":lease.id,"expires_in":15,"geometry_version":s.geometry}),
                );
            }
        }
        let id = uuid::Uuid::new_v4().to_string();
        s.lease = Some(Lease {
            id: id.clone(),
            controller: controller.into(),
            expires: now + LEASE_TTL,
        });
        Ok(json!({"lease_id":id,"expires_in":15,"geometry_version":s.geometry}))
    }
    pub async fn frame(&self) -> Result<Value> {
        let _operation = self.operation.lock().await;
        let frame = self.invoke(json!({"command":"frame"})).await?;
        let width = frame["width"]
            .as_u64()
            .filter(|x| *x > 0)
            .context("invalid frame width")?;
        let height = frame["height"]
            .as_u64()
            .filter(|x| *x > 0)
            .context("invalid frame height")?;
        let fingerprint = frame["geometry_version"]
            .as_u64()
            .context("missing geometry version")?;
        frame["image_base64"]
            .as_str()
            .context("missing screenshot image")?;
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow!("desktop state poisoned"))?;
        s.width = width;
        s.height = height;
        s.geometry = fingerprint;
        s.last_frame = Some(Instant::now());
        Ok(frame)
    }
    pub async fn action(
        &self,
        lease_id: &str,
        geometry_version: u64,
        action: Value,
    ) -> Result<Value> {
        let _operation = self.operation.lock().await;
        let generation = {
            let s = self
                .state
                .lock()
                .map_err(|_| anyhow!("desktop state poisoned"))?;
            let lease = s.lease.as_ref().context("desktop control lease required")?;
            if lease.id != lease_id || lease.expires <= Instant::now() {
                bail!("invalid or expired desktop lease");
            }
            if s.geometry != geometry_version {
                bail!("desktop geometry changed; capture a new frame");
            }
            if s.last_frame.is_none_or(|time| time.elapsed() > FRAME_TTL) {
                bail!("screenshot stale; capture a new frame");
            }
            Self::validate_action(&action, s.width, s.height)?;
            s.generation
        };
        // Recheck after helper startup as well: the helper compares geometry immediately
        // before injecting input. A stop invalidates every subsequent request.
        let current = self
            .state
            .lock()
            .map_err(|_| anyhow!("desktop state poisoned"))?
            .generation;
        if current != generation {
            bail!("desktop stopped");
        }
        self.invoke(json!({"command":"action","geometry_version":geometry_version,"action":action,"generation":generation})).await
    }
    fn validate_action(action: &Value, width: u64, height: u64) -> Result<()> {
        match action["type"].as_str().unwrap_or("") {
            "click" => {
                let x = action["x"].as_f64().context("click x required")?;
                let y = action["y"].as_f64().context("click y required")?;
                if !x.is_finite()
                    || !y.is_finite()
                    || x < 0.0
                    || y < 0.0
                    || x >= width as f64
                    || y >= height as f64
                {
                    bail!("click outside captured display");
                }
                if action["button"]
                    .as_str()
                    .is_some_and(|v| v != "left" && v != "right")
                {
                    bail!("invalid mouse button");
                }
            }
            "text" => {
                let text = action["text"].as_str().context("text required")?;
                if text.len() > 4096 {
                    bail!("text input exceeds 4096 bytes");
                }
            }
            "key" => {
                if action["key"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 32)
                    .is_none()
                {
                    bail!("key required");
                }
            }
            "scroll" => {
                for (key, alias) in [("dx", "delta_x"), ("dy", "delta_y")] {
                    if action
                        .get(key)
                        .or_else(|| action.get(alias))
                        .and_then(Value::as_i64)
                        .is_none_or(|v| !(-1200..=1200).contains(&v))
                    {
                        bail!("scroll values must be integers between -1200 and 1200");
                    }
                }
            }
            _ => bail!("unsupported desktop action"),
        }
        Ok(())
    }
    pub fn takeover(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.lease = None;
            s.generation = s.generation.wrapping_add(1);
            s.cancel.cancel();
            s.cancel = tokio_util::sync::CancellationToken::new();
        }
    }
    pub fn stop(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.lease = None;
            s.last_frame = None;
            s.generation = s.generation.wrapping_add(1);
            s.cancel.cancel();
            s.cancel = tokio_util::sync::CancellationToken::new();
            s.video_cancel.cancel();
            s.video_cancel = tokio_util::sync::CancellationToken::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_takeover_cancels_agent_input_without_stopping_video() {
        let d = Desktop::new();
        let input = d.state.lock().unwrap().cancel.clone();
        let video = d.stop_token();
        d.acquire("agent").unwrap();
        d.takeover();
        assert!(input.is_cancelled());
        assert!(!video.is_cancelled());
        assert!(d.acquire("human").is_ok());
        d.stop();
        assert!(video.is_cancelled());
    }
    #[test]
    fn leases_are_exclusive_and_revocable() {
        let d = Desktop::new();
        let a = d.acquire("a").unwrap();
        assert_eq!(a["lease_id"], d.acquire("a").unwrap()["lease_id"]);
        assert!(d.acquire("b").is_err());
        d.stop();
        assert!(d.acquire("b").is_ok());
    }
    #[tokio::test]
    async fn input_requires_recent_matching_frame() {
        let d = Desktop::new();
        let lease = d.acquire("a").unwrap();
        let id = lease["lease_id"].as_str().unwrap();
        assert!(d
            .action(id, 0, json!({"type":"key","key":"enter"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("stale"));
        assert!(d
            .action(id, 1, json!({"type":"key","key":"enter"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("geometry"));
    }
    #[tokio::test]
    async fn stop_cancels_inflight_and_rejects_previously_valid_actions() {
        let d = Desktop::new();
        let token = d.state.lock().unwrap().cancel.clone();
        d.stop();
        assert!(token.is_cancelled());
        let error = d
            .invoke(json!({"command":"action","generation":0}))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "desktop stopped");
        assert!(!d.state.lock().unwrap().cancel.is_cancelled());
    }
    #[test]
    fn coordinates_and_payloads_are_bounded() {
        assert!(
            Desktop::validate_action(&json!({"type":"click","x":100,"y":0}), 100, 100).is_err()
        );
        assert!(Desktop::validate_action(&json!({"type":"click","x":99,"y":0}), 100, 100).is_ok());
        assert!(Desktop::validate_action(
            &json!({"type":"text","text":"x".repeat(4097)}),
            100,
            100
        )
        .is_err());
        assert!(
            Desktop::validate_action(&json!({"type":"scroll","dx":0,"dy":1201}), 100, 100).is_err()
        );
    }
}
