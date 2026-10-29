//! Explicit WSL routing. Linux paths never pass through Windows Path::canonicalize.
use crate::models::ExecutionTarget;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, process::Stdio, sync::Mutex, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    pub user: String,
    pub roots: Vec<String>,
    pub bridge: String,
    #[serde(default)]
    pub claude_sidecar: Option<String>,
    #[serde(default)]
    pub computer_tools: Option<String>,
}

fn distributions() -> Result<BTreeMap<String, Distribution>> {
    let value = match std::env::var("CONTROL_PLANE_WSL_CONFIG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let configs: BTreeMap<String, Distribution> =
        serde_json::from_str(&value).context("invalid CONTROL_PLANE_WSL_CONFIG JSON")?;
    for (name, config) in &configs {
        validate_distribution(name)?;
        config.validate()?;
    }
    Ok(configs)
}

fn validate_distribution(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('-')
        || value
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ')))
    {
        bail!("invalid WSL distribution name");
    }
    Ok(())
}

pub fn validate_linux_path(value: &str) -> Result<()> {
    if !value.starts_with('/')
        || value == "/"
        || value.contains(['\\', '\0', '\n', '\r'])
        || value.split('/').any(|p| p == "..")
        || value == "/mnt"
        || value.starts_with("/mnt/")
    {
        bail!("WSL workspace must use an absolute Linux filesystem path without traversal");
    }
    Ok(())
}

impl Distribution {
    fn validate(&self) -> Result<()> {
        if self.user.is_empty()
            || self.user.len() > 64
            || self.user.starts_with('-')
            || !self
                .user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            bail!("invalid WSL user");
        }
        if self.roots.is_empty() {
            bail!("WSL distribution needs at least one allowed root");
        }
        for root in &self.roots {
            validate_linux_path(root)?;
        }
        validate_linux_path(&self.bridge)?;
        if let Some(path) = &self.claude_sidecar {
            validate_linux_path(path)?;
        }
        if let Some(path) = &self.computer_tools {
            validate_linux_path(path)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Bridge {
    launcher: String,
    prefix: Vec<String>,
    config: Distribution,
}
impl Bridge {
    fn load(distribution: &str) -> Result<Self> {
        validate_distribution(distribution)?;
        if !cfg!(windows) && std::env::var_os("CONTROL_PLANE_WSL_LAUNCHER").is_none() {
            bail!("WSL execution requires a Windows worker");
        }
        let config = distributions()?
            .remove(distribution)
            .ok_or_else(|| anyhow::anyhow!("WSL distribution is not configured for this worker"))?;
        let launcher =
            std::env::var("CONTROL_PLANE_WSL_LAUNCHER").unwrap_or_else(|_| "wsl.exe".into());
        let prefix = vec![
            "--distribution".into(),
            distribution.into(),
            "--user".into(),
            config.user.clone(),
            "--exec".into(),
            "python3".into(),
            config.bridge.clone(),
        ];
        Ok(Self {
            launcher,
            prefix,
            config,
        })
    }
    fn command(&self, mode: &str) -> Command {
        let mut command = Command::new(&self.launcher);
        command.args(&self.prefix).arg(mode).kill_on_drop(true);
        command
    }
    async fn request(&self, mut payload: Value) -> Result<Value> {
        payload["roots"] = json!(self.config.roots);
        payload["claude_sidecar"] = json!(self.config.claude_sidecar);
        let mut command = self.command("request");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child=command.spawn().context("could not launch configured WSL bridge; install it explicitly in the selected distro/user")?;
        let mut input = child.stdin.take().context("WSL stdin unavailable")?;
        let mut output = child.stdout.take().context("WSL stdout unavailable")?;
        let bytes = serde_json::to_vec(&payload)?;
        if bytes.len() > 4 * 1024 * 1024 {
            bail!("WSL request exceeds 4 MiB");
        }
        let job = async {
            let write = async move {
                input.write_all(&bytes).await?;
                input.shutdown().await?;
                drop(input);
                Ok::<_, anyhow::Error>(())
            };
            let read = async {
                let mut bytes = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = output.read(&mut chunk).await?;
                    if n == 0 {
                        break;
                    }
                    if bytes.len() + n > 24 * 1024 * 1024 {
                        bail!("WSL response exceeds 24 MiB");
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                Ok::<_, anyhow::Error>(bytes)
            };
            let (_, bytes) = tokio::try_join!(write, read)?;
            let status = child.wait().await?;
            let value: Value = serde_json::from_slice(&bytes).context(
                "invalid WSL bridge response; verify selected distro, user and bridge installation",
            )?;
            if !status.success() || value["ok"] != true {
                bail!(
                    "WSL bridge: {}",
                    value["error"].as_str().unwrap_or("request failed")
                );
            }
            Ok::<_, anyhow::Error>(value["result"].clone())
        };
        tokio::time::timeout(Duration::from_secs(130), job)
            .await
            .context("WSL bridge request timed out")?
    }
    fn spawn_spec(&self, mut config: Value) -> Result<LaunchSpec> {
        config["roots"] = json!(self.config.roots);
        let lease = WslLease {
            bridge: self.clone(),
            id: uuid::Uuid::new_v4().to_string(),
            renewal: Mutex::new(None),
        };
        let mut args = self.prefix.clone();
        args.extend([
            "run".into(),
            "--lease".into(),
            lease.id.clone(),
            "--config".into(),
            URL_SAFE.encode(serde_json::to_vec(&config)?),
        ]);
        Ok(LaunchSpec {
            program: self.launcher.clone(),
            args,
            lease,
        })
    }
}

pub fn configured_targets() -> Result<Vec<Value>> {
    Ok(distributions()?.into_iter().map(|(distribution,config)|json!({"execution_target":{"kind":"wsl","distribution":distribution},"user":config.user,"roots":config.roots})).collect())
}

pub async fn validate_workspace(target: &ExecutionTarget, path: &str) -> Result<String> {
    let ExecutionTarget::Wsl { distribution } = target else {
        bail!("WSL target required");
    };
    validate_linux_path(path)?;
    let value = Bridge::load(distribution)?
        .request(json!({"operation":"validate","workspace":path}))
        .await?;
    let root = value["workspace"]
        .as_str()
        .context("WSL bridge omitted workspace")?;
    validate_linux_path(root)?;
    Ok(root.into())
}

pub async fn request(
    target: &ExecutionTarget,
    workspace: &str,
    operation: &str,
    mut payload: Value,
) -> Result<Value> {
    let ExecutionTarget::Wsl { distribution } = target else {
        bail!("WSL target required");
    };
    validate_linux_path(workspace)?;
    payload["workspace"] = json!(workspace);
    payload["operation"] = json!(operation);
    Bridge::load(distribution)?.request(payload).await
}

pub async fn discover_target(target: &ExecutionTarget) -> Result<Vec<Value>> {
    let ExecutionTarget::Wsl { distribution } = target else {
        return Ok(crate::providers::discover().await);
    };
    let response = Bridge::load(distribution)?
        .request(json!({"operation":"discover"}))
        .await?;
    response["providers"]
        .as_array()
        .cloned()
        .context("WSL bridge omitted provider capabilities")
}

pub fn map_mcp_config(target: &ExecutionTarget, config: Option<Value>) -> Result<Option<Value>> {
    let ExecutionTarget::Wsl { distribution } = target else {
        return Ok(config);
    };
    let Some(mut config) = config else {
        return Ok(None);
    };
    let Some(command) = Bridge::load(distribution)?.config.computer_tools else {
        tracing::warn!("WSL computer tools are not configured; no Windows executable will be injected into Linux");
        return Ok(None);
    };
    config["command"] = json!(command);
    Ok(Some(config))
}

pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub lease: WslLease,
}

pub fn provider_spec(
    target: &ExecutionTarget,
    workspace: &str,
    provider: &str,
) -> Result<LaunchSpec> {
    let ExecutionTarget::Wsl { distribution } = target else {
        bail!("WSL target required");
    };
    validate_linux_path(workspace)?;
    let bridge = Bridge::load(distribution)?;
    let argv = match provider {
        "codex" => vec!["codex".into(), "app-server".into()],
        "kiro" => vec!["kiro-cli".into(), "acp".into(), "--agent-engine=v3".into()],
        "claude" => vec![
            "node".into(),
            bridge.config.claude_sidecar.clone().context(
                "Claude sidecar must be installed and configured inside this WSL distribution",
            )?,
        ],
        _ => bail!("unsupported WSL provider"),
    };
    bridge.spawn_spec(json!({"workspace":workspace,"argv":argv,"terminal":false}))
}

pub fn terminal_spec(target: &ExecutionTarget, workspace: &str) -> Result<LaunchSpec> {
    let ExecutionTarget::Wsl { distribution } = target else {
        bail!("WSL target required");
    };
    validate_linux_path(workspace)?;
    Bridge::load(distribution)?
        .spawn_spec(json!({"workspace":workspace,"argv":["/bin/bash","-l"],"terminal":true}))
}

pub struct WslLease {
    bridge: Bridge,
    id: String,
    renewal: Mutex<Option<JoinHandle<()>>>,
}
impl WslLease {
    pub fn start_renewal(&self) {
        let bridge = self.bridge.clone();
        let id = self.id.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let mut command = bridge.command("renew");
                command
                    .args(["--lease", &id])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if !matches!(tokio::time::timeout(Duration::from_secs(5),command.status()).await,Ok(Ok(status)) if status.success())
                {
                    tracing::warn!("WSL process lease renewal failed; Linux watchdog will stop abandoned execution");
                }
            }
        });
        if let Ok(mut renewal) = self.renewal.lock() {
            if let Some(old) = renewal.replace(task) {
                old.abort();
            }
        }
    }
    pub fn resize(&self, rows: u16, cols: u16) {
        let bridge = self.bridge.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            let mut command = bridge.command("resize");
            command
                .args([
                    "--lease",
                    &id,
                    "--rows",
                    &rows.to_string(),
                    "--cols",
                    &cols.to_string(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = tokio::time::timeout(Duration::from_secs(5), command.status()).await;
        });
    }
    pub fn request_cancel(&self) {
        if let Ok(mut renewal) = self.renewal.lock() {
            if let Some(task) = renewal.take() {
                task.abort();
            }
        }
        let bridge = self.bridge.clone();
        let id = self.id.clone();
        // Drop also runs during runtime shutdown; independent OS thread ensures
        // cancellation reaches Linux instead of only killing the wsl.exe relay.
        std::thread::spawn(move || {
            let mut command = std::process::Command::new(&bridge.launcher);
            let mut child = match command
                .args(&bridge.prefix)
                .args(["cancel", "--lease", &id])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(_) => return,
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        });
    }
}
impl Drop for WslLease {
    fn drop(&mut self) {
        self.request_cancel();
    }
}
