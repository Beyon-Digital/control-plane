//! A routing-only machine broker. Every account has an independently supervised
//! OS-user worker. The broker never launches an agent or stores provider secrets.
use crate::ipc::Endpoint;
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashMap, path::Path as FsPath, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

pub mod grpc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub endpoint: Endpoint,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub accounts: Vec<Account>,
}
#[derive(Debug)]
pub struct Broker {
    accounts: HashMap<String, Account>,
    connections: Arc<Semaphore>,
}
pub fn account_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
impl Broker {
    pub fn new(config: Config) -> Result<Arc<Self>> {
        if config.accounts.is_empty() || config.accounts.len() > 1000 {
            bail!("broker requires 1–1000 registered accounts");
        }
        let mut accounts = HashMap::new();
        let mut endpoints = std::collections::HashSet::new();
        for account in config.accounts {
            if !account_id(&account.id)
                || account.name.trim().is_empty()
                || account.name.len() > 256
            {
                bail!("invalid broker account");
            }
            account.endpoint.validate()?;
            if !endpoints.insert(serde_json::to_string(&account.endpoint)?)
                || accounts.insert(account.id.clone(), account).is_some()
            {
                bail!("duplicate broker account or worker endpoint");
            }
        }
        Ok(Arc::new(Self {
            accounts,
            connections: Arc::new(Semaphore::new(256)),
        }))
    }
    pub fn load(path: &FsPath) -> Result<Arc<Self>> {
        use std::io::Read;
        #[cfg(windows)]
        {
            crate::ipc::windows::validate_registry_path(path)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(nix::libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000);
        }
        let mut file = options.open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 1024 * 1024 {
            bail!("broker registry must be a regular file below 1 MiB");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if ![0, nix::unistd::geteuid().as_raw()].contains(&meta.uid())
                || meta.mode() & 0o022 != 0
            {
                bail!("broker registry must be owned by root or the broker user and not writable by other users");
            }
            let canonical = path.canonicalize()?;
            for parent in canonical.parent().into_iter().flat_map(FsPath::ancestors) {
                let directory = std::fs::metadata(parent)?;
                let trusted_owner = [0, nix::unistd::geteuid().as_raw()].contains(&directory.uid());
                let sticky_root = directory.uid() == 0 && directory.mode() & 0o1000 != 0;
                if !trusted_owner || (directory.mode() & 0o022 != 0 && !sticky_root) {
                    bail!("broker registry parent directory can be replaced by another OS user");
                }
            }
        }
        #[cfg(windows)]
        {
            crate::ipc::windows::validate_registry_handle(&file, path)?;
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            bail!("broker registry exceeds 1 MiB");
        }
        Self::new(serde_json::from_slice(&bytes).context("invalid broker registry JSON")?)
    }
    pub fn account(&self, id: &str) -> Result<&Account> {
        if !account_id(id) {
            bail!("unknown account");
        }
        self.accounts
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("unknown account"))
    }
    pub async fn channel(&self, id: &str) -> Result<tonic::transport::Channel> {
        let endpoint = self.account(id)?.endpoint.clone();
        Ok(
            tonic::transport::Endpoint::from_static("http://worker.internal")
                .connect_timeout(Duration::from_secs(5))
                .connect_with_connector(tower::service_fn(move |_| {
                    let endpoint = endpoint.clone();
                    async move {
                        endpoint
                            .connect(true)
                            .await
                            .map(TokioIo::new)
                            .map_err(|e| std::io::Error::other(e.to_string()))
                    }
                }))
                .await?,
        )
    }
}

pub fn router(broker: Arc<Broker>) -> Router {
    Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"ok","kind":"broker","protocol_version":1})) }),
        )
        .route("/accounts/{account}/{*path}", any(proxy))
        .with_state(broker)
}
async fn proxy(
    State(broker): State<Arc<Broker>>,
    Path((account, path)): Path<(String, String)>,
    mut request: Request<Body>,
) -> Response {
    if !path.starts_with("v1/") && path != "healthz" && path != ".well-known/control-plane-identity"
    {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"route not found"})),
        )
            .into_response();
    }
    let endpoint = match broker.account(&account) {
        Ok(a) => a.endpoint.clone(),
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"unknown account"})),
            )
                .into_response()
        }
    };
    let Ok(permit) = broker.connections.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"broker connection limit reached"})),
        )
            .into_response();
    };
    // Use the original encoded URI, not Path's percent-decoded value. Never let
    // an encoded slash or '?' change which upstream route receives credentials.
    let prefix = format!("/accounts/{account}");
    let Some(upstream_path) = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .and_then(|p| p.strip_prefix(&prefix))
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(uri) = upstream_path.parse() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *request.uri_mut() = uri;
    *request.version_mut() = axum::http::Version::HTTP_11;
    request.headers_mut().remove("x-account-id");
    request.headers_mut().remove("proxy-authorization");
    request.headers_mut().remove("proxy-connection");
    let peer = request
        .extensions()
        .get::<crate::certificates::TlsPeer>()
        .cloned();
    request
        .headers_mut()
        .remove(crate::certificates::PEER_HEADER);
    request
        .headers_mut()
        .remove(crate::certificates::CAPABLE_HEADER);
    if let Some(peer) = peer {
        if peer.client_auth_capable {
            request.headers_mut().insert(
                crate::certificates::CAPABLE_HEADER,
                axum::http::HeaderValue::from_static("1"),
            );
        }
        if let Some(cert) = peer.certificate {
            if let Ok(value) =
                axum::http::HeaderValue::from_str(&serde_json::to_string(&cert).unwrap_or_default())
            {
                request
                    .headers_mut()
                    .insert(crate::certificates::PEER_HEADER, value);
            }
        }
    }
    request.headers_mut().insert(
        "host",
        axum::http::HeaderValue::from_static("worker.internal"),
    );
    let websocket = request
        .headers()
        .get("upgrade")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let downstream = websocket.then(|| hyper::upgrade::on(&mut request));
    let result = async {
        let stream = tokio::time::timeout(Duration::from_secs(5), endpoint.connect(false))
            .await
            .context("worker connection timed out")??;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.with_upgrades().await {
                tracing::debug!(%error,"worker HTTP connection closed");
            }
        });
        let mut response =
            tokio::time::timeout(Duration::from_secs(30), sender.send_request(request))
                .await
                .context("worker response timed out")??;
        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            let downstream = downstream.context("unexpected worker protocol upgrade")?;
            let upstream = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                let _permit = permit;
                if let (Ok(a), Ok(b)) = tokio::join!(downstream, upstream) {
                    let _ =
                        tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b))
                            .await;
                }
            });
            Ok::<_, anyhow::Error>(response.map(Body::new))
        } else {
            Ok(response.map(|body| {
                Body::new(LimitedBody {
                    inner: body,
                    _permit: permit,
                })
            }))
        }
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%account,%error,"broker worker request failed");
            (StatusCode::BAD_GATEWAY,Json(json!({"error":"account worker unavailable or its OS identity could not be verified"}))).into_response()
        }
    }
}

struct LimitedBody {
    inner: hyper::body::Incoming,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl http_body::Body for LimitedBody {
    type Data = hyper::body::Bytes;
    type Error = hyper::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.inner).poll_frame(cx)
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_duplicate_accounts_and_ambiguous_ids() {
        let a = Account {
            id: "alice".into(),
            name: "Alice".into(),
            endpoint: Endpoint::Unix {
                http_socket: "/tmp/a/h".into(),
                grpc_socket: "/tmp/a/g".into(),
                uid: 501,
            },
        };
        assert!(Broker::new(Config {
            accounts: vec![a.clone(), a]
        })
        .is_err());
        for id in ["../alice", "alice/bob", "alice?token=abc", ""] {
            assert!(!account_id(id));
        }
    }
}
