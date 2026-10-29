#![cfg(unix)]
use control_plane_host::{
    models::{CreateSession, ExecutionTarget},
    providers, workspace, wsl,
};
use serde_json::{json, Value};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Environment {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let old = values
            .iter()
            .map(|(key, value)| {
                let old = std::env::var_os(key);
                std::env::set_var(key, value);
                (*key, old)
            })
            .collect();
        Self(old)
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            if let Some(value) = value {
                std::env::set_var(key, value)
            } else {
                std::env::remove_var(key)
            }
        }
    }
}

#[tokio::test]
async fn explicit_distribution_routes_files_and_git_without_shell_or_native_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("project ' $(touch injected); first");
    let second = temp.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    fs::write(first.join("identity.txt"), "distribution A").unwrap();
    fs::write(second.join("identity.txt"), "distribution B").unwrap();
    let log = temp.path().join("argv.jsonl");
    let launcher = temp.path().join("mock-wsl");
    let script = format!(
        r#"#!/usr/bin/env python3
import base64,json,subprocess,sys,time
args=sys.argv[1:]
with open({log:?},'a') as output:output.write(json.dumps(args)+'\n')
assert args[0]=='--distribution' and args[2]=='--user' and args[4]=='--exec'
assert args[1] in ['Ubuntu A','Ubuntu B'] and args[3]=='fixture'
if args[7] in ['cancel','renew','resize']:sys.exit(0)
if args[7]=='run':
    config=json.loads(base64.urlsafe_b64decode(args[args.index('--config')+1]))
    assert config['argv']==['codex','app-server']
    assert config['workspace'] in config['roots']
    def emit(value):print(json.dumps(value),flush=True)
    for line in sys.stdin:
        request=json.loads(line)
        method=request.get('method')
        if method=='initialize':emit(dict(id=request['id'],result=dict()))
        elif method=='thread/start':
            assert request['params']['cwd']==config['workspace']
            emit(dict(id=request['id'],result=dict(thread=dict(id='wsl-fixture-session'))))
        elif method=='turn/start':
            emit(dict(id=request['id'],result=dict(turn=dict(id='turn-fixture'))))
            if request['params']['input'][0]['text']=='wait':time.sleep(300)
            emit(dict(method='item/agentMessage/delta',params=dict(delta='from WSL fixture',itemId='item-fixture')))
            emit(dict(method='turn/completed',params=dict(turn=dict(id='turn-fixture',status='completed'))))
            sys.exit(0)
    sys.exit(1)
sys.exit(subprocess.call(args[5:]))
"#,
        log = log.to_string_lossy()
    );
    fs::write(&launcher, script).unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let bridge = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/wsl_bridge.py")
        .canonicalize()
        .unwrap();
    let config = json!({
        "Ubuntu A":{"user":"fixture","roots":[first],"bridge":bridge},
        "Ubuntu B":{"user":"fixture","roots":[second],"bridge":bridge}
    })
    .to_string();
    let _environment = Environment::set(&[
        ("CONTROL_PLANE_WSL_CONFIG", &config),
        ("CONTROL_PLANE_WSL_LAUNCHER", launcher.to_str().unwrap()),
    ]);
    let a = ExecutionTarget::Wsl {
        distribution: "Ubuntu A".into(),
    };
    let b = ExecutionTarget::Wsl {
        distribution: "Ubuntu B".into(),
    };
    assert_eq!(
        workspace::read_for(&a, first.to_str().unwrap(), "identity.txt")
            .await
            .unwrap()["content"],
        "distribution A"
    );
    assert_eq!(
        workspace::read_for(&b, second.to_str().unwrap(), "identity.txt")
            .await
            .unwrap()["content"],
        "distribution B"
    );
    assert!(
        workspace::read_for(&a, second.to_str().unwrap(), "identity.txt")
            .await
            .is_err()
    );
    assert!(
        workspace::read_for(&a, first.to_str().unwrap(), "../second/identity.txt")
            .await
            .is_err()
    );
    let missing = ExecutionTarget::Wsl {
        distribution: "Unconfigured".into(),
    };
    assert!(
        workspace::read_for(&missing, first.to_str().unwrap(), "identity.txt")
            .await
            .is_err()
    );
    assert!(providers::discover_target(&missing).await.is_err());
    assert!(
        workspace::read_for(&a, "C:\\Users\\fixture", "identity.txt")
            .await
            .is_err()
    );
    let current = workspace::read_for(&a, first.to_str().unwrap(), "identity.txt")
        .await
        .unwrap();
    assert!(workspace::write_for(
        &a,
        first.to_str().unwrap(),
        "identity.txt",
        "change",
        Some("stale")
    )
    .await
    .is_err());
    workspace::write_for(
        &a,
        first.to_str().unwrap(),
        "identity.txt",
        "change",
        current["sha256"].as_str(),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(first.join("identity.txt")).unwrap(),
        "change"
    );
    let native = ExecutionTarget::Native;
    assert_eq!(
        workspace::read_for(&native, second.to_str().unwrap(), "identity.txt")
            .await
            .unwrap()["content"],
        "distribution B"
    );
    let resolved = workspace::resolve_for(&a, first.to_str().unwrap(), "new name.txt")
        .await
        .unwrap();
    assert!(resolved.ends_with("/new name.txt"));
    let git = workspace::git_for(&a, first.to_str().unwrap(), &["status", "--porcelain"])
        .await
        .unwrap();
    assert_eq!(git["success"], false); // fixture is deliberately not a repository
    assert!(!temp.path().join("injected").exists());
    let calls = fs::read_to_string(&log).unwrap();
    for call in calls.lines() {
        let call: Value = serde_json::from_str(call).unwrap();
        assert_eq!(call[4], "--exec");
        assert_eq!(call[5], "python3");
        assert_eq!(call[6], json!(bridge));
    }
    let targets = wsl::configured_targets().unwrap();
    assert_eq!(targets.len(), 2);
    let (tx, mut events) = tokio::sync::mpsc::channel(32);
    let (_decisions, rx) = tokio::sync::mpsc::channel(4);
    providers::run_target(
        &a,
        "codex",
        &first,
        "fixture prompt ' $(touch injected)",
        None,
        false,
        tokio_util::sync::CancellationToken::new(),
        tx,
        rx,
        None,
    )
    .await
    .unwrap();
    let mut received = Vec::new();
    while let Some(event) = events.recv().await {
        received.push(event);
    }
    assert!(received
        .iter()
        .any(|event| event["kind"] == "message_delta"
            && event["payload"]["text"] == "from WSL fixture"));
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_run = cancel.clone();
    let target = a.clone();
    let root = first.clone();
    let (tx, mut events) = tokio::sync::mpsc::channel(32);
    let (_decisions, rx) = tokio::sync::mpsc::channel(4);
    let run = tokio::spawn(async move {
        providers::run_target(
            &target, "codex", &root, "wait", None, false, cancel_run, tx, rx, None,
        )
        .await
    });
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event["kind"], "provider_session");
    cancel.cancel();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("cancelled"));
    let spec = wsl::terminal_spec(&a, first.to_str().unwrap()).unwrap();
    assert_eq!(spec.program, launcher.to_string_lossy());
    assert_eq!(spec.args[1], "Ubuntu A");
    drop(spec);
}

#[test]
fn old_session_requests_default_to_native_and_bad_targets_fail() {
    let request: CreateSession = serde_json::from_value(
        json!({"provider":"codex","workspace":"/project","title":"t","request_id":"r"}),
    )
    .unwrap();
    assert_eq!(request.execution_target, ExecutionTarget::Native);
    assert!(serde_json::from_value::<ExecutionTarget>(
        json!({"kind":"native","distribution":"Ubuntu"})
    )
    .is_err());
    for path in [
        "C:\\project",
        "\\\\wsl$\\Ubuntu\\home",
        "/home/a/../b",
        "/mnt/c/project",
        "/",
    ] {
        assert!(wsl::validate_linux_path(path).is_err());
    }
}
