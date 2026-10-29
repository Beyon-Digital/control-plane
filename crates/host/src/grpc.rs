// tonic's public signatures require Status by value, including generated clients.
#![allow(clippy::result_large_err)]
use crate::{
    core::Core,
    models::{CreateSession, Device, ExecutionTarget as Target, Prompt},
};
use serde_json::{json, Value};
use std::{pin::Pin, sync::Arc};
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
pub mod pb {
    tonic::include_proto!("controlplane.v1");
}
use pb::*;
#[derive(Clone)]
pub struct Service(pub Arc<Core>);
pub fn certificate<T>(request: &Request<T>) -> Option<crate::certificates::PeerCertificate> {
    if let Some(info) = request
        .extensions()
        .get::<tonic::transport::server::TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>()
    {
        return info.peer_certs().and_then(|chain| {
            chain
                .first()
                .and_then(|cert| crate::certificates::peer_from_der(cert.as_ref()).ok())
        });
    }
    if request
        .extensions()
        .get::<crate::certificates::TrustedWorkerIPC>()
        .is_some()
    {
        return request
            .metadata()
            .get(crate::certificates::PEER_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() < 1024)
            .and_then(|v| serde_json::from_str(v).ok());
    }
    None
}
fn err(e: impl std::fmt::Display) -> Status {
    let text = e.to_string();
    if text.contains("forbidden") {
        Status::permission_denied(text)
    } else if text.contains("unauthorized") {
        Status::unauthenticated(text)
    } else {
        Status::failed_precondition(text)
    }
}
fn doc(v: Value) -> Response<Document> {
    Response::new(Document {
        json: v.to_string(),
    })
}
impl Service {
    fn device<T>(&self, r: &Request<T>) -> Result<Device, Status> {
        self.device_with_bootstrap(r, false)
    }
    fn device_with_bootstrap<T>(&self, r: &Request<T>, bootstrap: bool) -> Result<Device, Status> {
        if self.0.draining.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Status::unavailable("worker is draining for installation"));
        }
        let token = r
            .metadata()
            .get("x-device-token")
            .and_then(|s| s.to_str().ok())
            .ok_or_else(|| Status::unauthenticated("device token required"))?;
        let device = self.0.store.authenticate(token).map_err(err)?;
        crate::certificates::authorize(&self.0.store, &device, certificate(r).as_ref())
            .map_err(err)?;
        let capable=r.extensions().get::<tonic::transport::server::TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>().is_some() || (r.extensions().get::<crate::certificates::TrustedWorkerIPC>().is_some() && r.metadata().get(crate::certificates::CAPABLE_HEADER).is_some_and(|v|v=="1"));
        if capable && !bootstrap && !self.0.store.requires_certificate(&device.id).map_err(err)? {
            return Err(Status::unauthenticated(
                "device certificate enrollment required before remote access",
            ));
        }
        Ok(device)
    }
}
#[tonic::async_trait]
impl host_service_server::HostService for Service {
    async fn call(&self, r: Request<ApiCallRequest>) -> Result<Response<Document>, Status> {
        use tower::ServiceExt;
        self.device_with_bootstrap(&r, r.get_ref().path == "v1/certificates/enroll")?;
        let token = r
            .metadata()
            .get("x-device-token")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let peer = certificate(&r);
        let capable=r.extensions().get::<tonic::transport::server::TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>().is_some()
            || (r.extensions().get::<crate::certificates::TrustedWorkerIPC>().is_some() && r.metadata().get(crate::certificates::CAPABLE_HEADER).is_some_and(|v|v=="1"));
        let input = r.into_inner();
        if !input.path.starts_with("v1/")
            || input.path.contains("..")
            || input.path.contains(['?', '#', '%'])
            || input.body_json.len() > 2 * 1024 * 1024
        {
            return Err(Status::invalid_argument("invalid API path or payload"));
        }
        let method = match input.method.as_str() {
            "GET" => axum::http::Method::GET,
            "POST" => axum::http::Method::POST,
            _ => return Err(Status::invalid_argument("unsupported API method")),
        };
        let query: std::collections::BTreeMap<String, String> =
            serde_json::from_str(if input.query_json.is_empty() {
                "{}"
            } else {
                &input.query_json
            })
            .map_err(err)?;
        let mut url =
            reqwest::Url::parse(&format!("http://worker.internal/{}", input.path)).map_err(err)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(&url.as_str()["http://worker.internal".len()..])
            .header("x-device-token", token)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(input.body_json))
            .map_err(err)?;
        request
            .extensions_mut()
            .insert(crate::certificates::TlsPeer {
                certificate: peer,
                client_auth_capable: capable,
            });
        let response = crate::api::router(self.0.clone())
            .oneshot(request)
            .await
            .map_err(err)?;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .map_err(err)?;
        if !status.is_success() {
            let text = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_owned))
                .unwrap_or_else(|| status.to_string());
            return Err(err(text));
        }
        Ok(Response::new(Document {
            json: String::from_utf8(bytes.to_vec()).map_err(err)?,
        }))
    }
    async fn get_host(&self, r: Request<Empty>) -> Result<Response<Document>, Status> {
        self.device(&r)?;
        Ok(doc(self.0.host().await.map_err(err)?))
    }
    async fn emergency_stop(&self, r: Request<Empty>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        Ok(doc(self.0.emergency_stop(&d).await.map_err(err)?))
    }
}
#[tonic::async_trait]
impl session_service_server::SessionService for Service {
    async fn list(&self, r: Request<Empty>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        Ok(doc(self.0.list(&d).map_err(err)?))
    }
    async fn create(&self, r: Request<CreateSessionRequest>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        let i = r.into_inner();
        let execution_target = match i.execution_target {
            None => Target::Native,
            Some(target) if target.kind.is_empty() || target.kind == "native" => {
                if !target.distribution.is_empty() {
                    return Err(Status::invalid_argument(
                        "native target cannot name a distribution",
                    ));
                }
                Target::Native
            }
            Some(target) if target.kind == "wsl" && !target.distribution.is_empty() => {
                Target::Wsl {
                    distribution: target.distribution,
                }
            }
            _ => return Err(Status::invalid_argument("invalid execution target")),
        };
        Ok(doc(self
            .0
            .create(
                &d,
                CreateSession {
                    provider: i.provider,
                    workspace: i.workspace,
                    title: i.title,
                    trusted: i.trusted,
                    request_id: i.request_id,
                    execution_target,
                },
            )
            .await
            .map_err(err)?))
    }
    async fn prompt(&self, r: Request<PromptRequest>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        let i = r.into_inner();
        Ok(doc(self
            .0
            .prompt(
                &d,
                &i.session_id,
                Prompt {
                    text: i.text,
                    request_id: i.request_id,
                },
            )
            .await
            .map_err(err)?))
    }
    async fn interrupt(&self, r: Request<SessionRef>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        Ok(doc(self
            .0
            .interrupt(&d, &r.into_inner().session_id)
            .await
            .map_err(err)?))
    }
    async fn archive(&self, r: Request<SessionRef>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        Ok(doc(self
            .0
            .archive(&d, &r.into_inner().session_id)
            .await
            .map_err(err)?))
    }
    async fn decide(&self, r: Request<DecisionRequest>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        let i = r.into_inner();
        Ok(doc(self
            .0
            .decide(
                &d,
                &i.session_id,
                json!({"request_id":i.request_id,"decision":i.decision}),
            )
            .await
            .map_err(err)?))
    }
    type SubscribeStream = Pin<Box<dyn Stream<Item = Result<Event, Status>> + Send>>;
    async fn subscribe(
        &self,
        r: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let d = self.device(&r)?;
        let token = r
            .metadata()
            .get("x-device-token")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let peer = certificate(&r);
        let i = r.into_inner();
        self.0.session(&d, &i.session_id, false).map_err(err)?;
        let core = self.0.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            let mut after = i.after;
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(150));
            loop {
                interval.tick().await;
                if tx.is_closed() {
                    break;
                }
                if core.draining.load(std::sync::atomic::Ordering::SeqCst)
                    || crate::certificates::authorize(&core.store, &d, peer.as_ref()).is_err()
                    || core.store.authenticate(&token).is_err()
                    || core.session(&d, &i.session_id, false).is_err()
                {
                    let _ = tx
                        .send(Err(Status::permission_denied("access revoked")))
                        .await;
                    break;
                }
                match core.store.events(&i.session_id, after) {
                    Ok(events) => {
                        for e in events {
                            after = e.sequence;
                            if tx
                                .send(Ok(Event {
                                    sequence: e.sequence,
                                    session_id: e.session_id,
                                    kind: e.kind,
                                    payload_json: e.payload.to_string(),
                                    created_at: e.created_at,
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(err(e))).await;
                        break;
                    }
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }
}
#[tonic::async_trait]
impl workspace_service_server::WorkspaceService for Service {
    async fn list(&self, r: Request<WorkspaceRequest>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        let i = r.into_inner();
        let s = self.0.session(&d, &i.session_id, false).map_err(err)?;
        self.0
            .workspace_for(&s.execution_target, &s.workspace)
            .await
            .map_err(err)?;
        Ok(doc(crate::workspace::list_for(
            &s.execution_target,
            &s.workspace,
            &i.path,
        )
        .await
        .map_err(err)?))
    }
    async fn read(&self, r: Request<WorkspaceRequest>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        let i = r.into_inner();
        let s = self.0.session(&d, &i.session_id, false).map_err(err)?;
        self.0
            .workspace_for(&s.execution_target, &s.workspace)
            .await
            .map_err(err)?;
        Ok(doc(crate::workspace::read_for(
            &s.execution_target,
            &s.workspace,
            &i.path,
        )
        .await
        .map_err(err)?))
    }
    async fn write(&self, r: Request<WriteRequest>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        let i = r.into_inner();
        let _guard = self.0.mutation.lock().await;
        let s = self.0.session(&d, &i.session_id, true).map_err(err)?;
        self.0
            .workspace_for(&s.execution_target, &s.workspace)
            .await
            .map_err(err)?;
        Ok(doc(crate::workspace::write_for(
            &s.execution_target,
            &s.workspace,
            &i.path,
            &i.content,
            Some(&i.expected_sha256),
        )
        .await
        .map_err(err)?))
    }
    async fn diff(&self, r: Request<SessionRef>) -> Result<Response<Document>, Status> {
        let d = self.device(&r)?;
        let i = r.into_inner();
        let s = self.0.session(&d, &i.session_id, false).map_err(err)?;
        self.0
            .workspace_for(&s.execution_target, &s.workspace)
            .await
            .map_err(err)?;
        Ok(doc(crate::workspace::git_for(
            &s.execution_target,
            &s.workspace,
            &["diff", "HEAD", "--"],
        )
        .await
        .map_err(err)?))
    }
}
