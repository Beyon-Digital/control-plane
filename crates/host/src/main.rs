use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use control_plane_host::{
    core::Core,
    grpc::{
        self,
        pb::{
            host_service_server::HostServiceServer, session_service_server::SessionServiceServer,
            workspace_service_server::WorkspaceServiceServer,
        },
    },
    store::Store,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(
    version,
    about = "Private-network coding agent host. Runs as the current OS user."
)]
struct Cli {
    #[arg(long, env = "CONTROL_PLANE_DATA", default_value = ".control-plane")]
    data: PathBuf,
    #[arg(long, global = true, env = "CONTROL_PLANE_DESKTOP_HELPER")]
    desktop_helper: Option<PathBuf>,
    #[arg(long, global = true, env = "CONTROL_PLANE_VIDEO_HELPER")]
    video_helper: Option<PathBuf>,
    #[arg(long, global = true, env = "CONTROL_PLANE_URL")]
    computer_tools_url: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // Parsed once; keep the startup flags together.
enum Command {
    /// Start the HTTPS/WebSocket gateway and gRPC endpoint.
    Serve {
        #[arg(long, default_value = "127.0.0.1:9847")]
        listen: SocketAddr,
        #[arg(long, default_value = "127.0.0.1:9848")]
        grpc_listen: SocketAddr,
        #[arg(long, required = true)]
        workspace_root: Vec<PathBuf>,
        #[arg(long, default_value = "Control Plane")]
        name: String,
        #[arg(long)]
        tls_cert: Option<PathBuf>,
        #[arg(long)]
        tls_key: Option<PathBuf>,
        /// Require a client certificate signed by this CA on gRPC.
        #[arg(long)]
        client_ca: Option<PathBuf>,
        /// Serve only authenticated Unix IPC for a multi-user broker (no TCP listeners).
        #[arg(long, conflicts_with = "ipc_pipe")]
        ipc_socket: Option<PathBuf>,
        /// Local Windows pipe name, e.g. \\.\pipe\control-plane-alice.
        #[arg(long, conflicts_with = "ipc_socket")]
        ipc_pipe: Option<String>,
        #[arg(long, default_value = "0")]
        broker_uid: u32,
        #[arg(long, default_value = "S-1-5-18")]
        broker_sid: String,
    },
    /// Create a single-use enrollment secret, valid for five minutes.
    Enroll {
        #[arg(long, default_value = "administrator")]
        role: String,
    },
    /// List enrolled devices. Tokens are never returned.
    Devices,
    /// Resume a worker after a cancelled update maintenance window.
    Resume,
    /// Revoke a device locally; the running worker stops its runs within 250 ms.
    Revoke { id: String },
    /// Print this OS user's worker registration for an administrator's broker registry.
    WorkerInfo {
        #[arg(long)]
        account: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        ipc_socket: Option<PathBuf>,
        #[arg(long)]
        ipc_pipe: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "control_plane_host=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if let Some(path) = &cli.desktop_helper {
        std::env::set_var("CONTROL_PLANE_DESKTOP_HELPER", path);
    }
    if let Some(path) = &cli.video_helper {
        std::env::set_var("CONTROL_PLANE_VIDEO_HELPER", path);
    }
    if let Some(url) = &cli.computer_tools_url {
        std::env::set_var("CONTROL_PLANE_URL", url);
    }
    #[cfg(windows)]
    control_plane_host::ipc::windows::create_private_directory(&cli.data)?;
    #[cfg(not(windows))]
    std::fs::create_dir_all(&cli.data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli.data, std::fs::Permissions::from_mode(0o700))?;
    }
    let store = Store::open(&cli.data.join("host.sqlite3"))?;
    match cli.command {
        Command::Enroll { role } => {
            println!("{}", store.issue_pairing(&role)?);
        }
        Command::Resume => {
            control_plane_host::update_drain::resume(&cli.data)?;
            println!("Worker maintenance cleared. Start the worker normally.");
        }
        Command::Devices => println!("{}", serde_json::to_string_pretty(&store.devices()?)?),
        Command::Revoke { id } => {
            store.revoke("local-administrator", &id)?;
            println!("Device revoked");
        }
        Command::WorkerInfo {
            account,
            name,
            ipc_socket,
            ipc_pipe,
        } => {
            if !control_plane_host::broker::account_id(&account) {
                bail!("invalid account identifier");
            }
            #[cfg(unix)]
            let endpoint = {
                if ipc_pipe.is_some() {
                    bail!("named pipes require Windows");
                }
                let socket = ipc_socket.unwrap_or(cli.data.canonicalize()?.join("ipc/worker.sock"));
                control_plane_host::ipc::Endpoint::Unix {
                    grpc_socket: socket.with_extension("grpc.sock"),
                    http_socket: socket,
                    uid: nix::unistd::geteuid().as_raw(),
                }
            };
            #[cfg(windows)]
            let endpoint = {
                if ipc_socket.is_some() {
                    bail!("Unix sockets are not a Windows worker transport");
                }
                let pipe = ipc_pipe.unwrap_or_else(|| format!(r"\\.\pipe\control-plane-{account}"));
                control_plane_host::ipc::Endpoint::NamedPipe {
                    http_pipe: pipe.clone(),
                    grpc_pipe: format!("{pipe}-grpc"),
                    sid: control_plane_host::ipc::windows::current_sid()?,
                }
            };
            endpoint.validate()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&control_plane_host::broker::Account {
                    id: account.clone(),
                    name: name.unwrap_or(account),
                    endpoint
                })?
            );
        }
        Command::Serve {
            listen,
            grpc_listen,
            workspace_root,
            name,
            tls_cert,
            tls_key,
            client_ca,
            ipc_socket,
            ipc_pipe,
            broker_uid,
            broker_sid,
        } => {
            if control_plane_host::update_drain::pending(&cli.data)? {
                bail!(
                    "worker is in update maintenance; finish installation or run resume explicitly"
                );
            }
            let lock = control_plane_host::update_drain::worker_lock(&cli.data)?;
            fs2::FileExt::try_lock_exclusive(&lock)
                .context("another worker is already using this data directory")?;
            if tls_cert.is_some() != tls_key.is_some() {
                bail!("provide both --tls-cert and --tls-key");
            }
            if tls_cert.is_none() && (!listen.ip().is_loopback() || !grpc_listen.ip().is_loopback())
            {
                bail!("TLS is required for non-loopback listeners");
            }
            if client_ca.is_some() && tls_cert.is_none() {
                bail!("client CA requires TLS");
            }
            let roots = workspace_root
                .into_iter()
                .map(|r| r.canonicalize().context("workspace root does not exist"))
                .collect::<Result<Vec<_>>>()?;
            let core = Core::new(store, roots, name)?;
            let authority =
                control_plane_host::certificates::Authority::load_or_create(&cli.data.join("pki"))?;
            let mut client_roots = authority.ca_pem.as_bytes().to_vec();
            if let Some(path) = &client_ca {
                client_roots.extend_from_slice(&tokio::fs::read(path).await?);
            }
            core.identity
                .set(authority)
                .map_err(|_| anyhow::anyhow!("device authority already initialized"))?;
            control_plane_host::transfer::recover_staging(&core, true).await?;
            let maintenance_stop = tokio_util::sync::CancellationToken::new();
            let drain_stop = maintenance_stop.clone();
            let drain_core = Arc::downgrade(&core);
            let drain_data = cli.data.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
                loop {
                    tick.tick().await;
                    let Some(core) = drain_core.upgrade() else {
                        break;
                    };
                    if matches!(
                        control_plane_host::update_drain::pending(&drain_data),
                        Ok(true)
                    ) {
                        core.draining
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        stop_all(&core).await;
                        let deadline =
                            tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                        while !core.runs.lock().await.is_empty()
                            && tokio::time::Instant::now() < deadline
                        {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        drain_stop.cancel();
                        break;
                    }
                }
            });
            let maintenance = Arc::downgrade(&core);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
                loop {
                    tick.tick().await;
                    let Some(core) = maintenance.upgrade() else {
                        break;
                    };
                    let _guard = core.mutation.lock().await;
                    let actor = control_plane_host::models::Device {
                        id: "retention-service".into(),
                        name: "Scheduled retention".into(),
                        member_id: String::new(),
                        role: "administrator".into(),
                        revoked: false,
                    };
                    if let Err(error) = core.store.prune_retention(&actor) {
                        tracing::warn!(%error,"scheduled retention failed");
                    }
                    drop(_guard);
                    let _ = control_plane_host::transfer::recover_staging(&core, false).await;
                }
            });
            let _discovery = if cert_available_for_discovery(&tls_cert, listen) {
                control_plane_host::discovery::advertise(
                    &core.name,
                    listen.port(),
                    grpc_listen.port(),
                    "",
                )
                .ok()
            } else {
                None
            };
            let monitor = Arc::downgrade(&core);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
                loop {
                    tick.tick().await;
                    let Some(core) = monitor.upgrade() else { break };
                    core.revoke_inactive_devices().await;
                }
            });
            if ipc_socket.is_some() || ipc_pipe.is_some() {
                if tls_cert.is_some() || client_ca.is_some() {
                    bail!("worker IPC uses OS authentication; terminate TLS at the broker");
                }
                let stop = maintenance_stop.clone();
                let signal = stop.clone();
                let signal_core = core.clone();
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    stop_all(&signal_core).await;
                    signal.cancel();
                });
                if let Some(socket) = ipc_socket {
                    #[cfg(unix)]
                    {
                        control_plane_host::ipc::serve_unix(
                            core.clone(),
                            &socket,
                            broker_uid,
                            stop,
                        )
                        .await?;
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = (socket, broker_uid);
                        bail!("Unix IPC is unavailable on this OS");
                    }
                } else if let Some(pipe) = ipc_pipe {
                    #[cfg(windows)]
                    {
                        control_plane_host::ipc::serve_pipe(core.clone(), pipe, broker_sid, stop)
                            .await?;
                    }
                    #[cfg(not(windows))]
                    {
                        let _ = (pipe, broker_sid);
                        bail!("named-pipe IPC is only available on Windows");
                    }
                }
                stop_all(&core).await;
                return Ok(());
            }
            let cert = match tls_cert {
                Some(p) => Some(tokio::fs::read(p).await?),
                None => None,
            };
            let key = match tls_key {
                Some(p) => Some(tokio::fs::read(p).await?),
                None => None,
            };
            let service = grpc::Service(core.clone());
            let mut server = tonic::transport::Server::builder();
            if let (Some(cert), Some(key)) = (&cert, &key) {
                let mut config = tonic::transport::ServerTlsConfig::new()
                    .identity(tonic::transport::Identity::from_pem(cert, key));
                config = config
                    .client_ca_root(tonic::transport::Certificate::from_pem(&client_roots))
                    .client_auth_optional(true);
                server = server.tls_config(config)?;
            }
            let grpc = server
                .add_service(HostServiceServer::new(service.clone()))
                .add_service(SessionServiceServer::new(service.clone()))
                .add_service(WorkspaceServiceServer::new(service));
            tracing::info!(%listen,%grpc_listen,"host starting (one OS user per host process)");
            let shutdown = maintenance_stop.clone();
            let grpc_stop = shutdown.clone();
            let grpc_task = tokio::spawn(async move {
                grpc.serve_with_shutdown(grpc_listen, grpc_stop.cancelled())
                    .await
            });
            let signal_core = core.clone();
            let signal_stop = shutdown.clone();
            let handle = axum_server::Handle::new();
            let drain_handle = handle.clone();
            let drain_stop = shutdown.clone();
            tokio::spawn(async move {
                drain_stop.cancelled().await;
                drain_handle.graceful_shutdown(Some(std::time::Duration::from_secs(2)));
            });
            let signal_handle = handle.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                stop_all(&signal_core).await;
                signal_stop.cancel();
                signal_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
            });
            let app = control_plane_host::api::router(core.clone());
            let http_task = tokio::spawn(async move {
                if let (Some(cert), Some(key)) = (cert, key) {
                    let config = control_plane_host::secure_transport::config(
                        &cert,
                        &key,
                        Some(&client_roots),
                    )?;
                    axum_server::bind(listen)
                        .acceptor(control_plane_host::secure_transport::PeerAcceptor::new(
                            config, true,
                        ))
                        .handle(handle)
                        .serve(app.into_make_service())
                        .await?;
                } else {
                    tracing::warn!("loopback-only plaintext development transport");
                    axum_server::bind(listen)
                        .handle(handle)
                        .serve(app.into_make_service())
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            });
            tokio::select! {r=http_task=>{r??;},r=grpc_task=>{r??;}}
            stop_all(&core).await;
            shutdown.cancel();
        }
    }
    Ok(())
}
async fn stop_all(core: &Arc<Core>) {
    for run in core.runs.lock().await.values() {
        run.cancel.cancel();
    }
    core.terminals.stop_all();
    core.desktop.stop();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

fn cert_available_for_discovery(cert: &Option<PathBuf>, listen: SocketAddr) -> bool {
    cert.is_some() && !listen.ip().is_loopback()
}
