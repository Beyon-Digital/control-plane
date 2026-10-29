//! Bounded binary streaming with revision checks and crash-recoverable scratch files.
use crate::{
    api::ApiError,
    core::Core,
    models::{Device, ExecutionTarget, Session},
};
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Extension, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
const CHUNK: usize = 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
struct Metadata {
    size: u64,
    sha256: String,
    version: String,
}
#[derive(Deserialize)]
struct FileQuery {
    session_id: String,
    path: String,
}
fn version(meta: &std::fs::Metadata) -> String {
    format!(
        "{}:{}:{}",
        meta.len(),
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|t| t.as_nanos())
            .unwrap_or(0),
        meta.created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    )
}
fn native_meta(path: &Path, max: u64) -> Result<Metadata> {
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > max {
        bail!("binary file exceeds transfer limit or is not regular");
    }
    let before = version(&meta);
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65536];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > max {
            bail!("file exceeds transfer limit");
        }
        hash.update(&bytes[..n]);
    }
    if version(&file.metadata()?) != before || size != meta.len() {
        bail!("conflict: file changed while calculating revision");
    }
    Ok(Metadata {
        size,
        sha256: hex::encode(hash.finalize()),
        version: before,
    })
}
async fn metadata(c: &Core, s: &Session, path: &str) -> Result<Metadata> {
    let max = c.store.resource_limits()?.max_transfer_bytes;
    match &s.execution_target {
        ExecutionTarget::Native => {
            let path = crate::workspace::resolve(Path::new(&s.workspace), path)?;
            tokio::task::spawn_blocking(move || native_meta(&path, max)).await?
        }
        _ => {
            let value = crate::wsl::request(
                &s.execution_target,
                &s.workspace,
                "transfer_meta",
                json!({"path":path,"max_bytes":max}),
            )
            .await?;
            if value["exists"] == false {
                bail!("file not found");
            }
            Ok(serde_json::from_value(value)?)
        }
    }
}
async fn chunk(s: &Session, path: &str, meta: &Metadata, offset: u64) -> Result<Vec<u8>> {
    let count = (meta.size - offset).min(CHUNK as u64) as usize;
    match &s.execution_target {
        ExecutionTarget::Native => {
            let path = crate::workspace::resolve(Path::new(&s.workspace), path)?;
            let mut file = tokio::fs::File::open(path).await?;
            if version(&file.metadata().await?) != meta.version {
                bail!("conflict: file changed during download");
            }
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut bytes = vec![0u8; count];
            file.read_exact(&mut bytes).await?;
            if version(&file.metadata().await?) != meta.version {
                bail!("conflict: file changed during download");
            }
            Ok(bytes)
        }
        _ => {
            let result=crate::wsl::request(&s.execution_target,&s.workspace,"transfer_read",json!({"path":path,"offset":offset,"length":count,"version":meta.version,"max_bytes":meta.size})).await?;
            let encoded = result["data"]
                .as_str()
                .context("invalid WSL binary chunk")?;
            if encoded.len() > CHUNK * 2 {
                bail!("invalid WSL binary chunk size");
            }
            let bytes = STANDARD.decode(encoded)?;
            if bytes.len() != count {
                bail!("truncated WSL binary chunk");
            }
            Ok(bytes)
        }
    }
}
fn staging(workspace: &str, relative: &str) -> Result<PathBuf> {
    let destination = crate::workspace::resolve(Path::new(workspace), relative)?;
    let directory = destination
        .parent()
        .context("destination parent required")?
        .join(".control-plane-transfers");
    #[cfg(windows)]
    crate::ipc::windows::create_private_directory(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let meta = std::fs::symlink_metadata(&directory)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("unsafe transfer staging directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != nix::unistd::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            bail!("transfer staging directory must be user-owned and private");
        }
    }
    Ok(directory)
}
struct Reservation {
    core: Arc<Core>,
    id: String,
    local: Option<PathBuf>,
    remote: Option<(ExecutionTarget, String, String)>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some((target, workspace, path)) = self.remote.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let c = self.core.clone();
                let id = self.id.clone();
                runtime.spawn(async move {
                    if crate::wsl::request(
                        &target,
                        &workspace,
                        "transfer_abort",
                        json!({"id":id,"path":path}),
                    )
                    .await
                    .is_ok()
                        && forget_job(&c, &id).is_ok()
                    {
                        let _ = c.store.release_transfer(&id);
                    }
                });
            }
        } else {
            let cleaned = self
                .local
                .as_ref()
                .is_none_or(|path| match std::fs::remove_file(path) {
                    Ok(()) => true,
                    Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                });
            if cleaned && forget_job(&self.core, &self.id).is_ok() {
                let _ = self.core.store.release_transfer(&self.id);
            }
        }
    }
}
fn expected(headers: &HeaderMap) -> Result<Option<String>> {
    let matched = headers
        .get(header::IF_MATCH)
        .map(|h| h.to_str())
        .transpose()?
        .map(str::trim);
    let nonmatched = headers
        .get(header::IF_NONE_MATCH)
        .map(|h| h.to_str())
        .transpose()?
        .map(str::trim);
    if matched.is_some() && nonmatched.is_some() {
        bail!("use either If-Match or If-None-Match");
    }
    if nonmatched.is_some_and(|value| value != "*") {
        bail!("If-None-Match must be * for create-only upload");
    }
    match matched {
        None => Ok(None),
        Some(value) => {
            let value = value.trim_matches('"');
            if value.len() != 64 || !value.bytes().all(|v| v.is_ascii_hexdigit()) {
                bail!("If-Match requires a SHA-256 file revision");
            }
            Ok(Some(value.to_ascii_lowercase()))
        }
    }
}
pub fn router() -> Router<Arc<Core>> {
    Router::new().route(
        "/v1/workspace/transfer",
        get(download).head(head).post(upload),
    )
}
async fn authorized(c: &Core, d: &Device, q: &FileQuery, write: bool) -> Result<Session> {
    let s = c.session(d, &q.session_id, write)?;
    c.workspace_for(&s.execution_target, &s.workspace).await?;
    Ok(s)
}
fn response(status: StatusCode, body: Body, meta: &Metadata) -> Result<Response> {
    Ok(Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, meta.size.to_string())
        .header(header::ETAG, format!("\"{}\"", meta.sha256))
        .header("x-content-sha256", &meta.sha256)
        .body(body)?)
}
fn not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        || error.to_string() == "file not found"
}
async fn head(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let s = authorized(&c, &d, &q, false).await?;
    let meta = match metadata(&c, &s, &q.path).await {
        Ok(meta) => meta,
        Err(error) if not_found(&error) => {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())?)
        }
        Err(error) => return Err(error.into()),
    };
    Ok(response(StatusCode::OK, Body::empty(), &meta)?)
}
async fn download(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiError> {
    let s = authorized(&c, &d, &q, false).await?;
    let meta = match metadata(&c, &s, &q.path).await {
        Ok(value) => value,
        Err(error) if not_found(&error) => {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())?)
        }
        Err(error) => return Err(error.into()),
    };
    let id = c.store.reserve_transfer(&d, &s.id, meta.size)?;
    let reservation = Reservation {
        core: c.clone(),
        id,
        local: None,
        remote: None,
    };
    let bytes = stream::try_unfold(
        (c, d, s, q.path, meta.clone(), 0u64, reservation),
        |(c, d, s, path, meta, offset, reservation)| async move {
            if offset >= meta.size {
                return Ok::<_, anyhow::Error>(None);
            }
            c.session(&d, &s.id, false)?;
            let bytes = chunk(&s, &path, &meta, offset).await?;
            let next = offset + bytes.len() as u64;
            Ok(Some((bytes, (c, d, s, path, meta, next, reservation))))
        },
    );
    Ok(response(StatusCode::OK, Body::from_stream(bytes), &meta)?)
}
async fn upload(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Query(q): Query<FileQuery>,
    request: Request,
) -> Result<Response, ApiError> {
    Ok(tokio::time::timeout(
        std::time::Duration::from_secs(300),
        upload_inner(c, d, q, request),
    )
    .await
    .context("upload timed out")??)
}
async fn upload_inner(c: Arc<Core>, d: Device, q: FileQuery, request: Request) -> Result<Response> {
    let s = authorized(&c, &d, &q, true).await?;
    let size = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .context("Content-Length is required for quota reservation")?;
    let expected = expected(request.headers())?;
    let id = c.store.reserve_transfer(&d, &s.id, size)?;
    let mut reservation = Reservation {
        core: c.clone(),
        id: id.clone(),
        local: None,
        remote: None,
    };
    record_job(&c, &id, &s, &q.path)?;
    let mut file = None;
    match &s.execution_target {
        ExecutionTarget::Native => {
            let destination = crate::workspace::resolve(Path::new(&s.workspace), &q.path)?;
            match std::fs::symlink_metadata(&destination) {
                Ok(_) => {
                    let actual = metadata(&c, &s, &q.path).await?;
                    if expected.as_deref() != Some(actual.sha256.as_str()) {
                        bail!("conflict: upload requires current If-Match SHA to replace an existing file");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if expected.is_some() {
                        bail!("conflict: expected file no longer exists");
                    }
                }
                Err(error) => return Err(error.into()),
            }
            let path = staging(&s.workspace, &q.path)?.join(&id);
            reservation.local = Some(path.clone());
            let mut options = tokio::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            file = Some(options.open(path).await?);
        }
        _ => {
            reservation.remote = Some((
                s.execution_target.clone(),
                s.workspace.clone(),
                q.path.clone(),
            ));
            crate::wsl::request(
                &s.execution_target,
                &s.workspace,
                "transfer_begin",
                json!({"id":id,"path":q.path,"size":size,"expected_sha256":expected}),
            )
            .await?;
        }
    }
    let mut stream = request.into_body().into_data_stream();
    let mut received = 0u64;
    let mut sent = 0u64;
    let mut pending = Vec::new();
    let mut hash = Sha256::new();
    while let Some(part) = stream.next().await {
        c.session(&d, &s.id, true)?;
        let part = part?;
        received = received
            .checked_add(part.len() as u64)
            .context("upload size overflow")?;
        if received > size {
            bail!("upload exceeds declared Content-Length");
        }
        hash.update(&part);
        if let Some(file) = file.as_mut() {
            file.write_all(&part).await?;
        } else {
            for slice in part.chunks(CHUNK) {
                pending.extend_from_slice(slice);
                while pending.len() >= CHUNK {
                    let rest = pending.split_off(CHUNK);
                    let data = std::mem::replace(&mut pending, rest);
                    crate::wsl::request(
                        &s.execution_target,
                        &s.workspace,
                        "transfer_chunk",
                        json!({"id":id,"path":q.path,"offset":sent,"data":STANDARD.encode(&data)}),
                    )
                    .await?;
                    sent += data.len() as u64;
                }
            }
        }
    }
    if received != size {
        bail!("upload ended before declared Content-Length");
    }
    if !pending.is_empty() {
        crate::wsl::request(
            &s.execution_target,
            &s.workspace,
            "transfer_chunk",
            json!({"id":id,"path":q.path,"offset":sent,"data":STANDARD.encode(&pending)}),
        )
        .await?;
    }
    if let Some(mut file) = file.take() {
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
    }
    let sha = hex::encode(hash.finalize());
    let _guard = c.mutation.lock().await;
    c.session(&d, &s.id, true)?;
    if matches!(s.execution_target, ExecutionTarget::Native) {
        let path = crate::workspace::resolve(Path::new(&s.workspace), &q.path)?;
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => {
                let actual = metadata(&c, &s, &q.path).await?;
                if expected.as_deref() != Some(actual.sha256.as_str()) {
                    bail!("conflict: destination changed during upload");
                }
                let staged = reservation.local.as_ref().context("staging file missing")?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::{MetadataExt, PermissionsExt};
                    std::fs::set_permissions(
                        staged,
                        std::fs::Permissions::from_mode(meta.mode() & 0o777),
                    )?;
                }
                #[cfg(not(unix))]
                std::fs::set_permissions(staged, meta.permissions())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if expected.is_some() {
                    bail!("conflict: destination was removed during upload");
                }
            }
            Err(error) => return Err(error.into()),
        }
        atomic_commit(
            reservation.local.as_ref().context("staging file missing")?,
            &path,
            expected.is_some(),
        )?;
        reservation.local = None;
    } else {
        let result = crate::wsl::request(
            &s.execution_target,
            &s.workspace,
            "transfer_commit",
            json!({"id":id,"path":q.path,"sha256":sha}),
        )
        .await?;
        if result["sha256"] != sha {
            bail!("WSL upload checksum mismatch");
        }
        reservation.remote = None;
    }
    let result = json!({"path":q.path,"size":size,"sha256":sha});
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ETAG, format!("\"{sha}\""))
        .body(Body::from(serde_json::to_vec(&result)?))?)
}
fn atomic_commit(staged: &Path, destination: &Path, replace: bool) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, ReplaceFileW};
        let from: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let ok = unsafe {
            if replace {
                ReplaceFileW(
                    to.as_ptr(),
                    from.as_ptr(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            } else {
                MoveFileExW(from.as_ptr(), to.as_ptr(), 0)
            }
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .context("conflict: destination could not be atomically published");
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        if replace {
            std::fs::rename(staged, destination)?;
            return Ok(());
        }
        std::fs::hard_link(staged, destination).context(
            "conflict: destination appeared or filesystem cannot publish create-only upload",
        )?;
        std::fs::remove_file(staged)?;
        Ok(())
    }
}
pub fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS transfer_jobs(id TEXT PRIMARY KEY,target_json TEXT NOT NULL,workspace TEXT NOT NULL,path TEXT NOT NULL,created_at INTEGER NOT NULL);")?;
    Ok(())
}
fn record_job(c: &Core, id: &str, s: &Session, path: &str) -> Result<()> {
    c.store.0.lock().unwrap().execute("INSERT INTO transfer_jobs(id,target_json,workspace,path,created_at) VALUES(?1,?2,?3,?4,?5)",rusqlite::params![id,serde_json::to_string(&s.execution_target)?,s.workspace,path,crate::models::now()])?;
    Ok(())
}
fn forget_job(c: &Core, id: &str) -> Result<()> {
    c.store
        .0
        .lock()
        .unwrap()
        .execute("DELETE FROM transfer_jobs WHERE id=?1", [id])?;
    Ok(())
}
pub async fn recover_staging(c: &Arc<Core>, startup: bool) -> Result<Value> {
    let cutoff = if startup {
        i64::MAX
    } else {
        crate::models::now() - 3600
    };
    let jobs = {
        let conn = c.store.0.lock().unwrap();
        let mut q = conn.prepare(
            "SELECT id,target_json,workspace,path FROM transfer_jobs WHERE created_at<?1",
        )?;
        let rows = q
            .query_map([cutoff], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut removed = 0;
    let mut failed = 0;
    for (id, target, workspace, path) in jobs {
        let target: ExecutionTarget = serde_json::from_str(&target)?;
        if c.workspace_for(&target, &workspace).await.is_err() {
            failed += 1;
            continue;
        }
        let cleaned = match target {
            ExecutionTarget::Native => match staging(&workspace, &path) {
                Ok(directory) => match std::fs::remove_file(directory.join(&id)) {
                    Ok(()) => true,
                    Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                },
                Err(_) => false,
            },
            _ => crate::wsl::request(
                &target,
                &workspace,
                "transfer_abort",
                json!({"id":id,"path":path}),
            )
            .await
            .is_ok(),
        };
        if cleaned {
            forget_job(c, &id)?;
            c.store.release_transfer(&id)?;
            removed += 1;
        } else {
            failed += 1;
        }
    }
    if startup {
        c.store.0.lock().unwrap().execute(
            "DELETE FROM transfer_reservations WHERE id NOT IN(SELECT id FROM transfer_jobs)",
            [],
        )?;
    }
    Ok(json!({"scratch_jobs_removed":removed,"scratch_jobs_retained":failed}))
}
#[cfg(test)]
mod integration_tests {
    use super::*;
    use http_body_util::BodyExt;
    #[tokio::test]
    async fn binary_upload_head_download_and_revision_replace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        let pair = store
            .pair(&store.issue_pairing("administrator").unwrap(), "Fixture")
            .unwrap();
        let d = store.authenticate(pair["token"].as_str().unwrap()).unwrap();
        let c = Core::new(
            store,
            vec![workspace.canonicalize().unwrap()],
            "Fixture".into(),
        )
        .unwrap();
        let s = Session {
            id: uuid::Uuid::new_v4().to_string(),
            provider: "codex".into(),
            workspace: workspace.to_string_lossy().into(),
            title: "Binary".into(),
            state: "idle".into(),
            owner: d.member_id.clone(),
            created_at: crate::models::now(),
            provider_session_id: None,
            shared: false,
            trusted: false,
            execution_target: Default::default(),
        };
        c.store
            .accept(
                &d.id,
                "binary-create",
                "create",
                &json!({}),
                &json!(s),
                &s,
                "state",
                json!({"state":"idle"}),
            )
            .unwrap();
        let q = || FileQuery {
            session_id: s.id.clone(),
            path: "file.bin".into(),
        };
        let data = vec![0u8, 255, 128, 0, 1, 2, 3];
        let request = Request::builder()
            .method("POST")
            .header(header::CONTENT_LENGTH, data.len())
            .header(header::IF_NONE_MATCH, "*")
            .body(Body::from(data.clone()))
            .unwrap();
        let uploaded = upload_inner(c.clone(), d.clone(), q(), request)
            .await
            .unwrap();
        let etag = uploaded.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        let response = head(State(c.clone()), Extension(d.clone()), Query(q()))
            .await
            .unwrap();
        assert_eq!(response.headers()[header::ETAG], etag);
        let downloaded = download(State(c.clone()), Extension(d.clone()), Query(q()))
            .await
            .unwrap();
        assert_eq!(
            downloaded
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            data
        );
        let request = Request::builder()
            .method("POST")
            .header(header::CONTENT_LENGTH, 3)
            .body(Body::from("bad"))
            .unwrap();
        assert!(upload_inner(c.clone(), d.clone(), q(), request)
            .await
            .is_err());
        assert_eq!(std::fs::read(workspace.join("file.bin")).unwrap(), data);
        let request = Request::builder()
            .method("POST")
            .header(header::CONTENT_LENGTH, 3)
            .header(header::IF_MATCH, etag)
            .body(Body::from("new"))
            .unwrap();
        upload_inner(c.clone(), d, q(), request).await.unwrap();
        assert_eq!(std::fs::read(workspace.join("file.bin")).unwrap(), b"new");
        assert_eq!(c.store.resource_usage().unwrap()["staging_bytes"], 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn create_only_publication_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest");
        let staged = dir.path().join("staged");
        std::fs::write(&staged, b"new").unwrap();
        std::fs::write(&dest, b"existing").unwrap();
        assert!(atomic_commit(&staged, &dest, false).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing");
        std::fs::remove_file(&dest).unwrap();
        atomic_commit(&staged, &dest, false).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!staged.exists());
    }
    #[test]
    fn conditional_headers_require_revision() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_MATCH, "*".parse().unwrap());
        assert!(expected(&h).is_err());
        h.insert(
            header::IF_MATCH,
            format!("\"{}\"", "a".repeat(64)).parse().unwrap(),
        );
        assert_eq!(expected(&h).unwrap(), Some("a".repeat(64)));
        h.insert(header::IF_NONE_MATCH, "*".parse().unwrap());
        assert!(expected(&h).is_err());
    }
    #[test]
    fn binary_metadata_exact_and_bounded() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("binary");
        let data = [0, 255, 128, 0, 1, 2, 3];
        std::fs::write(&p, data).unwrap();
        let m = native_meta(&p, 7).unwrap();
        assert_eq!(m.sha256, hex::encode(Sha256::digest(data)));
        assert!(native_meta(&p, 6).is_err());
    }
}
