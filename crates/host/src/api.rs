use crate::{core::Core, models::*};
use anyhow::{bail, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
    },
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{interval, Duration};

#[derive(Debug)]
pub struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let text = self.0.to_string();
        let status = if text.contains("unauthorized") {
            StatusCode::UNAUTHORIZED
        } else if text.contains("forbidden") {
            StatusCode::FORBIDDEN
        } else if text.contains("not found") {
            StatusCode::NOT_FOUND
        } else if text.contains("already")
            || text.contains("changed")
            || text.contains("conflict")
            || text.contains("history_gap")
        {
            StatusCode::CONFLICT
        } else if text.contains("quota") {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({"error":text}))).into_response()
    }
}
type Output = Result<Json<Value>, ApiError>;

async fn auth(
    State(core): State<Arc<Core>>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if core.draining.load(std::sync::atomic::Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"worker is draining for installation"})),
        )
            .into_response();
    }
    let token = req
        .headers()
        .get("x-device-token")
        .and_then(|x| x.to_str().ok())
        .unwrap_or("");
    match core.store.authenticate(token) {
        Ok(d) => {
            let peer = crate::certificates::request_peer(req.extensions(), req.headers());
            if let Err(error) = crate::certificates::authorize(&core.store, &d, peer.as_ref()) {
                return ApiError(error).into_response();
            }
            if crate::certificates::client_auth_capable(req.extensions(), req.headers())
                && !core.store.requires_certificate(&d.id).unwrap_or(true)
                && req.uri().path() != "/v1/certificates/enroll"
            {
                return (StatusCode::UNAUTHORIZED,Json(json!({"error":"device certificate enrollment required before remote access"}))).into_response();
            }
            if let Some(peer) = peer {
                req.extensions_mut().insert(peer);
            }
            req.extensions_mut().insert(d);
            next.run(req).await
        }
        Err(e) => ApiError(e).into_response(),
    }
}

pub fn router(core: Arc<Core>) -> Router {
    let protected = Router::new()
        .route("/v1/host", get(host))
        .route("/v1/execution-targets", get(execution_targets))
        .route("/v1/providers/discover", post(discover_providers))
        .route("/v1/sessions", get(list).post(create))
        .route("/v1/sessions/{id}/events", get(events))
        .route("/v1/events", get(ws_events))
        .route("/v1/terminal", get(ws_terminal))
        .route("/v1/sessions/{id}/prompt", post(prompt))
        .route("/v1/sessions/{id}/interrupt", post(interrupt))
        .route("/v1/sessions/{id}/archive", post(archive))
        .route("/v1/sessions/{id}/decision", post(decide))
        .route("/v1/sessions/{id}/share", post(share))
        .route("/v1/workspace/list", get(files))
        .route("/v1/workspace/read", get(read))
        .route("/v1/workspace/write", post(write))
        .route("/v1/workspace/diff", get(diff))
        .route("/v1/workspace/git", post(git))
        .route("/v1/desktop/status", get(desktop_status))
        .route("/v1/desktop/frame", get(desktop_frame))
        .route("/v1/desktop/accessibility", get(desktop_accessibility))
        .route("/v1/desktop/lease", post(desktop_lease))
        .route("/v1/desktop/action", post(desktop_action))
        .route("/v1/desktop/display", post(desktop_display))
        .route("/v1/emergency-stop", post(stop))
        .route("/v1/devices", get(devices))
        .route("/v1/notifications/register", post(register_push))
        .route("/v1/devices/{id}/revoke", post(revoke))
        .route("/v1/enrollment", post(enrollment))
        .route("/v1/audit", get(audit))
        .merge(crate::video::router())
        .merge(crate::certificates::protected_router())
        .merge(crate::team::protected_router())
        .merge(crate::quotas::router())
        .merge(crate::history::router())
        .merge(crate::transfer::router())
        .route("/v1/me", get(identity))
        .route("/v1/notifications/unregister", post(unregister_push))
        .route_layer(middleware::from_fn_with_state(core.clone(), auth));
    Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"ok","protocol_version":1})) }),
        )
        .route("/v1/pair", post(pair))
        .merge(crate::certificates::public_router())
        .merge(crate::team::public_router())
        .merge(protected)
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(core)
}
#[derive(Deserialize)]
struct Pair {
    secret: String,
    device_name: String,
    csr_pem: Option<String>,
}
async fn pair(
    State(c): State<Arc<Core>>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    Json(v): Json<Pair>,
) -> Output {
    let capable = crate::certificates::client_auth_capable(&extensions, &headers);
    crate::certificates::validate_pairing_csr(&c, capable, v.csr_pem.as_deref())?;
    Ok(Json(crate::certificates::finish_pairing(
        &c,
        c.store.pair(&v.secret, &v.device_name)?,
        v.csr_pem.as_deref(),
    )?))
}
async fn host(State(c): State<Arc<Core>>) -> Output {
    Ok(Json(c.host().await?))
}
async fn execution_targets() -> Output {
    let mut targets = vec![json!({"execution_target":{"kind":"native"},"name":"Host OS"})];
    targets.extend(crate::wsl::configured_targets()?);
    Ok(Json(json!(targets)))
}
#[derive(Deserialize)]
struct TargetRequest {
    #[serde(default)]
    execution_target: ExecutionTarget,
}
async fn discover_providers(Json(input): Json<TargetRequest>) -> Output {
    Ok(Json(json!(
        crate::providers::discover_target(&input.execution_target).await?
    )))
}
async fn list(State(c): State<Arc<Core>>, Extension(d): Extension<Device>) -> Output {
    Ok(Json(c.list(&d)?))
}
async fn create(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<CreateSession>,
) -> Output {
    Ok(Json(c.create(&d, v).await?))
}
async fn prompt(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(v): Json<Prompt>,
) -> Output {
    Ok(Json(c.prompt(&d, &id, v).await?))
}
async fn interrupt(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> Output {
    Ok(Json(c.interrupt(&d, &id).await?))
}
async fn archive(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> Output {
    Ok(Json(c.archive(&d, &id).await?))
}
async fn decide(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(v): Json<Value>,
) -> Output {
    Ok(Json(c.decide(&d, &id, v).await?))
}
async fn share(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(v): Json<Value>,
) -> Output {
    let _guard = c.mutation.lock().await;
    let shared = v["shared"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("shared must be boolean"))?;
    c.store.set_legacy_sharing(&d, &id, shared)?;
    c.emit(&id, "sharing", json!({"shared":shared}))?;
    Ok(Json(json!(c.store.get(&id)?)))
}
#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: u64,
}
async fn events(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Query(q): Query<Cursor>,
) -> Output {
    c.session(&d, &id, false)?;
    Ok(Json(json!(c.store.events(&id, q.after)?)))
}
#[derive(Deserialize)]
struct StreamQuery {
    session_id: String,
    #[serde(default)]
    after: u64,
}
async fn ws_events(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<StreamQuery>,
    peer: Option<Extension<crate::certificates::PeerCertificate>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    c.session(&d, &q.session_id, false)?;
    Ok(ws.on_upgrade(move |socket| stream(c, d, q, socket, peer.map(|p| p.0))))
}
async fn stream(
    c: Arc<Core>,
    d: Device,
    q: StreamQuery,
    mut ws: WebSocket,
    peer: Option<crate::certificates::PeerCertificate>,
) {
    // Poll durable events rather than relying on a bounded broadcast queue. This
    // also recovers gaps when a consumer is slow or another writer commits events.
    let mut after = q.after;
    let mut tick = interval(Duration::from_millis(150));
    loop {
        tokio::select! {
            _=tick.tick()=>{
                let allowed=c.store.device_active(&d.id) && !c.draining.load(std::sync::atomic::Ordering::SeqCst);
                if !allowed || crate::certificates::authorize(&c.store,&d,peer.as_ref()).is_err() || c.session(&d,&q.session_id,false).is_err(){let _=ws.send(Message::Close(None)).await;break;}
                match c.store.events(&q.session_id,after){Ok(events)=>for e in events {after=e.sequence;if ws.send(Message::Text(serde_json::to_string(&e).unwrap().into())).await.is_err(){return;}},Err(error)=>{let _=ws.send(Message::Text(json!({"error":error.to_string()}).to_string().into())).await;break}}
            }
            incoming=ws.recv()=>match incoming {Some(Ok(Message::Ping(v)))=>{if ws.send(Message::Pong(v)).await.is_err(){break;}},Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,_=>{}}
        }
    }
}
#[derive(Deserialize)]
pub struct FileQuery {
    pub session_id: String,
    #[serde(default)]
    pub path: String,
}
async fn workspace_session(c: &Core, d: &Device, id: &str, write: bool) -> Result<Session> {
    let s = c.session(d, id, write)?;
    c.workspace_for(&s.execution_target, &s.workspace).await?;
    Ok(s)
}
async fn files(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<FileQuery>,
) -> Output {
    let s = workspace_session(&c, &d, &q.session_id, false).await?;
    Ok(Json(
        crate::workspace::list_for(&s.execution_target, &s.workspace, &q.path).await?,
    ))
}
async fn read(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<FileQuery>,
) -> Output {
    let s = workspace_session(&c, &d, &q.session_id, false).await?;
    Ok(Json(
        crate::workspace::read_for(&s.execution_target, &s.workspace, &q.path).await?,
    ))
}
#[derive(Deserialize)]
struct Write {
    session_id: String,
    path: String,
    content: String,
    expected_sha256: Option<String>,
}
async fn write(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<Write>,
) -> Output {
    let _guard = c.mutation.lock().await;
    let s = workspace_session(&c, &d, &v.session_id, true).await?;
    let result = crate::workspace::write_for(
        &s.execution_target,
        &s.workspace,
        &v.path,
        &v.content,
        v.expected_sha256.as_deref(),
    )
    .await?;
    c.emit(&v.session_id, "file_written", json!({"path":v.path}))?;
    Ok(Json(result))
}
async fn diff(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<FileQuery>,
) -> Output {
    let s = workspace_session(&c, &d, &q.session_id, false).await?;
    Ok(Json(
        crate::workspace::git_for(&s.execution_target, &s.workspace, &["diff", "HEAD", "--"])
            .await?,
    ))
}
#[derive(Deserialize)]
struct Git {
    session_id: String,
    operation: String,
    message: Option<String>,
    path: Option<String>,
    branch: Option<String>,
}
async fn git(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<Git>,
) -> Output {
    let _guard = c.mutation.lock().await;
    let s = workspace_session(&c, &d, &v.session_id, true).await?;
    let target = &s.execution_target;
    let dir = &s.workspace;
    let result = match v.operation.as_str() {
        "commit" => {
            let message = v
                .message
                .as_deref()
                .filter(|s| !s.trim().is_empty() && s.len() < 4096)
                .ok_or_else(|| anyhow::anyhow!("commit message required"))?;
            crate::workspace::git_for(target, dir, &["commit", "-m", message]).await?
        }
        "push" => crate::workspace::git_for(target, dir, &["push"]).await?,
        "stage" => {
            let path = v
                .path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("path required"))?;
            crate::workspace::resolve_for(target, dir, path).await?;
            crate::workspace::git_for(target, dir, &["add", "--", path]).await?
        }
        "worktree" => {
            let path = v
                .path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("path required"))?;
            let worktree = crate::workspace::resolve_for(target, dir, path).await?;
            let branch = v
                .branch
                .as_deref()
                .filter(|b| !b.starts_with('-') && !b.is_empty())
                .ok_or_else(|| anyhow::anyhow!("branch required"))?;
            let mut result = crate::workspace::git_for(
                target,
                dir,
                &["worktree", "add", "-b", branch, &worktree],
            )
            .await?;
            if result["success"] == true {
                result["worktree_path"] = json!(worktree);
            }
            result
        }
        _ => return Err(anyhow::anyhow!("unsupported git operation").into()),
    };
    c.emit(
        &v.session_id,
        "git",
        json!({"operation":v.operation,"success":result["success"]}),
    )?;
    Ok(Json(result))
}
async fn desktop_status(State(c): State<Arc<Core>>, Extension(d): Extension<Device>) -> Output {
    Core::authorize(&d, true)?;
    Ok(Json(c.desktop.status().await))
}
async fn desktop_frame(State(c): State<Arc<Core>>, Extension(d): Extension<Device>) -> Output {
    Core::authorize(&d, true)?;
    Ok(Json(c.desktop.frame().await?))
}
async fn desktop_accessibility(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
) -> Output {
    Core::authorize(&d, true)?;
    Ok(Json(c.desktop.accessibility().await?))
}
async fn desktop_lease(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<Value>,
) -> Output {
    Core::authorize(&d, true)?;
    let controller = v["controller_id"].as_str().unwrap_or("human");
    if controller.len() > 128 {
        return Err(anyhow::anyhow!("controller_id too long").into());
    }
    if v["takeover"].as_bool() == Some(true) {
        c.desktop.takeover();
    }
    Ok(Json(c.desktop.acquire(&format!("{}:{controller}", d.id))?))
}
async fn desktop_action(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<Value>,
) -> Output {
    Core::authorize(&d, true)?;
    // Reacquire as this authenticated device: another device cannot use a leaked lease.
    let controller = v["controller_id"].as_str().unwrap_or("human");
    let lease = c.desktop.acquire(&format!("{}:{controller}", d.id))?;
    let id = v["lease_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("lease_id required"))?;
    if lease["lease_id"] != id {
        return Err(anyhow::anyhow!("invalid lease").into());
    }
    Ok(Json(
        c.desktop
            .action(
                id,
                v["geometry_version"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("geometry_version required"))?,
                v["action"].clone(),
            )
            .await?,
    ))
}
async fn stop(State(c): State<Arc<Core>>, Extension(d): Extension<Device>) -> Output {
    Ok(Json(c.emergency_stop(&d).await?))
}
async fn register_push(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<Value>,
) -> Output {
    c.store.register_push(
        &d.id,
        v["apns_token"].as_str().unwrap_or(""),
        v["environment"].as_str().unwrap_or(""),
    )?;
    Ok(Json(json!({"registered":true})))
}
fn admin(d: &Device) -> Result<()> {
    if d.role != "administrator" {
        bail!("forbidden: administrator required");
    }
    Ok(())
}
async fn devices(State(c): State<Arc<Core>>, Extension(d): Extension<Device>) -> Output {
    admin(&d)?;
    Ok(Json(c.store.devices()?))
}
async fn audit(State(c): State<Arc<Core>>, Extension(d): Extension<Device>) -> Output {
    admin(&d)?;
    Ok(Json(c.store.audit()?))
}
async fn enrollment(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<Value>,
) -> Output {
    admin(&d)?;
    Ok(Json(
        json!({"secret":c.store.issue_pairing(v["role"].as_str().unwrap_or("operator"))?,"expires_in":300}),
    ))
}
async fn revoke(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> Output {
    admin(&d)?;
    c.store.revoke(&d.id, &id)?;
    for (sid, run) in c.runs.lock().await.iter() {
        if run.device_id == id {
            run.cancel.cancel();
            c.terminals.stop(sid);
        }
    }
    c.desktop.stop();
    Ok(Json(json!({"revoked":true})))
}

async fn ws_terminal(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<StreamQuery>,
    peer: Option<Extension<crate::certificates::PeerCertificate>>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let session = c.session(&d, &q.session_id, true)?;
    if !session.trusted || session.state == "archived" {
        return Err(anyhow::anyhow!("terminal requires a non-archived trusted session").into());
    }
    c.workspace_for(&session.execution_target, &session.workspace)
        .await?;
    let terminal =
        c.terminals
            .open_target(&session.id, &session.execution_target, &session.workspace)?;
    c.terminal_devices
        .lock()
        .await
        .insert(session.id.clone(), d.id.clone());
    c.emit(&session.id, "terminal_attached", json!({"device_id":d.id}))?;
    let peer = peer.map(|p| p.0);
    Ok(ws.on_upgrade(move|mut socket|async move{
        use base64::Engine;let mut after=q.after;let mut tick=interval(Duration::from_millis(80));
        loop{tokio::select!{
            _=tick.tick()=>{
                let allowed=c.store.device_active(&d.id) && !c.draining.load(std::sync::atomic::Ordering::SeqCst);
                if !allowed || crate::certificates::authorize(&c.store,&d,peer.as_ref()).is_err() || c.session(&d,&q.session_id,true).is_err(){break;}
                for event in terminal.events(after){after=event["sequence"].as_u64().unwrap_or(after);if socket.send(Message::Text(event.to_string().into())).await.is_err(){return;}}
            },
            incoming=socket.recv()=>match incoming{
                Some(Ok(message @ (Message::Text(_)|Message::Binary(_))))=>{
                    if c.draining.load(std::sync::atomic::Ordering::SeqCst) || crate::certificates::authorize(&c.store,&d,peer.as_ref()).is_err() || !c.store.device_active(&d.id){break;}
                    let bytes=match message{Message::Text(text)=>text.as_bytes().to_vec(),Message::Binary(data)=>data.to_vec(),_=>unreachable!()};
                    let result=(||->Result<()>{let v:Value=serde_json::from_slice(&bytes)?;match v["type"].as_str(){
                        Some("input")=>terminal.input(&base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap_or(""))?),
                        Some("resize")=>terminal.resize(v["cols"].as_u64().filter(|n|*n<=500).unwrap_or(100) as u16,v["rows"].as_u64().filter(|n|*n<=300).unwrap_or(30) as u16),
                        _=>bail!("unknown terminal message")
                    }})();
                    if let Err(e)=result{let _=socket.send(Message::Text(json!({"type":"error","message":e.to_string()}).to_string().into())).await;}
                },
                Some(Ok(Message::Ping(v)))=>{let _=socket.send(Message::Pong(v)).await;},
                Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,_=>{}
            }
        }}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;
    #[tokio::test]
    async fn private_routes_require_auth() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Core::new(
            crate::store::Store::open(&tmp.path().join("db")).unwrap(),
            vec![tmp.path().to_path_buf()],
            "test".into(),
        )
        .unwrap();
        let r = router(c)
            .oneshot(
                Request::builder()
                    .uri("/v1/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
}

async fn identity(Extension(d): Extension<Device>) -> Json<Value> {
    Json(json!({"device_id":d.id,"member_id":d.member_id,"role":d.role,"name":d.name}))
}
async fn unregister_push(State(c): State<Arc<Core>>, Extension(d): Extension<Device>) -> Output {
    c.store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM push_registrations WHERE device_id=?1", [&d.id])?;
    Ok(Json(json!({"unregistered":true})))
}

#[derive(Deserialize)]
struct DisplaySelection {
    display_id: String,
}
async fn desktop_display(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<DisplaySelection>,
) -> Output {
    Core::authorize(&d, true)?;
    if c.store.is_automation_device(&d.id)? {
        return Err(
            anyhow::anyhow!("forbidden: display selection requires a human controller").into(),
        );
    }
    Ok(Json(c.desktop.select_display(&v.display_id).await?))
}
