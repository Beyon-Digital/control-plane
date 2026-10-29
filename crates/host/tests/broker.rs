#![cfg(unix)]
use control_plane_host::{
    broker::{self, Account, Broker, Config},
    core::Core,
    ipc::{self, Endpoint},
    models::CreateSession,
    store::Store,
};
use serde_json::{json, Value};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

struct Worker {
    core: Arc<Core>,
    endpoint: Endpoint,
    token: String,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}
async fn worker(root: &Path, name: &str) -> Worker {
    let path = root.join(name);
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store = Store::open(&path.join("db.sqlite")).unwrap();
    let secret = store.issue_pairing("administrator").unwrap();
    let paired = store.pair(&secret, name).unwrap();
    let token = paired["token"].as_str().unwrap().to_owned();
    let core = Core::new(store, vec![path.canonicalize().unwrap()], name.into()).unwrap();
    let socket = path.join("http.sock");
    let endpoint = Endpoint::Unix {
        http_socket: socket.clone(),
        grpc_socket: socket.with_extension("grpc.sock"),
        uid: nix::unistd::geteuid().as_raw(),
    };
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let state = core.clone();
    let task = tokio::spawn(async move {
        ipc::serve_unix(state, &socket, nix::unistd::geteuid().as_raw(), signal)
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if endpoint.connect(true).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    Worker {
        core,
        endpoint,
        token,
        stop,
        task,
    }
}
fn registry(alice: &Worker, bob: &Worker) -> Arc<Broker> {
    Broker::new(Config {
        accounts: vec![
            Account {
                id: "alice".into(),
                name: "Alice".into(),
                endpoint: alice.endpoint.clone(),
            },
            Account {
                id: "bob".into(),
                name: "Bob".into(),
                endpoint: bob.endpoint.clone(),
            },
        ],
    })
    .unwrap()
}
async fn gateway(broker: Arc<Broker>) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let app = broker::router(broker);
    let task = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (format!("http://{address}"), task)
}
fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

#[tokio::test]
async fn two_os_workers_keep_tokens_files_and_sessions_isolated() {
    let tmp = tempfile::Builder::new()
        .prefix("cpb")
        .tempdir_in("/tmp")
        .unwrap();
    let alice = worker(tmp.path(), "a").await;
    let bob = worker(tmp.path(), "b").await;
    let (base, task) = gateway(registry(&alice, &bob)).await;
    let client = client();
    let body = CreateSession {
        provider: "codex".into(),
        workspace: tmp.path().join("a").to_string_lossy().into(),
        title: "Alice only".into(),
        trusted: false,
        request_id: "create-one".into(),
        execution_target: Default::default(),
    };
    let created = client
        .post(format!("{base}/accounts/alice/v1/sessions"))
        .header("x-device-token", &alice.token)
        .header("x-account-id", "bob")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(created.status().is_success());
    let alice_sessions: Vec<Value> = client
        .get(format!("{base}/accounts/alice/v1/sessions"))
        .header("x-device-token", &alice.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(alice_sessions.len(), 1);
    let wrong = client
        .get(format!("{base}/accounts/bob/v1/sessions"))
        .header("x-device-token", &alice.token)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let bob_sessions: Vec<Value> = client
        .get(format!("{base}/accounts/bob/v1/sessions"))
        .header("x-device-token", &bob.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(bob_sessions.is_empty());
    let body = json!({"provider":"codex","workspace":tmp.path().join("a"),"title":"No cross-account filesystem","trusted":false,"request_id":"cross-root"});
    let denied = client
        .post(format!("{base}/accounts/bob/v1/sessions"))
        .header("x-device-token", &bob.token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(!denied.status().is_success());
    task.abort();
}

#[tokio::test]
async fn pairing_is_bound_to_selected_worker() {
    let tmp = tempfile::Builder::new()
        .prefix("cpb")
        .tempdir_in("/tmp")
        .unwrap();
    let alice = worker(tmp.path(), "a").await;
    let bob = worker(tmp.path(), "b").await;
    let (base, task) = gateway(registry(&alice, &bob)).await;
    let client = client();
    let secret = bob.core.store.issue_pairing("operator").unwrap();
    let body = json!({"secret":secret,"device_name":"Bob phone"});
    assert!(!client
        .post(format!("{base}/accounts/alice/v1/pair"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let response = client
        .post(format!("{base}/accounts/bob/v1/pair"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let token: Value = response.json().await.unwrap();
    assert!(bob
        .core
        .store
        .authenticate(token["token"].as_str().unwrap())
        .is_ok());
    assert!(alice
        .core
        .store
        .authenticate(token["token"].as_str().unwrap())
        .is_err());
    task.abort();
}

#[tokio::test]
async fn websocket_tunnel_preserves_replay_and_revocation() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let tmp = tempfile::Builder::new()
        .prefix("cpb")
        .tempdir_in("/tmp")
        .unwrap();
    let alice = worker(tmp.path(), "a").await;
    let bob = worker(tmp.path(), "b").await;
    let device = alice.core.store.authenticate(&alice.token).unwrap();
    let session = alice
        .core
        .create(
            &device,
            CreateSession {
                provider: "codex".into(),
                workspace: tmp.path().join("a").to_string_lossy().into(),
                title: "Tunnel".into(),
                trusted: false,
                request_id: "create".into(),
                execution_target: Default::default(),
            },
        )
        .await
        .unwrap();
    let id = session["id"].as_str().unwrap();
    let (base, task) = gateway(registry(&alice, &bob)).await;
    let mut request = format!(
        "{}/accounts/alice/v1/events?session_id={id}&after=0",
        base.replacen("http:", "ws:", 1)
    )
    .into_client_request()
    .unwrap();
    request
        .headers_mut()
        .insert("x-device-token", alice.token.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(first["kind"], "state");
    alice.core.store.revoke("local-admin", &device.id).unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap();
    assert!(closed.is_none() || closed.unwrap().unwrap().is_close());
    assert!(bob.core.store.authenticate(&bob.token).is_ok());
    task.abort();
}

#[tokio::test]
async fn grpc_requires_account_and_worker_credentials() {
    use control_plane_host::grpc::pb::{
        self, session_service_client::SessionServiceClient,
        session_service_server::SessionServiceServer,
    };
    let tmp = tempfile::Builder::new()
        .prefix("cpb")
        .tempdir_in("/tmp")
        .unwrap();
    let alice = worker(tmp.path(), "a").await;
    let bob = worker(tmp.path(), "b").await;
    let service = broker::grpc::Service(registry(&alice, &bob));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SessionServiceServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let mut client = SessionServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let mut missing = tonic::Request::new(pb::Empty {});
    missing
        .metadata_mut()
        .insert("x-device-token", alice.token.parse().unwrap());
    assert_eq!(
        client.list(missing).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    for (account, valid) in [("alice", true), ("bob", false)] {
        let mut request = tonic::Request::new(pb::Empty {});
        request
            .metadata_mut()
            .insert("x-account-id", account.parse().unwrap());
        request
            .metadata_mut()
            .insert("x-device-token", alice.token.parse().unwrap());
        let result = client.list(request).await;
        if valid {
            assert!(result.is_ok());
        } else {
            assert_eq!(result.unwrap_err().code(), tonic::Code::Unauthenticated);
        }
    }
    task.abort();
}

#[tokio::test]
async fn wrong_worker_uid_is_never_forwarded() {
    let tmp = tempfile::Builder::new()
        .prefix("cpb")
        .tempdir_in("/tmp")
        .unwrap();
    let worker = worker(tmp.path(), "a").await;
    let mut endpoint = worker.endpoint.clone();
    if let Endpoint::Unix { uid, .. } = &mut endpoint {
        *uid = uid.wrapping_add(1);
    }
    let broker = Broker::new(Config {
        accounts: vec![Account {
            id: "wrong".into(),
            name: "Wrong user".into(),
            endpoint,
        }],
    })
    .unwrap();
    let (base, task) = gateway(broker).await;
    let response = client()
        .get(format!("{base}/accounts/wrong/v1/sessions"))
        .header("x-device-token", &worker.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    task.abort();
}

#[test]
fn privileged_registry_rejects_world_writable_configuration() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("broker.json");
    std::fs::write(&path, b"{\"accounts\":[]}").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(Broker::load(&path)
        .unwrap_err()
        .to_string()
        .contains("not writable"));
}
