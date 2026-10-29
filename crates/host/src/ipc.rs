//! OS-authenticated transport for user workers. HTTP headers never substitute
//! for OS peer identity; normal device authentication also runs over this IPC.
#[cfg(unix)]
use anyhow::Context;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::path::Path;
use std::{path::PathBuf, pin::Pin};
use tokio::io::{AsyncRead, AsyncWrite};

pub trait IpcStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IpcStream for T {}
pub type Stream = Pin<Box<dyn IpcStream>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Endpoint {
    Unix {
        http_socket: PathBuf,
        grpc_socket: PathBuf,
        uid: u32,
    },
    NamedPipe {
        http_pipe: String,
        grpc_pipe: String,
        sid: String,
    },
}
impl Endpoint {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Unix {
                http_socket,
                grpc_socket,
                ..
            } => {
                if !http_socket.is_absolute()
                    || !grpc_socket.is_absolute()
                    || http_socket == grpc_socket
                    || [http_socket, grpc_socket].iter().any(|p| {
                        p.components()
                            .any(|c| matches!(c, std::path::Component::ParentDir))
                    })
                {
                    bail!("IPC sockets must be distinct absolute paths without traversal");
                }
            }
            Self::NamedPipe {
                http_pipe,
                grpc_pipe,
                sid,
            } => {
                for pipe in [http_pipe, grpc_pipe] {
                    if !pipe.starts_with(r"\\.\pipe\control-plane-")
                        || pipe.len() > 240
                        || pipe.contains('\0')
                        || pipe[r"\\.\pipe\".len()..].contains(['\\', '/'])
                    {
                        bail!("invalid local control-plane pipe name");
                    }
                }
                if http_pipe == grpc_pipe
                    || !sid.starts_with("S-1-")
                    || !sid
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b == b'S' || b == b'-')
                {
                    bail!("invalid worker SID or pipe names");
                }
            }
        }
        Ok(())
    }
    pub async fn connect(&self, grpc: bool) -> Result<Stream> {
        self.validate()?;
        match self {
            Self::Unix {
                http_socket,
                grpc_socket,
                uid,
            } => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{FileTypeExt, MetadataExt};
                    let path = if grpc { grpc_socket } else { http_socket };
                    let meta =
                        std::fs::symlink_metadata(path).context("worker socket unavailable")?;
                    if !meta.file_type().is_socket()
                        || meta.uid() != *uid
                        || meta.mode() & 0o077 != 0
                    {
                        bail!("worker socket ownership or permissions invalid");
                    }
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    if stream.peer_cred()?.uid() != *uid {
                        bail!("worker OS identity mismatch");
                    }
                    Ok(Box::pin(stream))
                }
                #[cfg(not(unix))]
                {
                    let _ = (http_socket, grpc_socket, uid);
                    bail!("Unix worker endpoints are unavailable on this OS")
                }
            }
            Self::NamedPipe {
                http_pipe,
                grpc_pipe,
                sid,
            } => {
                #[cfg(windows)]
                {
                    windows::connect(if grpc { grpc_pipe } else { http_pipe }, sid).await
                }
                #[cfg(not(windows))]
                {
                    let _ = (http_pipe, grpc_pipe, sid);
                    bail!("Windows named-pipe endpoints are unavailable on this OS")
                }
            }
        }
    }
}

#[cfg(unix)]
pub async fn unix_listener(path: &Path) -> Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let parent = path.parent().context("socket directory required")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let meta = std::fs::symlink_metadata(parent)?;
    let uid = nix::unistd::geteuid().as_raw();
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != uid
        || meta.mode() & 0o077 != 0
    {
        bail!("worker socket directory must be owned by this user with mode 0700");
    }
    if let Ok(existing) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if !existing.file_type().is_socket() || existing.uid() != uid {
            bail!("refusing to replace an unexpected IPC path");
        }
        match tokio::time::timeout(
            std::time::Duration::from_millis(250),
            tokio::net::UnixStream::connect(path),
        )
        .await
        {
            Ok(Err(error))
                if [
                    std::io::ErrorKind::ConnectionRefused,
                    std::io::ErrorKind::NotFound,
                ]
                .contains(&error.kind()) =>
            {
                std::fs::remove_file(path)?;
            }
            _ => bail!("worker socket is live or cannot be safely replaced"),
        }
    }
    let listener = tokio::net::UnixListener::bind(path).context(
        "bind worker socket (remove an obsolete socket only after its worker has stopped)",
    )?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(unix)]
pub struct PeerListener {
    pub listener: tokio::net::UnixListener,
    pub broker_uid: u32,
}
#[cfg(unix)]
impl axum::serve::Listener for PeerListener {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    let own = nix::unistd::geteuid().as_raw();
                    if stream
                        .peer_cred()
                        .is_ok_and(|p| p.uid() == self.broker_uid || p.uid() == own)
                    {
                        return (stream, address);
                    }
                    tracing::warn!("rejected worker IPC peer with unexpected OS identity");
                }
                Err(e) => {
                    tracing::warn!(error=%e,"worker IPC accept error");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// Both transports enforce OS identity before the application sees any bytes.
/// Device tokens remain mandatory inside the worker's ordinary API handlers.
pub async fn serve_pair<H, G>(
    core: std::sync::Arc<crate::core::Core>,
    http: H,
    grpc: G,
    stop: tokio_util::sync::CancellationToken,
) -> Result<()>
where
    H: axum::serve::Listener,
    H::Addr: std::fmt::Debug,
    G: axum::serve::Listener,
    G::Io: tonic::transport::server::Connected,
{
    use crate::grpc::pb::{
        host_service_server::HostServiceServer, session_service_server::SessionServiceServer,
        workspace_service_server::WorkspaceServiceServer,
    };
    let incoming = Box::pin(futures_util::stream::unfold(
        grpc,
        |mut listener| async move {
            let (stream, _) = listener.accept().await;
            Some((Ok::<_, std::io::Error>(stream), listener))
        },
    ));
    let service = crate::grpc::Service(core.clone());
    let grpc_stop = stop.clone();
    let http_stop = stop.clone();
    let grpc = tonic::transport::Server::builder()
        .layer(crate::secure_transport::IpcLayer)
        .add_service(HostServiceServer::new(service.clone()))
        .add_service(SessionServiceServer::new(service.clone()))
        .add_service(WorkspaceServiceServer::new(service))
        .serve_with_incoming_shutdown(incoming, grpc_stop.cancelled_owned());
    let http = async move {
        axum::serve(
            http,
            crate::api::router(core)
                .layer(axum::Extension(crate::certificates::TrustedWorkerIPC))
                .into_make_service(),
        )
        .with_graceful_shutdown(http_stop.cancelled_owned())
        .await
    };
    let result = tokio::select! {r=grpc=>r.map_err(anyhow::Error::from),r=http=>r.map_err(anyhow::Error::from),_=stop.cancelled()=>Ok(())};
    stop.cancel();
    result
}

#[cfg(unix)]
pub async fn serve_unix(
    core: std::sync::Arc<crate::core::Core>,
    socket: &Path,
    broker_uid: u32,
    stop: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let grpc_socket = socket.with_extension("grpc.sock");
    let http = PeerListener {
        listener: unix_listener(socket).await?,
        broker_uid,
    };
    let grpc = PeerListener {
        listener: unix_listener(&grpc_socket).await?,
        broker_uid,
    };
    let result = serve_pair(core, http, grpc, stop).await;
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(grpc_socket);
    result
}

#[cfg(windows)]
pub async fn serve_pipe(
    core: std::sync::Arc<crate::core::Core>,
    name: String,
    broker_sid: String,
    stop: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let http = windows::PipeListener::bind(name.clone(), broker_sid.clone())?;
    let grpc = windows::PipeListener::bind(format!("{name}-grpc"), broker_sid)?;
    serve_pair(core, http, grpc, stop).await
}

#[cfg(windows)]
pub mod windows;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoints_reject_remote_pipes_and_relative_sockets() {
        assert!(Endpoint::NamedPipe {
            http_pipe: r"\\remote\pipe\p".into(),
            grpc_pipe: r"\\remote\pipe\g".into(),
            sid: "S-1-5-18".into()
        }
        .validate()
        .is_err());
        assert!(Endpoint::Unix {
            http_socket: "relative".into(),
            grpc_socket: "other".into(),
            uid: 1
        }
        .validate()
        .is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn connector_checks_peer_and_socket_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("http.sock");
        let listener = unix_listener(&path).await.unwrap();
        let uid = nix::unistd::geteuid().as_raw();
        let endpoint = Endpoint::Unix {
            http_socket: path.clone(),
            grpc_socket: temp.path().join("grpc.sock"),
            uid,
        };
        assert!(endpoint.connect(false).await.is_ok());
        let wrong = Endpoint::Unix {
            http_socket: path,
            grpc_socket: temp.path().join("grpc.sock"),
            uid: uid.wrapping_add(1),
        };
        assert!(wrong.connect(false).await.is_err());
        drop(listener);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn live_sockets_are_preserved_and_obsolete_sockets_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/http.sock");
        let first = unix_listener(&path).await.unwrap();
        assert!(unix_listener(&path).await.is_err());
        drop(first);
        // kqueue may retain a just-closed listener until the next reactor turn.
        // Match a supervisor retry rather than unlinking a still-connectable path.
        let recovered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match unix_listener(&path).await {
                    Ok(listener) => break listener,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("closed socket should become safely replaceable");
        assert!(path.exists());
        drop(recovered);
    }
}
