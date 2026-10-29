//! Bonjour metadata is a discovery hint; TLS trust and device pairing remain mandatory.
use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;
pub struct Advertisement {
    daemon: ServiceDaemon,
    name: String,
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.name);
        let _ = self.daemon.shutdown();
    }
}
pub fn advertise(name: &str, http_port: u16, grpc_port: u16, path: &str) -> Result<Advertisement> {
    let daemon = ServiceDaemon::new()?;
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|v| v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .unwrap_or_else(|| "control-plane".into());
    let hostname = format!("{hostname}.local.");
    let properties = HashMap::from([
        ("tls".to_owned(), "1".to_owned()),
        ("path".into(), path.to_owned()),
        ("grpc_port".into(), grpc_port.to_string()),
        ("protocol".into(), "1".into()),
    ]);
    let service = ServiceInfo::new(
        "_control-plane._tcp.local.",
        &name.chars().take(48).collect::<String>(),
        &hostname,
        "",
        http_port,
        properties,
    )?
    .enable_addr_auto();
    let name = service.get_fullname().to_string();
    daemon.register(service)?;
    Ok(Advertisement { daemon, name })
}
