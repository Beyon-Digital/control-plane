use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use control_plane_host::{
    api,
    core::Core,
    models::{CreateSession, Device},
    store::Store,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn setup() -> (tempfile::TempDir, Arc<Core>, String, Device) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(&root.path().join("db.sqlite")).unwrap();
    let secret = store.issue_pairing("administrator").unwrap();
    let pairing = store.pair(&secret, "Mac").unwrap();
    let token = pairing["token"].as_str().unwrap().to_owned();
    let device = store.authenticate(&token).unwrap();
    let core = Core::new(
        store,
        vec![root.path().canonicalize().unwrap()],
        "Test host".into(),
    )
    .unwrap();
    (root, core, token, device)
}
fn input(root: &std::path::Path) -> CreateSession {
    CreateSession {
        provider: "codex".into(),
        workspace: root.to_string_lossy().into(),
        title: "Implement task".into(),
        trusted: false,
        request_id: "create-one".into(),
        execution_target: Default::default(),
    }
}
async fn request(
    core: Arc<Core>,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = api::router(core)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-device-token", token)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn creation_is_idempotent_and_input_bound() {
    let (root, core, _, device) = setup();
    let first = core.create(&device, input(root.path())).await.unwrap();
    let second = core.create(&device, input(root.path())).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(core.store.list().unwrap().len(), 1);
    let mut changed = input(root.path());
    changed.title = "Different".into();
    assert!(core.create(&device, changed).await.is_err());
}
#[tokio::test]
async fn another_member_cannot_mutate_shared_session() {
    let (root, core, token, owner) = setup();
    let session = core.create(&owner, input(root.path())).await.unwrap();
    let id = session["id"].as_str().unwrap();
    let invitation = core
        .store
        .create_invitation(&owner, "Other member", "operator", 60)
        .unwrap();
    let paired = core
        .store
        .accept_invitation(invitation["secret"].as_str().unwrap(), "Other Mac")
        .unwrap();
    let other = core
        .store
        .authenticate(paired["token"].as_str().unwrap())
        .unwrap();
    assert!(core.session(&other, id, false).is_err());
    let (status, _) = request(
        core.clone(),
        &token,
        "POST",
        &format!("/v1/sessions/{id}/share"),
        json!({"shared":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(core.session(&other, id, false).is_ok());
    assert!(core.session(&other, id, true).is_err());
}
#[tokio::test]
async fn viewers_cannot_create_or_control_desktop() {
    let (root, core, _, _) = setup();
    let p = core
        .store
        .pair(&core.store.issue_pairing("viewer").unwrap(), "Reviewer")
        .unwrap();
    let token = p["token"].as_str().unwrap();
    let (status, _) = request(
        core.clone(),
        token,
        "POST",
        "/v1/sessions",
        serde_json::to_value(input(root.path())).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(core, token, "GET", "/v1/desktop/frame", json!(null)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn paired_phone_and_mac_manage_same_member_sessions() {
    let (root, core, _, mac) = setup();
    let s = core.create(&mac, input(root.path())).await.unwrap();
    let p = core
        .store
        .pair(&core.store.issue_pairing("operator").unwrap(), "My phone")
        .unwrap();
    let phone = core
        .store
        .authenticate(p["token"].as_str().unwrap())
        .unwrap();
    assert_ne!(mac.id, phone.id);
    assert_eq!(mac.member_id, phone.member_id);
    assert!(core
        .session(&phone, s["id"].as_str().unwrap(), true)
        .is_ok());
}
#[tokio::test]
async fn file_api_checks_revision_and_workspace() {
    let (root, core, token, d) = setup();
    let s = core.create(&d, input(root.path())).await.unwrap();
    let id = s["id"].as_str().unwrap();
    let (status, v) = request(
        core.clone(),
        &token,
        "POST",
        "/v1/workspace/write",
        json!({"session_id":id,"path":"example.txt","content":"one"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        core.clone(),
        &token,
        "POST",
        "/v1/workspace/write",
        json!({"session_id":id,"path":"example.txt","content":"two","expected_sha256":"stale"}),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    let (status, _) = request(
        core.clone(),
        &token,
        "POST",
        "/v1/workspace/write",
        json!({"session_id":id,"path":"example.txt","content":"two","expected_sha256":v["sha256"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        core,
        &token,
        "GET",
        &format!("/v1/workspace/read?session_id={id}&path=..%2Fsecret"),
        json!(null),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
}
#[tokio::test]
async fn restart_marks_running_sessions_uncertain_without_replay() {
    let (root, core, _, d) = setup();
    let v = core.create(&d, input(root.path())).await.unwrap();
    let mut s = core.store.get(v["id"].as_str().unwrap()).unwrap();
    s.state = "running".into();
    core.store.save(&s).unwrap();
    drop(core);
    let core = Core::new(
        Store::open(&root.path().join("db.sqlite")).unwrap(),
        vec![root.path().to_path_buf()],
        "Restart".into(),
    )
    .unwrap();
    assert_eq!(core.store.get(&s.id).unwrap().state, "uncertain");
    assert!(core.runs.lock().await.is_empty());
}
#[tokio::test]
async fn revoked_device_is_denied_existing_routes() {
    let (_, core, _, _) = setup();
    let p = core
        .store
        .pair(&core.store.issue_pairing("operator").unwrap(), "Phone")
        .unwrap();
    core.store
        .revoke("admin", p["device_id"].as_str().unwrap())
        .unwrap();
    let (status, _) = request(
        core,
        p["token"].as_str().unwrap(),
        "GET",
        "/v1/sessions",
        json!(null),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
#[tokio::test]
async fn grpc_auth_and_creation_share_gateway_state() {
    use control_plane_host::grpc::{
        self,
        pb::{
            session_service_client::SessionServiceClient,
            session_service_server::SessionServiceServer, CreateSessionRequest, Empty,
        },
    };
    let (root, core, token, _) = setup();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = tokio_util::sync::CancellationToken::new();
    let signal = stop.clone();
    let service = grpc::Service(core.clone());
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SessionServiceServer::new(service))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                signal.cancelled(),
            )
            .await
            .unwrap()
    });
    let mut client = SessionServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    assert_eq!(
        client.list(Empty {}).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    let mut request = tonic::Request::new(CreateSessionRequest {
        provider: "codex".into(),
        workspace: root.path().to_string_lossy().into(),
        title: "gRPC session".into(),
        trusted: false,
        request_id: "grpc-create".into(),
        execution_target: None,
    });
    request
        .metadata_mut()
        .insert("x-device-token", token.parse().unwrap());
    let result = client.create(request).await.unwrap().into_inner();
    let session: Value = serde_json::from_str(&result.json).unwrap();
    let (status, list) = request_http(core, &token, "GET", "/v1/sessions", json!(null)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["id"], session["id"]);
    stop.cancel();
    server.await.unwrap();
}
// Alias avoids shadowing the request helper with tonic request objects.
async fn request_http(
    core: Arc<Core>,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    request(core, token, method, path, body).await
}
#[tokio::test]
async fn websocket_replays_and_closes_after_revocation() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let (root, core, token, d) = setup();
    let session = core.create(&d, input(root.path())).await.unwrap();
    let id = session["id"].as_str().unwrap();
    core.emit(id, "message", json!({"text":"durable before connect"}))
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = api::router(core.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut upgrade = format!("ws://{address}/v1/events?session_id={id}&after=0")
        .into_client_request()
        .unwrap();
    upgrade
        .headers_mut()
        .insert("x-device-token", token.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(first["kind"], "state");
    let second = socket.next().await.unwrap().unwrap();
    let second: Value = serde_json::from_str(second.to_text().unwrap()).unwrap();
    assert_eq!(second["payload"]["text"], "durable before connect");
    core.store.revoke("local-admin", &d.id).unwrap();
    let close = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap();
    assert!(close.is_none() || close.unwrap().unwrap().is_close());
    server.abort();
}

#[tokio::test]
async fn local_revocation_watchdog_cancels_existing_run() {
    let (_root, core, _token, device) = setup();
    let cancel = tokio_util::sync::CancellationToken::new();
    let (decisions, _) = tokio::sync::mpsc::channel(1);
    core.runs.lock().await.insert(
        "test-run".into(),
        control_plane_host::core::Run {
            cancel: cancel.clone(),
            decisions,
            device_id: device.id.clone(),
        },
    );
    core.store.revoke("local-admin", &device.id).unwrap();
    core.revoke_inactive_devices().await;
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn abandoned_agent_credentials_are_revoked_at_restart() {
    let (root, core, _token, _device) = setup();
    let secret = core.store.issue_pairing("operator").unwrap();
    let agent = core.store.pair(&secret, "Automation").unwrap();
    core.store
        .mark_automation(agent["device_id"].as_str().unwrap(), "session")
        .unwrap();
    drop(core);
    let restarted = Core::new(
        Store::open(&root.path().join("db.sqlite")).unwrap(),
        vec![root.path().to_owned()],
        "Restart".into(),
    )
    .unwrap();
    assert!(restarted
        .store
        .authenticate(agent["token"].as_str().unwrap())
        .is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_accepts_binary_input_and_survives_reconnection() {
    use base64::Engine;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let (root, core, token, device) = setup();
    let mut create = input(root.path());
    create.trusted = true;
    let session = core.create(&device, create).await.unwrap();
    let id = session["id"].as_str().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = api::router(core.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let connect = || {
        let mut request = format!("ws://{address}/v1/terminal?session_id={id}&after=0")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("x-device-token", token.parse().unwrap());
        request
    };
    let (mut socket, _) = tokio_tungstenite::connect_async(connect()).await.unwrap();
    let data = base64::engine::general_purpose::STANDARD
        .encode("printf 'control-%s\\n' 'plane-ws-test'\n");
    socket
        .send(Message::Binary(
            json!({"type":"input","data":data})
                .to_string()
                .into_bytes()
                .into(),
        ))
        .await
        .unwrap();
    let mut output = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !output.contains("control-plane-ws-test") {
            let message = socket.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            if let Some(encoded) = value["data"].as_str() {
                output.push_str(&String::from_utf8_lossy(
                    &base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .unwrap(),
                ));
            }
        }
    })
    .await
    .unwrap();
    socket.close(None).await.unwrap();
    let (mut reconnect, _) = tokio_tungstenite::connect_async(connect()).await.unwrap();
    let replay = tokio::time::timeout(std::time::Duration::from_secs(2), reconnect.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(replay.is_text());
    core.emergency_stop(&device).await.unwrap();
    server.abort();
}
