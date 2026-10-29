//! Publisher signatures authenticate exact manifest bytes before artifact URLs or installers are used.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const MAX_FEED: usize = 128 * 1024;
const MAX_ARTIFACT: u64 = 1024 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub format: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub channel: String,
    pub platform: String,
    pub version: String,
    pub sequence: u64,
    pub published_at: u64,
    pub expires_at: u64,
    pub artifact: Artifact,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    signed: String,
    signature: String,
}
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct State {
    sequence: u64,
    payload_hash: String,
}
#[derive(Clone)]
pub struct Verified {
    pub manifest: Manifest,
    pub envelope: Vec<u8>,
    pub payload_hash: String,
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn platform() -> Result<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("macos-arm64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Ok("windows-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("linux-x86_64")
    } else {
        bail!("updates are not configured for this platform")
    }
}
fn version(text: &str) -> Result<[u64; 3]> {
    let parts = text.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        bail!("stable major.minor.patch required")
    };
    let mut values = [0; 3];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty()
            || p.len() > 10
            || !p.bytes().all(|v| v.is_ascii_digit())
            || (p.len() > 1 && p.starts_with('0'))
        {
            bail!("invalid stable version")
        };
        values[i] = p.parse()?;
    }
    Ok(values)
}
fn https(text: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(text)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("update URLs require HTTPS without credentials or fragments")
    };
    Ok(url)
}
pub fn publisher_key() -> Result<Vec<u8>> {
    let text=option_env!("CONTROL_PLANE_UPDATE_PUBLIC_KEY").context("This development release has no pinned update publisher key. Install a publisher-signed release manually.")?;
    let key = STANDARD.decode(text)?;
    if key.len() != 32 {
        bail!("invalid pinned Ed25519 publisher key")
    };
    Ok(key)
}
pub fn default_feed() -> Result<&'static str> {
    option_env!("CONTROL_PLANE_UPDATE_FEED")
        .context("This release has no configured HTTPS update feed.")
}
fn verify_inner(
    bytes: &[u8],
    key: &[u8],
    current: &str,
    target: &str,
    feed: &str,
    clock: u64,
    require_newer: bool,
) -> Result<Verified> {
    if bytes.len() > MAX_FEED {
        bail!("update feed exceeds limit")
    };
    let envelope: Envelope = serde_json::from_slice(bytes)?;
    let payload = STANDARD.decode(&envelope.signed)?;
    if payload.len() > 64 * 1024 {
        bail!("signed payload exceeds limit")
    };
    let signature = STANDARD.decode(&envelope.signature)?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&payload, &signature)
        .map_err(|_| anyhow::anyhow!("update publisher signature invalid"))?;
    let manifest: Manifest = serde_json::from_slice(&payload)?;
    if manifest.schema != 1
        || manifest.channel != "stable"
        || manifest.platform != target
        || manifest.sequence == 0
    {
        bail!("update schema/channel/platform/sequence mismatch")
    };
    let offered = version(&manifest.version)?;
    let installed = version(current)?;
    if manifest.published_at > clock.saturating_add(300)
        || manifest.expires_at <= clock
        || manifest.expires_at <= manifest.published_at
        || manifest.expires_at - manifest.published_at > 31 * 86400
    {
        bail!("update manifest expired or timestamps invalid")
    };
    let source = https(feed)?;
    let artifact = https(&manifest.artifact.url)?;
    if artifact.origin() != source.origin() {
        bail!("artifact origin differs from feed origin")
    };
    let expected = match target {
        "macos-arm64" => "pkg",
        "windows-x86_64" => "msi",
        "linux-x86_64" => "deb",
        _ => bail!("unsupported update target"),
    };
    if manifest.artifact.format != expected
        || manifest.artifact.size == 0
        || manifest.artifact.size > MAX_ARTIFACT
        || hex::decode(&manifest.artifact.sha256)?.len() != 32
    {
        bail!("invalid artifact metadata")
    };
    if require_newer && offered <= installed {
        bail!("update is not newer than installed version")
    };
    Ok(Verified {
        manifest,
        envelope: bytes.to_vec(),
        payload_hash: hex::encode(Sha256::digest(&payload)),
    })
}
pub fn verify(
    bytes: &[u8],
    key: &[u8],
    current: &str,
    target: &str,
    feed: &str,
    clock: u64,
) -> Result<Verified> {
    verify_inner(bytes, key, current, target, feed, clock, true)
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(600))
        .build()?)
}
async fn fetch_inner(feed: &str, current: &str, require_newer: bool) -> Result<Verified> {
    https(feed)?;
    let mut response = client()?
        .get(feed)
        .header("accept", "application/json")
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("update feed returned HTTP {}", response.status())
    };
    if response
        .content_length()
        .is_some_and(|n| n > MAX_FEED as u64)
    {
        bail!("feed exceeds limit")
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_FEED {
            bail!("feed exceeds limit")
        };
        bytes.extend_from_slice(&chunk);
    }
    verify_inner(
        &bytes,
        &publisher_key()?,
        current,
        platform()?,
        feed,
        now(),
        require_newer,
    )
}
pub async fn fetch(feed: &str, current: &str) -> Result<Verified> {
    fetch_inner(feed, current, true).await
}
pub async fn check(feed: &str, current: &str) -> Result<serde_json::Value> {
    let v = fetch_inner(feed, current, false).await?;
    let available = version(&v.manifest.version)? > version(current)?;
    let mut value = serde_json::to_value(&v.manifest)?;
    value["available"] = serde_json::json!(available);
    Ok(value)
}
fn private_directory(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        crate::ipc::windows::create_private_directory(path)?;
        crate::ipc::windows::validate_private_directory(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        if !path.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        let m = std::fs::symlink_metadata(path)?;
        let uid = nix::unistd::geteuid().as_raw();
        if !m.is_dir() || m.file_type().is_symlink() || m.uid() != uid {
            bail!("staging directory must belong to this OS user and not be a symlink")
        };
        for parent in path
            .canonicalize()?
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
        {
            let m = std::fs::metadata(parent)?;
            let sticky_root = m.uid() == 0 && m.mode() & 0o1000 != 0;
            if ![0, uid].contains(&m.uid()) || (m.mode() & 0o022 != 0 && !sticky_root) {
                bail!("staging ancestor replaceable by another OS user")
            };
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path.canonicalize()?)
}
fn new_file(path: &Path) -> Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    Ok(o.open(path)?)
}
fn lock_directory(dir: &Path) -> Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    let file = o.open(dir.join("update.lock"))?;
    if !file.metadata()?.is_file() {
        bail!("invalid update lock path")
    };
    fs2::FileExt::try_lock_exclusive(&file).context("another update operation active")?;
    Ok(file)
}
fn read_small(path: &Path, max: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut f = o.open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.len() > max as u64 {
        bail!("invalid update metadata file")
    };
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut f)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        bail!("metadata exceeds bound")
    };
    Ok(bytes)
}
fn check_sequence(dir: &Path, v: &Verified) -> Result<()> {
    let path = dir.join("update-state.json");
    if !path.exists() {
        return Ok(());
    };
    let s: State = serde_json::from_slice(&read_small(&path, 4096)?)?;
    if v.manifest.sequence < s.sequence
        || (v.manifest.sequence == s.sequence && v.payload_hash != s.payload_hash)
    {
        bail!("update sequence rollback or equivocation detected")
    };
    Ok(())
}
fn save_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut f = new_file(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        let target = dir.join(name);
        if let Ok(m) = std::fs::symlink_metadata(&target) {
            if !m.is_file() || m.file_type().is_symlink() {
                bail!("unexpected update metadata path")
            }
        };
        crate::update_drain::replace_file(&temp, &target)?;
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
async fn verify_file(path: &Path, a: &Artifact) -> Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let f = o.open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.len() != a.size {
        bail!("staged artifact size/type mismatch")
    };
    let mut f = tokio::fs::File::from_std(f);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = f.read(&mut buffer).await?;
        if n == 0 {
            break;
        };
        digest.update(&buffer[..n]);
    }
    if hex::encode(digest.finalize()) != a.sha256.to_lowercase() {
        bail!("artifact SHA-256 mismatch")
    };
    Ok(())
}
pub async fn stage(directory: &Path, v: Verified) -> Result<PathBuf> {
    let dir = private_directory(directory)?;
    let _lock = lock_directory(&dir)?;
    check_sequence(&dir, &v)?;
    let m = &v.manifest;
    let name = format!(
        "ControlPlane-{}-{}.{}",
        m.version, m.platform, m.artifact.format
    );
    let target = dir.join(&name);
    if target.exists() {
        verify_file(&target, &m.artifact).await?;
    } else {
        let temp = dir.join(format!(".{}.download", uuid::Uuid::new_v4()));
        let mut f = tokio::fs::File::from_std(new_file(&temp)?);
        let result = async {
            let mut r = client()?.get(&m.artifact.url).send().await?;
            if !r.status().is_success() {
                bail!("artifact HTTP {}", r.status())
            };
            if r.content_length().is_some_and(|s| s != m.artifact.size) {
                bail!("content length differs from signed size")
            };
            let mut size = 0u64;
            let mut digest = Sha256::new();
            while let Some(chunk) = r.chunk().await? {
                size = size
                    .checked_add(chunk.len() as u64)
                    .context("size overflow")?;
                if size > m.artifact.size {
                    bail!("artifact exceeds signed size")
                };
                digest.update(&chunk);
                f.write_all(&chunk).await?;
            }
            if size != m.artifact.size
                || hex::encode(digest.finalize()) != m.artifact.sha256.to_lowercase()
            {
                bail!("artifact hash/size mismatch")
            };
            f.flush().await?;
            f.sync_all().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drop(f);
        if let Err(e) = result {
            let _ = std::fs::remove_file(temp);
            return Err(e);
        };
        std::fs::rename(temp, &target)?;
    }
    save_atomic(&dir, &format!("{name}.manifest.json"), &v.envelope)?;
    save_atomic(
        &dir,
        "update-state.json",
        &serde_json::to_vec(&State {
            sequence: m.sequence,
            payload_hash: v.payload_hash.clone(),
        })?,
    )?;
    Ok(target)
}
pub async fn apply(path: &Path, feed: &str, current: &str, worker_data: &[PathBuf]) -> Result<()> {
    if worker_data.is_empty() {
        bail!("explicit local worker data directories required before installation")
    };
    let dir = private_directory(path.parent().context("artifact directory required")?)?;
    let _lock = lock_directory(&dir)?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .context("invalid staged name")?;
    let envelope = read_small(&dir.join(format!("{name}.manifest.json")), MAX_FEED)?;
    let v = verify(
        &envelope,
        &publisher_key()?,
        current,
        platform()?,
        feed,
        now(),
    )?;
    check_sequence(&dir, &v)?;
    if name
        != format!(
            "ControlPlane-{}-{}.{}",
            v.manifest.version, v.manifest.platform, v.manifest.artifact.format
        )
    {
        bail!("artifact filename differs from signed metadata")
    };
    let target = dir.join(name);
    verify_file(&target, &v.manifest.artifact).await?;
    let absolute = target.canonicalize()?;
    let _workers = crate::update_drain::acquire(worker_data).await?;
    verify_file(&absolute, &v.manifest.artifact).await?;
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = tokio::process::Command::new("/usr/bin/open");
        c.arg(&absolute);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = tokio::process::Command::new("msiexec.exe");
        c.arg("/i").arg(&absolute);
        c
    } else {
        let mut c = tokio::process::Command::new("xdg-open");
        c.arg(&absolute);
        c
    };
    let mut child = cmd.spawn().context("open OS installer")?;
    if let Ok(result) = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await
    {
        let status = result?;
        if !status.success() && !matches!(status.code(), Some(1641 | 3010)) {
            bail!("OS installer could not be opened")
        };
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    fn sample() -> Manifest {
        Manifest {
            schema: 1,
            channel: "stable".into(),
            platform: "linux-x86_64".into(),
            version: "0.2.0".into(),
            sequence: 10,
            published_at: 999999,
            expires_at: 1001000,
            artifact: Artifact {
                url: "https://updates.example/artifact.deb".into(),
                size: 3,
                sha256: "00".repeat(32),
                format: "deb".into(),
            },
        }
    }
    fn signed(m: &Manifest) -> (Vec<u8>, Vec<u8>) {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let p = serde_json::to_vec(m).unwrap();
        (
            serde_json::to_vec(&Envelope {
                signed: STANDARD.encode(&p),
                signature: STANDARD.encode(pair.sign(&p).as_ref()),
            })
            .unwrap(),
            pair.public_key().as_ref().to_vec(),
        )
    }
    fn valid(m: &Manifest) -> Result<Verified> {
        let (b, k) = signed(m);
        verify(
            &b,
            &k,
            "0.1.0",
            "linux-x86_64",
            "https://updates.example/stable.json",
            1000000,
        )
    }
    #[test]
    fn rejects_payload_tampering() {
        let mut m = sample();
        assert!(valid(&m).is_ok());
        let (b, k) = signed(&m);
        let mut e: Envelope = serde_json::from_slice(&b).unwrap();
        m.version = "0.3.0".into();
        e.signed = STANDARD.encode(serde_json::to_vec(&m).unwrap());
        assert!(verify(
            &serde_json::to_vec(&e).unwrap(),
            &k,
            "0.1.0",
            "linux-x86_64",
            "https://updates.example/stable.json",
            1000000
        )
        .is_err());
    }
    #[test]
    fn rejects_expiry_platform_downgrade_origin_size() {
        for key in 0..6 {
            let mut m = sample();
            match key {
                0 => m.expires_at = 1000000,
                1 => m.platform = "windows-x86_64".into(),
                2 => m.version = "0.1.0".into(),
                3 => m.artifact.url = "https://evil.example/file.deb".into(),
                4 => m.artifact.url = "http://updates.example/file.deb".into(),
                _ => m.artifact.size = MAX_ARTIFACT + 1,
            };
            assert!(valid(&m).is_err());
        }
    }
    #[test]
    fn rejects_sequence_rollback_equivocation() {
        let d = tempfile::tempdir().unwrap();
        let good = valid(&sample()).unwrap();
        save_atomic(
            d.path(),
            "update-state.json",
            &serde_json::to_vec(&State {
                sequence: 10,
                payload_hash: good.payload_hash.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(check_sequence(d.path(), &good).is_ok());
        let mut m = sample();
        m.sequence = 9;
        assert!(check_sequence(d.path(), &valid(&m).unwrap()).is_err());
        m = sample();
        m.version = "0.3.0".into();
        assert!(check_sequence(d.path(), &valid(&m).unwrap()).is_err());
    }
    #[test]
    fn equal_version_still_validates_expiry() {
        let mut m = sample();
        m.version = "0.1.0".into();
        let (b, k) = signed(&m);
        assert!(verify_inner(
            &b,
            &k,
            "0.1.0",
            "linux-x86_64",
            "https://updates.example/stable.json",
            1000000,
            false
        )
        .is_ok());
        m.expires_at = 1000000;
        let (b, k) = signed(&m);
        assert!(verify_inner(
            &b,
            &k,
            "0.1.0",
            "linux-x86_64",
            "https://updates.example/stable.json",
            1000000,
            false
        )
        .is_err());
    }
    #[tokio::test]
    async fn rejects_substituted_artifacts() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("artifact.deb");
        std::fs::write(&p, b"bad").unwrap();
        let mut a = sample().artifact;
        a.sha256 = hex::encode(Sha256::digest(b"abc"));
        assert!(verify_file(&p, &a).await.is_err());
        std::fs::write(&p, b"abc").unwrap();
        assert!(verify_file(&p, &a).await.is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlink_staging() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("link");
        std::os::unix::fs::symlink(d.path(), &p).unwrap();
        assert!(private_directory(&p).is_err());
    }
}
