//! gRPC proxy preserves the original per-user device token. Account metadata is
//! routing information, never authentication or OS identity.
#![allow(clippy::result_large_err)]
use super::Broker;
use crate::grpc::pb::*;
use std::{pin::Pin, sync::Arc};
use tonic::{Request, Response, Status};
#[derive(Clone)]
pub struct Service(pub Arc<Broker>);
impl Service {
    async fn forward<T>(
        &self,
        mut request: Request<T>,
    ) -> Result<
        (
            tokio::sync::OwnedSemaphorePermit,
            tonic::transport::Channel,
            Request<T>,
        ),
        Status,
    > {
        let account = request
            .metadata()
            .get("x-account-id")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Status::invalid_argument("x-account-id metadata is required"))?;
        if !request
            .metadata()
            .get("x-device-token")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Status::unauthenticated("valid device token required"));
        }
        let permit = self
            .0
            .connections
            .clone()
            .try_acquire_owned()
            .map_err(|_| Status::resource_exhausted("broker connection limit reached"))?;
        let channel = self.0.channel(account).await.map_err(|_| {
            Status::unavailable("account worker unavailable or OS identity mismatch")
        })?;
        let peer = crate::grpc::certificate(&request);
        let capable=request.extensions().get::<tonic::transport::server::TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>().is_some();
        request
            .metadata_mut()
            .remove(crate::certificates::PEER_HEADER);
        request
            .metadata_mut()
            .remove(crate::certificates::CAPABLE_HEADER);
        if capable {
            request
                .metadata_mut()
                .insert(crate::certificates::CAPABLE_HEADER, "1".parse().unwrap());
        }
        if let Some(peer) = peer {
            let value =
                serde_json::to_string(&peer).map_err(|_| Status::internal("peer serialization"))?;
            request.metadata_mut().insert(
                crate::certificates::PEER_HEADER,
                value
                    .parse()
                    .map_err(|_| Status::internal("peer serialization"))?,
            );
        }
        request.metadata_mut().remove("x-account-id");
        Ok((permit, channel, request))
    }
}
#[tonic::async_trait]
impl host_service_server::HostService for Service {
    async fn call(&self, r: Request<ApiCallRequest>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(r).await?;
        host_service_client::HostServiceClient::new(channel)
            .call(request)
            .await
    }
    async fn get_host(&self, request: Request<Empty>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        host_service_client::HostServiceClient::new(channel)
            .get_host(request)
            .await
    }
    async fn emergency_stop(&self, request: Request<Empty>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        host_service_client::HostServiceClient::new(channel)
            .emergency_stop(request)
            .await
    }
}
#[tonic::async_trait]
impl session_service_server::SessionService for Service {
    async fn list(&self, request: Request<Empty>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        session_service_client::SessionServiceClient::new(channel)
            .list(request)
            .await
    }
    async fn create(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        session_service_client::SessionServiceClient::new(channel)
            .create(request)
            .await
    }
    async fn prompt(&self, request: Request<PromptRequest>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        session_service_client::SessionServiceClient::new(channel)
            .prompt(request)
            .await
    }
    async fn interrupt(&self, request: Request<SessionRef>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        session_service_client::SessionServiceClient::new(channel)
            .interrupt(request)
            .await
    }
    async fn archive(&self, request: Request<SessionRef>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        session_service_client::SessionServiceClient::new(channel)
            .archive(request)
            .await
    }
    async fn decide(
        &self,
        request: Request<DecisionRequest>,
    ) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        session_service_client::SessionServiceClient::new(channel)
            .decide(request)
            .await
    }
    type SubscribeStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Status>> + Send>>;
    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        let response = session_service_client::SessionServiceClient::new(channel)
            .subscribe(request)
            .await?;
        Ok(Response::new(Box::pin(LimitedStream {
            inner: response.into_inner(),
            _permit,
        })))
    }
}
#[tonic::async_trait]
impl workspace_service_server::WorkspaceService for Service {
    async fn list(&self, request: Request<WorkspaceRequest>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        workspace_service_client::WorkspaceServiceClient::new(channel)
            .list(request)
            .await
    }
    async fn read(&self, request: Request<WorkspaceRequest>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        workspace_service_client::WorkspaceServiceClient::new(channel)
            .read(request)
            .await
    }
    async fn write(&self, request: Request<WriteRequest>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        workspace_service_client::WorkspaceServiceClient::new(channel)
            .write(request)
            .await
    }
    async fn diff(&self, request: Request<SessionRef>) -> Result<Response<Document>, Status> {
        let (_permit, channel, request) = self.forward(request).await?;
        workspace_service_client::WorkspaceServiceClient::new(channel)
            .diff(request)
            .await
    }
}
struct LimitedStream<S> {
    inner: S,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl<S> tokio_stream::Stream for LimitedStream<S>
where
    S: tokio_stream::Stream<Item = Result<Event, Status>> + Unpin,
{
    type Item = Result<Event, Status>;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn malformed_tokens_and_exhausted_capacity_do_not_open_worker_channels() {
        #[cfg(unix)]
        let endpoint = crate::ipc::Endpoint::Unix {
            http_socket: "/unused/http.sock".into(),
            grpc_socket: "/unused/grpc.sock".into(),
            uid: 1,
        };
        #[cfg(windows)]
        let endpoint = crate::ipc::Endpoint::NamedPipe {
            http_pipe: r"\\.\pipe\control-plane-test-http".into(),
            grpc_pipe: r"\\.\pipe\control-plane-test-grpc".into(),
            sid: "S-1-5-18".into(),
        };
        let state = Broker::new(super::super::Config {
            accounts: vec![super::super::Account {
                id: "alice".into(),
                name: "Alice".into(),
                endpoint,
            }],
        })
        .unwrap();
        let service = Service(state.clone());
        let mut invalid = Request::new(Empty {});
        invalid
            .metadata_mut()
            .insert("x-account-id", "alice".parse().unwrap());
        invalid
            .metadata_mut()
            .insert("x-device-token", "".parse().unwrap());
        assert_eq!(
            service.forward(invalid).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        let _all = state
            .connections
            .clone()
            .acquire_many_owned(256)
            .await
            .unwrap();
        let mut valid_shape = Request::new(Empty {});
        valid_shape
            .metadata_mut()
            .insert("x-account-id", "alice".parse().unwrap());
        valid_shape
            .metadata_mut()
            .insert("x-device-token", "a".repeat(64).parse().unwrap());
        assert_eq!(
            service.forward(valid_shape).await.unwrap_err().code(),
            tonic::Code::ResourceExhausted
        );
    }
    #[tokio::test]
    async fn subscriptions_hold_capacity_until_the_stream_is_dropped() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let stream = LimitedStream {
            inner: tokio_stream::empty::<Result<Event, Status>>(),
            _permit: permit,
        };
        assert_eq!(semaphore.available_permits(), 0);
        drop(stream);
        assert_eq!(semaphore.available_permits(), 1);
    }
}
