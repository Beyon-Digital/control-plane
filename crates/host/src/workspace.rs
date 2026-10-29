//! Workspace-scoped file and Git operations. Each worker must already run as its owner.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;

const MAX_TEXT: u64 = 2 * 1024 * 1024;

pub async fn resolve_for(
    target: &crate::models::ExecutionTarget,
    root: &str,
    relative: &str,
) -> Result<String> {
    match target {
        crate::models::ExecutionTarget::Native => Ok(resolve(Path::new(root), relative)?
            .to_string_lossy()
            .into_owned()),
        _ => crate::wsl::request(target, root, "resolve", json!({"path":relative})).await?["path"]
            .as_str()
            .map(str::to_owned)
            .context("WSL bridge omitted resolved path"),
    }
}
pub async fn list_for(
    target: &crate::models::ExecutionTarget,
    root: &str,
    relative: &str,
) -> Result<Value> {
    match target {
        crate::models::ExecutionTarget::Native => list(Path::new(root), relative),
        _ => crate::wsl::request(target, root, "list", json!({"path":relative})).await,
    }
}
pub async fn read_for(
    target: &crate::models::ExecutionTarget,
    root: &str,
    relative: &str,
) -> Result<Value> {
    match target {
        crate::models::ExecutionTarget::Native => read(Path::new(root), relative),
        _ => crate::wsl::request(target, root, "read", json!({"path":relative})).await,
    }
}
pub async fn write_for(
    target: &crate::models::ExecutionTarget,
    root: &str,
    relative: &str,
    content: &str,
    expected_sha256: Option<&str>,
) -> Result<Value> {
    match target {
        crate::models::ExecutionTarget::Native => {
            write(Path::new(root), relative, content, expected_sha256)
        }
        _ => {
            crate::wsl::request(
                target,
                root,
                "write",
                json!({"path":relative,"content":content,"expected_sha256":expected_sha256}),
            )
            .await
        }
    }
}
pub async fn git_for(
    target: &crate::models::ExecutionTarget,
    root: &str,
    args: &[&str],
) -> Result<Value> {
    match target {
        crate::models::ExecutionTarget::Native => git(Path::new(root), args).await,
        _ => crate::wsl::request(target, root, "git", json!({"args":args})).await,
    }
}

/// Resolve an existing path or a new file beneath an existing directory. Symlink
/// traversal, absolute input, and parent components are deliberately rejected.
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    let root = root.canonicalize().context("workspace root unavailable")?;
    let input = Path::new(relative);
    if input
        .components()
        .any(|v| v.as_os_str() == ".control-plane-transfers")
    {
        bail!("internal transfer staging paths are private");
    }
    if input.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("path must be relative to the workspace without parent components");
    }
    let mut path = root.clone();
    let parts: Vec<_> = input
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    for (i, component) in parts.iter().enumerate() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("symlinks are not exposed through the file API")
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && i == parts.len() - 1 => {}
            Err(e) => return Err(e.into()),
        }
    }
    if !path.starts_with(root) {
        bail!("path escapes workspace");
    }
    Ok(path)
}

pub fn list(root: &Path, relative: &str) -> Result<Value> {
    let path = resolve(root, relative)?;
    let mut entries = Vec::new();
    for item in fs::read_dir(path)? {
        let item = item?;
        let meta = item.metadata()?;
        if item.file_type()?.is_symlink() {
            continue;
        }
        let name = item.file_name().to_string_lossy().into_owned();
        if name == ".git" || name == ".control-plane-transfers" {
            continue;
        }
        entries.push(json!({"name":name,"is_dir":meta.is_dir(),"size":meta.len(),
            "path": Path::new(relative).join(&name).to_string_lossy()}));
        if entries.len() >= 10_000 {
            bail!("directory exceeds 10000 entries");
        }
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({"path":relative,"entries":entries}))
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn read(root: &Path, relative: &str) -> Result<Value> {
    let path = resolve(root, relative)?;
    if fs::metadata(&path)?.len() > MAX_TEXT {
        bail!("text file exceeds 2 MiB");
    }
    let data = fs::read(path)?;
    let content = std::str::from_utf8(&data).context("binary file is not editable")?;
    if content.contains('\0') {
        bail!("binary file is not editable");
    }
    Ok(json!({"path":relative,"content":content,"sha256":hash(&data)}))
}

pub fn write(
    root: &Path,
    relative: &str,
    content: &str,
    expected_sha256: Option<&str>,
) -> Result<Value> {
    if content.len() as u64 > MAX_TEXT || content.contains('\0') {
        bail!("invalid text or file exceeds 2 MiB");
    }
    let path = resolve(root, relative)?;
    if path == root.canonicalize()? {
        bail!("cannot replace workspace root");
    }
    let existing = match fs::read(&path) {
        Ok(data) => Some(data),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    match (expected_sha256, existing.as_ref()) {
        (Some(expected), Some(data)) if expected == hash(data) => {}
        (None, None) => {}
        _ => bail!("conflict: provide current sha256 to replace an existing file"),
    }
    let temp = path.with_file_name(format!(".control-plane-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        if let Ok(meta) = fs::metadata(&path) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        // Re-check immediately before replacing to detect concurrent editor changes.
        let current = fs::read(&path).ok();
        if current != existing {
            bail!("conflict: file changed during save");
        }
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    Ok(json!({"path":relative,"sha256":hash(content.as_bytes())}))
}

pub async fn git(root: &Path, args: &[&str]) -> Result<Value> {
    // Never invoke a shell. Server handlers must also authorize mutations.
    let allowed = [
        "status",
        "diff",
        "log",
        "show",
        "add",
        "commit",
        "push",
        "worktree",
        "rev-parse",
        "branch",
    ];
    if !args.first().is_some_and(|a| allowed.contains(a)) {
        bail!("unsupported git operation");
    }
    if args.iter().any(|a| a.contains('\0')) {
        bail!("invalid argument");
    }
    let mut command = Command::new("git");
    command
        .arg("--no-pager")
        .args(args)
        .current_dir(root.canonicalize()?)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("missing git stdout")?;
    let stderr = child.stderr.take().context("missing git stderr")?;
    let capture = async {
        tokio::try_join!(bounded_output(stdout), bounded_output(stderr), async {
            child.wait().await.map_err(anyhow::Error::from)
        })
    };
    let (stdout, stderr, status) = tokio::time::timeout(Duration::from_secs(120), capture)
        .await
        .context("git operation timed out")??;
    Ok(json!({"success":status.success(),"exit_code":status.code(),
        "stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr)}))
}

async fn bounded_output<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > 8 * 1024 * 1024 {
            bail!("git output exceeds 8 MiB per stream");
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let p = std::env::temp_dir().join(format!("cp-workspace-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn rejects_escape_and_absolute_paths() {
        let r = root();
        assert!(resolve(&r, "../secret").is_err());
        assert!(resolve(&r, "/tmp").is_err());
        fs::remove_dir_all(r).unwrap();
    }
    #[test]
    fn saves_require_matching_revision() {
        let r = root();
        write(&r, "a.txt", "one", None).unwrap();
        assert!(write(&r, "a.txt", "two", None).is_err());
        assert!(write(&r, "a.txt", "two", Some("stale")).is_err());
        let rev = read(&r, "a.txt").unwrap()["sha256"]
            .as_str()
            .unwrap()
            .to_owned();
        write(&r, "a.txt", "two", Some(&rev)).unwrap();
        assert_eq!(read(&r, "a.txt").unwrap()["content"], "two");
        fs::remove_dir_all(r).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_even_inside_root() {
        let r = root();
        fs::write(r.join("real"), "x").unwrap();
        std::os::unix::fs::symlink(r.join("real"), r.join("link")).unwrap();
        assert!(resolve(&r, "link").is_err());
        fs::remove_dir_all(r).unwrap();
    }
}
