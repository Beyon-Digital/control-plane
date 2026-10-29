use crate::certificates::{peer_from_der, TlsPeer};
use anyhow::{Context, Result};
use axum::http::Request;
use axum_server::{
    accept::Accept,
    tls_rustls::{RustlsAcceptor, RustlsConfig},
};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
};
use tokio::net::TcpStream;
use tower::Service;
#[derive(Clone)]
pub struct IpcService<S>(pub S);
#[derive(Clone)]
pub struct IpcLayer;
impl<S> tower::Layer<S> for IpcLayer {
    type Service = IpcService<S>;
    fn layer(&self, service: S) -> Self::Service {
        IpcService(service)
    }
}
impl<S, B> Service<Request<B>> for IpcService<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;
    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }
    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        request
            .extensions_mut()
            .insert(crate::certificates::TrustedWorkerIPC);
        self.0.call(request)
    }
}

pub fn config(cert: &[u8], key: &[u8], client_ca: Option<&[u8]>) -> Result<RustlsConfig> {
    let chain = rustls_pemfile::certs(&mut io::Cursor::new(cert))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut io::Cursor::new(key))?
        .context("TLS private key missing")?;
    let builder = rustls::ServerConfig::builder();
    let mut config = if let Some(ca) = client_ca {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut io::Cursor::new(ca)) {
            roots.add(cert?)?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .allow_unauthenticated()
            .build()?;
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)?
    } else {
        builder.with_no_client_auth().with_single_cert(chain, key)?
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(RustlsConfig::from_config(Arc::new(config)))
}
#[derive(Clone)]
pub struct PeerAcceptor {
    inner: RustlsAcceptor,
    capable: bool,
}
impl PeerAcceptor {
    pub fn new(config: RustlsConfig, capable: bool) -> Self {
        Self {
            inner: RustlsAcceptor::new(config),
            capable,
        }
    }
}
#[derive(Clone)]
pub struct PeerService<S> {
    inner: S,
    peer: TlsPeer,
}
impl<S, B> Service<Request<B>> for PeerService<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;
    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        request.extensions_mut().insert(self.peer.clone());
        self.inner.call(request)
    }
}
impl<S> Accept<TcpStream, S> for PeerAcceptor
where
    S: Send + 'static,
{
    type Stream = tokio_rustls::server::TlsStream<TcpStream>;
    type Service = PeerService<S>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;
    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let accept = self.inner.accept(stream, service);
        let capable = self.capable;
        Box::pin(async move {
            let (stream, service) = accept.await?;
            let certificate = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
                .map(|cert| peer_from_der(cert.as_ref()))
                .transpose()
                .map_err(io::Error::other)?;
            Ok((
                stream,
                PeerService {
                    inner: service,
                    peer: TlsPeer {
                        certificate,
                        client_auth_capable: capable,
                    },
                },
            ))
        })
    }
}
