//! Real CPV1 fixture through worker/broker WebSockets. Never captures a desktop.
#![cfg(unix)]
use control_plane_host::{
    api,
    broker::{self, Account, Broker, Config},
    core::Core,
    ipc::{self, Endpoint},
    store::Store,
    video::Packet,
};
use futures_util::StreamExt;
use std::{path::Path, time::Duration};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};
use tokio_util::sync::CancellationToken;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(base: &str, token: &str) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("{}/v1/desktop/video", base.replacen("http:", "ws:", 1))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-device-token", token.parse().unwrap());
    tokio_tungstenite::connect_async(request).await.map(|v| v.0)
}
async fn frame(socket: &mut Socket) {
    let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Binary(data) = message else {
        panic!("expected binary CPV1 frame: {message:?}")
    };
    let packet = Packet::read(&mut data.as_ref()).await.unwrap();
    assert_eq!(packet.header.width, 64);
    assert_eq!(packet.header.height, 64);
    assert!(packet.header.keyframe);
}
async fn closed(socket: &mut Socket) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match socket.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}
async fn helpers_reaped(path: &Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let running = std::fs::read_dir(path)
                .unwrap()
                .flatten()
                .filter(|p| p.path().extension().and_then(|v| v.to_str()) == Some("pid"))
                .any(|p| {
                    let pid: i32 = p
                        .path()
                        .file_stem()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .parse()
                        .unwrap();
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
                });
            if !running {
                break;
            };
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn real_h264_fixture_survives_worker_and_broker_transport_and_stops_on_revoke() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::Builder::new()
        .prefix("cpv")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apple/Tests/ControlPlaneKitTests/Fixtures/synthetic-h264.cpv")
        .canonicalize()
        .unwrap();
    let helper = temp.path().join("fixture.py");
    std::fs::write(&helper,format!("import os,sys,time,pathlib\npathlib.Path({dir:?},str(os.getpid())+'.pid').write_text('started')\npacket=pathlib.Path({fixture:?}).read_bytes()\nwhile True:\n sys.stdout.buffer.write(packet);sys.stdout.buffer.flush();time.sleep(0.05)\n",dir=temp.path().to_str().unwrap(),fixture=fixture.to_str().unwrap())).unwrap();
    // This binary contains one test; these process-global settings cannot race
    // other integration-test executables, each of which has its own environment.
    std::env::set_var("CONTROL_PLANE_VIDEO_HELPER", &helper);
    let store = Store::open(&temp.path().join("db.sqlite")).unwrap();
    let pair = |role: &str| {
        store
            .pair(&store.issue_pairing(role).unwrap(), "fixture client")
            .unwrap()
    };
    let first = pair("administrator");
    let second = pair("administrator");
    let viewer = pair("viewer");
    let first_token = first["token"].as_str().unwrap().to_owned();
    let second_token = second["token"].as_str().unwrap().to_owned();
    let core = Core::new(
        store,
        vec![temp.path().canonicalize().unwrap()],
        "Fixture host".into(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = api::router(core.clone());
    let worker = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    assert!(connect(&base, "invalid-token").await.is_err());
    assert!(connect(&base, viewer["token"].as_str().unwrap())
        .await
        .is_err());
    let mut a = connect(&base, &first_token).await.unwrap();
    frame(&mut a).await;
    let mut b = connect(&base, &second_token).await.unwrap();
    frame(&mut b).await;
    assert!(
        connect(&base, &second_token).await.is_err(),
        "third viewer must be refused before spawning a capture process"
    );
    core.store
        .revoke("test", first["device_id"].as_str().unwrap())
        .unwrap();
    closed(&mut a).await;
    core.desktop.stop();
    closed(&mut b).await;
    helpers_reaped(temp.path()).await;

    let socket = temp.path().join("http.sock");
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let state = core.clone();
    let uid = nix::unistd::geteuid().as_raw();
    let path = socket.clone();
    let ipc =
        tokio::spawn(async move { ipc::serve_unix(state, &path, uid, signal).await.unwrap() });
    let endpoint = Endpoint::Unix {
        http_socket: socket.clone(),
        grpc_socket: socket.with_extension("grpc.sock"),
        uid,
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if endpoint.connect(true).await.is_ok() {
                break;
            };
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    let broker = Broker::new(Config {
        accounts: vec![Account {
            id: "my-account".into(),
            name: "Fixture account".into(),
            endpoint,
        }],
    })
    .unwrap();
    let gateway = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_base = format!(
        "http://{}/accounts/my-account",
        gateway.local_addr().unwrap()
    );
    let app = broker::router(broker);
    let proxy = tokio::spawn(async move { axum::serve(gateway, app).await.unwrap() });
    let mut socket = connect(&gateway_base, &second_token).await.unwrap();
    frame(&mut socket).await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let response = reqwest::Client::new()
        .post(format!("{gateway_base}/v1/emergency-stop"))
        .header("x-device-token", &second_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    closed(&mut socket).await;
    helpers_reaped(temp.path()).await;
    stop.cancel();
    ipc.abort();
    proxy.abort();
    worker.abort();
    std::env::remove_var("CONTROL_PLANE_VIDEO_HELPER");
}
