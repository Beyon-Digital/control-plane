//! CPV1 H264 streaming. Each independently decodable IDR packet carries its
//! parameter sets, so dropping obsolete frames never corrupts decoder state.
use crate::{api::ApiError, core::Core, models::Device};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::Response,
    routing::get,
    Extension, Router,
};
use serde::{Deserialize, Serialize};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::watch,
};
use tokio_util::sync::CancellationToken;

pub const MAX_HEADER: usize = 64 * 1024;
pub const MAX_PAYLOAD: usize = 8 * 1024 * 1024;
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Header {
    pub version: u32,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub desktop_width: u32,
    pub desktop_height: u32,
    pub geometry_version: u64,
    pub timestamp_us: u64,
    pub keyframe: bool,
    pub sps_base64: String,
    pub pps_base64: String,
}
#[derive(Clone)]
pub struct Packet {
    pub header: Header,
    pub bytes: Vec<u8>,
}
impl Packet {
    pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Self> {
        let mut prefix = [0u8; 12];
        reader
            .read_exact(&mut prefix)
            .await
            .context("video helper stream ended")?;
        if &prefix[..4] != b"CPV1" {
            bail!("unsupported video framing; grant Screen Recording permission on the host");
        }
        let header_len = u32::from_be_bytes(prefix[4..8].try_into()?) as usize;
        let payload_len = u32::from_be_bytes(prefix[8..12].try_into()?) as usize;
        if header_len == 0
            || header_len > MAX_HEADER
            || payload_len == 0
            || payload_len > MAX_PAYLOAD
        {
            bail!("oversized or empty video packet");
        }
        let mut bytes = Vec::with_capacity(12 + header_len + payload_len);
        bytes.extend_from_slice(&prefix);
        bytes.resize(12 + header_len + payload_len, 0);
        reader.read_exact(&mut bytes[12..]).await?;
        let header: Header = serde_json::from_slice(&bytes[12..12 + header_len])?;
        if header.version != 1
            || header.codec != "h264"
            || !header.keyframe
            || header.width == 0
            || header.height == 0
            || header.width > 4096
            || header.height > 4096
            || header.desktop_width == 0
            || header.desktop_height == 0
            || header.desktop_width > 32768
            || header.desktop_height > 32768
            || header.sps_base64.is_empty()
            || header.pps_base64.is_empty()
        {
            bail!("invalid video configuration");
        }
        Ok(Self { header, bytes })
    }
}
pub fn router() -> Router<Arc<Core>> {
    Router::new().route("/v1/desktop/video", get(ws_video))
}
pub async fn ws_video(
    State(core): State<Arc<Core>>,
    Extension(device): Extension<Device>,
    peer: Option<Extension<crate::certificates::PeerCertificate>>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    Core::authorize(&device, true)?;
    let stop = core.desktop.stop_token();
    let permit = core
        .desktop
        .video_slots
        .clone()
        .try_acquire_owned()
        .context("desktop video viewer limit reached (2)")?;
    let mut command = core.desktop.video_command()?;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start H264 desktop helper")?;
    Ok(upgrade
        .max_message_size(4096)
        .on_upgrade(move |ws| async move {
            let _permit = permit;
            stream(ws, core, device, child, stop, peer.map(|p| p.0)).await
        }))
}
async fn stream(
    mut ws: WebSocket,
    core: Arc<Core>,
    device: Device,
    mut child: tokio::process::Child,
    stop: CancellationToken,
    peer: Option<crate::certificates::PeerCertificate>,
) {
    let Some(mut stdout) = child.stdout.take() else {
        return;
    };
    let client_stop = CancellationToken::new();
    let producer_stop = client_stop.clone();
    let emergency = stop.clone();
    let (tx, mut latest) = watch::channel::<Option<Result<Arc<Packet>, String>>>(None);
    let producer = tokio::spawn(async move {
        loop {
            let result = tokio::select! {biased;_=producer_stop.cancelled()=>break,_=emergency.cancelled()=>break,result=tokio::time::timeout(Duration::from_secs(5),Packet::read(&mut stdout))=>result.context("desktop video stalled").and_then(|value|value)};
            match result {
                Ok(packet) => {
                    if tx.send(Some(Ok(Arc::new(packet)))).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Some(Err(error.to_string())));
                    break;
                }
            }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
    });
    let mut check = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            biased;
            _=stop.cancelled()=>break,
            _=check.tick()=>{if !core.store.device_active(&device.id) || crate::certificates::authorize(&core.store,&device,peer.as_ref()).is_err() {break}},
            message=ws.recv()=>match message {Some(Ok(Message::Ping(data)))=>{if ws.send(Message::Pong(data)).await.is_err(){break}},Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,_=>{}},
            changed=latest.changed()=>{
                if changed.is_err(){break}
                let value=latest.borrow_and_update().clone();
                match value {
                    Some(Ok(packet))=>{
                        let sent=tokio::select!{_=stop.cancelled()=>false,result=tokio::time::timeout(Duration::from_secs(2),ws.send(Message::Binary(packet.bytes.clone().into())))=>matches!(result,Ok(Ok(())))};
                        if !sent {break}
                        let h=&packet.header; if core.desktop.observe_video(h.geometry_version,u64::from(h.desktop_width),u64::from(h.desktop_height)).is_err(){break}
                    },
                    Some(Err(error))=>{let _=ws.send(Message::Text(serde_json::json!({"error":error,"fallback":"snapshots"}).to_string().into())).await;break},None=>{}
                }
            }
        }
    }
    client_stop.cancel();
    let _ = producer.await;
    let _ = ws.send(Message::Close(None)).await;
}
#[cfg(test)]
mod tests {
    use super::*;
    fn packet() -> Vec<u8> {
        let header=serde_json::json!({"version":1,"codec":"h264","width":640,"height":480,"desktop_width":1280,"desktop_height":960,"geometry_version":3,"timestamp_us":1,"keyframe":true,"sps_base64":"Zw==","pps_base64":"aA=="}).to_string().into_bytes();
        let mut data = b"CPV1".to_vec();
        data.extend_from_slice(&(header.len() as u32).to_be_bytes());
        data.extend_from_slice(&5u32.to_be_bytes());
        data.extend_from_slice(&header);
        data.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        data
    }
    #[tokio::test]
    async fn framing_preserves_configuration_and_payload() {
        let bytes = packet();
        let parsed = Packet::read(&mut bytes.as_slice()).await.unwrap();
        assert_eq!(parsed.header.desktop_width, 1280);
        assert_eq!(parsed.bytes, bytes);
    }
    #[tokio::test]
    async fn rejects_unbounded_or_truncated_frames() {
        let mut oversized = b"CPV1".to_vec();
        oversized.extend_from_slice(&1u32.to_be_bytes());
        oversized.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(Packet::read(&mut oversized.as_slice()).await.is_err());
        let mut truncated = packet();
        truncated.pop();
        assert!(Packet::read(&mut truncated.as_slice()).await.is_err());
    }
}
