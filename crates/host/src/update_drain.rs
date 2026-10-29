//! Owner-authenticated local maintenance. The worker exits before installer launch.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Maintenance {
    id: String,
    created_at: i64,
    installed_version: String,
}
fn directory(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    crate::ipc::windows::validate_private_directory(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("worker data must be a private real directory")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != nix::unistd::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            bail!("worker data must be owned by the updater user with mode 0700")
        }
    }
    Ok(path.canonicalize()?)
}
fn open(path: &Path, create: bool) -> Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true);
    if create {
        o.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    let file = o.open(path)?;
    let m = file.metadata()?;
    if !m.is_file() {
        bail!("unsafe maintenance file")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.uid() != nix::unistd::geteuid().as_raw() || m.mode() & 0o077 != 0 {
            bail!("maintenance file must be owner-only")
        }
    }
    #[cfg(windows)]
    crate::ipc::windows::validate_registry_handle(&file, path)?;
    Ok(file)
}
pub fn pending(path: &Path) -> Result<bool> {
    let path = directory(path)?.join("update.drain");
    let mut file = match open(&path, false) {
        Ok(v) => v,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|v| v.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false)
        }
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("maintenance record too large")
    };
    let v: Maintenance = serde_json::from_slice(&bytes)?;
    if uuid::Uuid::parse_str(&v.id).is_err() || v.created_at > crate::models::now() + 300 {
        bail!("invalid maintenance record")
    };
    // Same-version restarts remain blocked until explicit resume. A newly installed
    // version consumes the marker, so service managers cannot restart old binaries.
    if v.installed_version != env!("CARGO_PKG_VERSION") {
        std::fs::remove_file(path)?;
        return Ok(false);
    };
    Ok(true)
}
pub fn resume(path: &Path) -> Result<()> {
    let path = directory(path)?.join("update.drain");
    if path.exists() {
        let _ = open(&path, false)?;
        std::fs::remove_file(path)?;
    }
    Ok(())
}
pub async fn acquire(paths: &[PathBuf]) -> Result<Vec<File>> {
    let mut directories = paths
        .iter()
        .map(|p| directory(p))
        .collect::<Result<Vec<_>>>()?;
    directories.sort();
    directories.dedup();
    if directories.is_empty() {
        bail!("no worker directories supplied")
    }
    let mut held = Vec::new();
    for dir in directories {
        let record = Maintenance {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: crate::models::now(),
            installed_version: env!("CARGO_PKG_VERSION").into(),
        };
        let temporary = dir.join(format!(".{}.drain", record.id));
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut request = o.open(&temporary)?;
        request.write_all(&serde_json::to_vec(&record)?)?;
        request.sync_all()?;
        drop(request);
        replace_file(&temporary, &dir.join("update.drain"))?;
        let lock = open(&dir.join("worker.lock"), true)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if fs2::FileExt::try_lock_exclusive(&lock).is_ok() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("worker did not drain within 30 seconds; installer was not launched. Maintenance remains active; resume explicitly after inspecting the worker.")
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        held.push(lock);
    }
    Ok(held)
}
pub fn replace_file(source: &Path, target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let a: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileExW(a.as_ptr(), b.as_ptr(), 1 | 8)
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("atomic update metadata replacement");
        };
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(source, target).context("atomic update metadata replacement")
    }
}

pub fn worker_lock(path: &Path) -> Result<File> {
    open(&directory(path)?.join("worker.lock"), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn maintenance_blocks_same_version_restart_until_resume() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let locks = acquire(&[d.path().into()]).await.unwrap();
        assert!(pending(d.path()).unwrap());
        assert!(fs2::FileExt::try_lock_exclusive(
            &open(&d.path().join("worker.lock"), false).unwrap()
        )
        .is_err());
        drop(locks);
        resume(d.path()).unwrap();
        assert!(!pending(d.path()).unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn unsafe_signal_file_rejected() {
        let d = tempfile::tempdir().unwrap();
        use std::os::unix::fs::{symlink, PermissionsExt};
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink("/etc/passwd", d.path().join("update.drain")).unwrap();
        assert!(pending(d.path()).is_err());
    }
}
