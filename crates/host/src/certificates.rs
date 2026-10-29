//! Per-device proof-of-possession enrollment. Private keys never leave the client.
use crate::{
    api::ApiError,
    core::Core,
    models::{now, Device},
    store::Store,
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    routing::{get, post},
    Extension, Json, Router,
};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Write, path::Path, sync::Arc};

pub const PEER_HEADER: &str = "x-control-plane-peer-certificate";
pub const CAPABLE_HEADER: &str = "x-control-plane-client-auth";
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerCertificate {
    pub device_id: String,
    pub fingerprint: String,
}
#[derive(Clone, Debug, Default)]
pub struct TlsPeer {
    pub certificate: Option<PeerCertificate>,
    pub client_auth_capable: bool,
}
#[derive(Clone, Debug)]
pub struct TrustedWorkerIPC;
pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS device_certificates(fingerprint TEXT PRIMARY KEY,device_id TEXT NOT NULL REFERENCES devices(id),expires INTEGER NOT NULL);CREATE INDEX IF NOT EXISTS certificates_device ON device_certificates(device_id);")?;
    Ok(())
}
impl Store {
    pub fn bind_certificate(&self, device: &str, fingerprint: &str, expires: i64) -> Result<()> {
        if fingerprint.len() != 64
            || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
            || expires <= now()
        {
            bail!("invalid device certificate binding");
        }
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let active: bool =
            tx.query_row("SELECT revoked=0 FROM devices WHERE id=?1", [device], |r| {
                r.get(0)
            })?;
        if !active {
            bail!("unauthorized device");
        }
        tx.execute(
            "INSERT INTO device_certificates VALUES(?1,?2,?3)",
            params![fingerprint, device, expires],
        )?;
        tx.execute("INSERT INTO audit(actor,action,target,created_at) VALUES(?1,'certificate.enroll',?2,?3)",params![device,fingerprint,now()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn requires_certificate(&self, device: &str) -> Result<bool> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM device_certificates WHERE device_id=?1)",
            [device],
            |r| r.get(0),
        )?)
    }
    pub fn certificate_active(&self, device: &str, fingerprint: &str) -> Result<bool> {
        Ok(self.0.lock().unwrap().query_row("SELECT 1 FROM device_certificates c JOIN devices d ON c.device_id=d.id WHERE c.device_id=?1 AND c.fingerprint=?2 AND c.expires>?3 AND d.revoked=0",params![device,fingerprint,now()],|r|r.get::<_,i64>(0)).optional()?.is_some())
    }
}
pub fn peer_from_der(bytes: &[u8]) -> Result<PeerCertificate> {
    use x509_parser::prelude::*;
    let (_, certificate) = X509Certificate::from_der(bytes)
        .map_err(|_| anyhow::anyhow!("invalid peer certificate"))?;
    let san = certificate
        .subject_alternative_name()?
        .context("device certificate requires a subject identity")?;
    let ids = san
        .value
        .general_names
        .iter()
        .filter_map(|value| match value {
            GeneralName::URI(value) => value.strip_prefix("urn:control-plane:device:"),
            _ => None,
        })
        .collect::<Vec<_>>();
    if ids.len() != 1 || uuid::Uuid::parse_str(ids[0]).is_err() {
        bail!("invalid device certificate identity");
    }
    Ok(PeerCertificate {
        device_id: ids[0].into(),
        fingerprint: hex::encode(Sha256::digest(bytes)),
    })
}
pub fn authorize(store: &Store, device: &Device, peer: Option<&PeerCertificate>) -> Result<()> {
    if !store.requires_certificate(&device.id)? {
        return Ok(());
    }
    let peer = peer.context("unauthorized: enrolled device certificate required")?;
    if peer.device_id != device.id || !store.certificate_active(&device.id, &peer.fingerprint)? {
        bail!("unauthorized: device token and certificate do not match");
    }
    Ok(())
}
pub fn request_peer(
    extensions: &axum::http::Extensions,
    headers: &axum::http::HeaderMap,
) -> Option<PeerCertificate> {
    if let Some(peer) = extensions.get::<TlsPeer>() {
        return peer.certificate.clone();
    }
    if extensions.get::<TrustedWorkerIPC>().is_some() {
        return headers
            .get(PEER_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|s| s.len() < 1024)
            .and_then(|v| serde_json::from_str(v).ok());
    }
    None
}
#[derive(Clone)]
pub struct Authority {
    pub ca_pem: String,
    key_pem: String,
}
fn protected_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
impl Authority {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        #[cfg(windows)]
        crate::ipc::windows::create_private_directory(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            bail!("identity directory must be a real directory");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if meta.uid() != nix::unistd::geteuid().as_raw() {
                bail!("identity directory belongs to another OS account");
            }
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }

        let cert = path.join("device-ca.pem");
        let key = path.join("device-ca-key.pem");
        if cert.exists() || key.exists() {
            for file in [&cert, &key] {
                let meta = std::fs::symlink_metadata(file)?;
                if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 65536 {
                    bail!("invalid device CA file");
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    if meta.uid() != nix::unistd::geteuid().as_raw()
                        || meta.permissions().mode() & 0o077 != 0
                    {
                        bail!("device CA files must be private to the worker");
                    }
                }
            }
            let result = Self {
                ca_pem: std::fs::read_to_string(cert)?,
                key_pem: std::fs::read_to_string(key)?,
            };
            Issuer::from_ca_cert_pem(&result.ca_pem, KeyPair::from_pem(&result.key_pem)?)?;
            return Ok(result);
        }
        let keypair = KeyPair::generate()?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
            .distinguished_name
            .push(DnType::CommonName, "Control Plane device enrollment");
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(3650);
        let certificate = params.self_signed(&keypair)?;
        let result = Self {
            ca_pem: certificate.pem(),
            key_pem: keypair.serialize_pem(),
        };
        protected_write(&key, result.key_pem.as_bytes())?;
        protected_write(&cert, result.ca_pem.as_bytes())?;
        Ok(result)
    }
    pub fn issue(&self, device: &str, csr: &str) -> Result<Value> {
        if uuid::Uuid::parse_str(device).is_err() || csr.len() > 16384 {
            bail!("invalid device identity or signing request");
        }
        let mut request = CertificateSigningRequestParams::from_pem(csr)
            .context("invalid certificate request or proof of possession")?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, device);
        params.subject_alt_names = vec![SanType::URI(
            format!("urn:control-plane:device:{device}").try_into()?,
        )];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.is_ca = IsCa::NoCa;
        params.serial_number = Some(uuid::Uuid::new_v4().as_bytes().to_vec().into());
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(30);
        let expires = params.not_after.unix_timestamp();
        request.params = params;
        let issuer = Issuer::from_ca_cert_pem(&self.ca_pem, KeyPair::from_pem(&self.key_pem)?)?;
        let cert = request.signed_by(&issuer)?;
        Ok(
            json!({"device_id":device,"certificate_pem":cert.pem(),"ca_pem":self.ca_pem,"fingerprint":hex::encode(Sha256::digest(cert.der().as_ref())),"expires_at":expires}),
        )
    }
}
#[derive(Deserialize)]
struct Enroll {
    csr_pem: String,
}
pub fn protected_router() -> Router<Arc<Core>> {
    Router::new().route("/v1/certificates/enroll", post(enroll))
}
pub fn public_router() -> Router<Arc<Core>> {
    Router::new().route("/.well-known/control-plane-identity", get(info))
}
async fn info(State(core): State<Arc<Core>>) -> Json<Value> {
    Json(match core.identity.get() {
        Some(a) => {
            json!({"device_certificates":true,"ca_pem":a.ca_pem,"algorithm":"ECDSA-P256-SHA256","validity_days":30})
        }
        None => json!({"device_certificates":false}),
    })
}
async fn enroll(
    State(core): State<Arc<Core>>,
    Extension(device): Extension<Device>,
    peer: Option<Extension<TlsPeer>>,
    ipc: Option<Extension<TrustedWorkerIPC>>,
    headers: axum::http::HeaderMap,
    Json(input): Json<Enroll>,
) -> Result<Json<Value>, ApiError> {
    let capable = peer.is_some_and(|p| p.client_auth_capable)
        || (ipc.is_some() && headers.get(CAPABLE_HEADER).is_some_and(|v| v == "1"));
    if !capable {
        return Err(anyhow::anyhow!("device certificate enrollment requires a TLS gateway configured to verify this worker CA").into());
    }
    let authority = core
        .identity
        .get()
        .context("device certificate issuer unavailable")?;
    let value = authority.issue(&device.id, &input.csr_pem)?;
    core.store.bind_certificate(
        &device.id,
        value["fingerprint"].as_str().unwrap(),
        value["expires_at"].as_i64().unwrap(),
    )?;
    Ok(Json(value))
}

pub fn client_auth_capable(
    extensions: &axum::http::Extensions,
    headers: &axum::http::HeaderMap,
) -> bool {
    extensions
        .get::<TlsPeer>()
        .is_some_and(|v| v.client_auth_capable)
        || (extensions.get::<TrustedWorkerIPC>().is_some()
            && headers.get(CAPABLE_HEADER).is_some_and(|v| v == "1"))
}
pub fn validate_pairing_csr(core: &Core, capable: bool, csr: Option<&str>) -> Result<()> {
    if capable && csr.is_none() {
        bail!("device pairing over TLS requires a proof-of-possession certificate request");
    }
    if let Some(csr) = csr {
        if !capable || csr.len() > 16384 {
            bail!("certificate pairing requires a TLS gateway configured with this worker CA");
        }
        core.identity
            .get()
            .context("device certificate issuer unavailable")?;
        CertificateSigningRequestParams::from_pem(csr)
            .context("invalid certificate request or proof of possession")?;
    }
    Ok(())
}
pub fn finish_pairing(core: &Core, mut result: Value, csr: Option<&str>) -> Result<Value> {
    if let Some(csr) = csr {
        let id = result["device_id"]
            .as_str()
            .context("device identity missing")?;
        let certificate = core
            .identity
            .get()
            .context("device issuer unavailable")?
            .issue(id, csr)?;
        if let Err(error) = core.store.bind_certificate(
            id,
            certificate["fingerprint"].as_str().unwrap(),
            certificate["expires_at"].as_i64().unwrap(),
        ) {
            let _ = core.store.revoke("failed-certificate-enrollment", id);
            return Err(error);
        }
        result["certificate"] = certificate;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn csr() -> String {
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::new(vec!["attacker.example".into()]).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.serialize_request(&key).unwrap().pem().unwrap()
    }
    #[test]
    fn issuer_overwrites_untrusted_identity_and_usage() {
        use x509_parser::prelude::*;
        let d = tempfile::tempdir().unwrap();
        let authority = Authority::load_or_create(&d.path().join("pki")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let value = authority.issue(&id, &csr()).unwrap();
        let pem = value["certificate_pem"].as_str().unwrap();
        let cert = rustls_pemfile::certs(&mut pem.as_bytes())
            .next()
            .unwrap()
            .unwrap();
        let peer = peer_from_der(cert.as_ref()).unwrap();
        assert_eq!(peer.device_id, id);
        let (_, x) = X509Certificate::from_der(cert.as_ref()).unwrap();
        assert!(!x.is_ca());
        assert!(x.extended_key_usage().unwrap().unwrap().value.client_auth);
        assert!(!x.extended_key_usage().unwrap().unwrap().value.server_auth);
        assert_eq!(
            x.subject_alternative_name()
                .unwrap()
                .unwrap()
                .value
                .general_names
                .len(),
            1
        );
        assert_eq!(
            authority.ca_pem,
            Authority::load_or_create(&d.path().join("pki"))
                .unwrap()
                .ca_pem
        );
    }
    #[test]
    fn binding_requires_matching_certificate_and_revocation() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(&d.path().join("state.db")).unwrap();
        let pair = store
            .pair(&store.issue_pairing("administrator").unwrap(), "Owner")
            .unwrap();
        let device = store.authenticate(pair["token"].as_str().unwrap()).unwrap();
        let peer = PeerCertificate {
            device_id: device.id.clone(),
            fingerprint: "a".repeat(64),
        };
        store
            .bind_certificate(&device.id, &peer.fingerprint, now() + 60)
            .unwrap();
        assert!(authorize(&store, &device, None).is_err());
        assert!(authorize(&store, &device, Some(&peer)).is_ok());
        assert!(authorize(
            &store,
            &device,
            Some(&PeerCertificate {
                device_id: uuid::Uuid::new_v4().to_string(),
                ..peer.clone()
            })
        )
        .is_err());
        assert!(authorize(
            &store,
            &device,
            Some(&PeerCertificate {
                fingerprint: "b".repeat(64),
                ..peer.clone()
            })
        )
        .is_err());
        store.revoke("local-administrator", &device.id).unwrap();
        assert!(authorize(&store, &device, Some(&peer)).is_err());
    }
    #[test]
    fn caller_headers_are_not_transport_proof() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            PEER_HEADER,
            r#"{"device_id":"fake","fingerprint":"fake"}"#.parse().unwrap(),
        );
        headers.insert(CAPABLE_HEADER, "1".parse().unwrap());
        let mut extensions = axum::http::Extensions::new();
        assert!(request_peer(&extensions, &headers).is_none());
        assert!(!client_auth_capable(&extensions, &headers));
        extensions.insert(TrustedWorkerIPC);
        assert!(request_peer(&extensions, &headers).is_some());
        assert!(client_auth_capable(&extensions, &headers));
    }
    #[tokio::test]
    async fn tls_pairing_binds_certificate_before_returning_token() {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(&d.path().join("state.db")).unwrap();
        let secret = store.issue_pairing("administrator").unwrap();
        let core = Core::new(
            store,
            vec![d.path().canonicalize().unwrap()],
            "Fixture".into(),
        )
        .unwrap();
        assert!(core
            .identity
            .set(Authority::load_or_create(&d.path().join("pki")).unwrap())
            .is_ok());
        let app = crate::api::router(core.clone());
        let payload = json!({"secret":secret,"device_name":"Fixture","csr_pem":csr()});
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/pair")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
        request.extensions_mut().insert(TlsPeer {
            certificate: None,
            client_auth_capable: true,
        });
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let data = response.into_body().collect().await.unwrap().to_bytes();
        let value: Value = serde_json::from_slice(&data).unwrap();
        let device = core
            .store
            .authenticate(value["token"].as_str().unwrap())
            .unwrap();
        assert!(core.store.requires_certificate(&device.id).unwrap());
        assert_eq!(value["certificate"]["device_id"], device.id);
        assert!(authorize(&core.store, &device, None).is_err());
    }
}
