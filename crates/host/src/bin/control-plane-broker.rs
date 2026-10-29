use anyhow::{bail, Result};
use clap::Parser;
use control_plane_host::{
    broker::{self, Broker},
    grpc::pb::{
        host_service_server::HostServiceServer, session_service_server::SessionServiceServer,
        workspace_service_server::WorkspaceServiceServer,
    },
};
use std::{net::SocketAddr, path::PathBuf};
#[derive(Parser)]
#[command(
    version,
    about = "Routing-only multi-user machine broker. Agents always execute in separate OS-user workers."
)]
struct Cli {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, default_value = "127.0.0.1:9847")]
    listen: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:9848")]
    grpc_listen: SocketAddr,
    #[arg(long)]
    tls_cert: Option<PathBuf>,
    #[arg(long)]
    tls_key: Option<PathBuf>,
    #[arg(long)]
    client_ca: Option<PathBuf>,
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
    let broker = Broker::load(&cli.config)?;
    if cli.tls_cert.is_some() != cli.tls_key.is_some() {
        bail!("provide both TLS certificate and key");
    }
    if cli.tls_cert.is_none()
        && (!cli.listen.ip().is_loopback() || !cli.grpc_listen.ip().is_loopback())
    {
        bail!("TLS is required for remote broker listeners");
    }
    if cli.tls_cert.is_some() && cli.client_ca.is_none() {
        bail!("TLS broker requires --client-ca containing registered workers’ public device CAs");
    }
    let cert = match cli.tls_cert {
        Some(p) => Some(tokio::fs::read(p).await?),
        None => None,
    };
    let key = match cli.tls_key {
        Some(p) => Some(tokio::fs::read(p).await?),
        None => None,
    };
    let mut server = tonic::transport::Server::builder();
    let client_roots = match &cli.client_ca {
        Some(path) => Some(tokio::fs::read(path).await?),
        None => None,
    };
    if let (Some(cert), Some(key)) = (&cert, &key) {
        let mut tls = tonic::transport::ServerTlsConfig::new()
            .identity(tonic::transport::Identity::from_pem(cert, key));
        if let Some(ca) = &client_roots {
            tls = tls
                .client_ca_root(tonic::transport::Certificate::from_pem(ca))
                .client_auth_optional(true);
        }
        server = server.tls_config(tls)?;
    } else if cli.client_ca.is_some() {
        bail!("client CA requires TLS");
    }
    let service = broker::grpc::Service(broker.clone());
    let grpc = server
        .add_service(HostServiceServer::new(service.clone()))
        .add_service(SessionServiceServer::new(service.clone()))
        .add_service(WorkspaceServiceServer::new(service));
    let stop = tokio_util::sync::CancellationToken::new();
    let signal = stop.clone();
    let handle = axum_server::Handle::new();
    let signal_handle = handle.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
        signal_handle.graceful_shutdown(Some(std::time::Duration::from_secs(3)));
    });
    let grpc_stop = stop.clone();
    let grpc_task = tokio::spawn(async move {
        grpc.serve_with_shutdown(cli.grpc_listen, grpc_stop.cancelled_owned())
            .await
    });
    let http_task = tokio::spawn(async move {
        let app = broker::router(broker);
        if let (Some(cert), Some(key)) = (cert, key) {
            let tls =
                control_plane_host::secure_transport::config(&cert, &key, client_roots.as_deref())?;
            axum_server::bind(cli.listen)
                .acceptor(control_plane_host::secure_transport::PeerAcceptor::new(
                    tls,
                    client_roots.is_some(),
                ))
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        } else {
            axum_server::bind(cli.listen)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    tokio::select! {r=grpc_task=>{r??;},r=http_task=>{r??;}}
    stop.cancel();
    Ok(())
}
