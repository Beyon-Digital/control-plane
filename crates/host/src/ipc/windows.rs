//! Named pipes are local-only and have an explicit protected DACL for the worker
//! owner and broker identity. The broker also verifies the server process SID.
use super::{Endpoint, Stream};
use anyhow::{bail, Context, Result};
use std::{
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Component, Path, PathBuf, Prefix},
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, HANDLE},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    System::{
        Pipes::GetNamedPipeServerProcessId,
        Threading::{
            GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};
use windows_sys::Win32::{
    Foundation::{GENERIC_ALL, GENERIC_WRITE, INVALID_HANDLE_VALUE},
    Security::{
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
        GetAce, IsValidAcl, IsValidSid, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
        INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PSID,
    },
    Storage::FileSystem::{
        CreateFileW, GetDriveTypeW, GetFileInformationByHandle, GetFileType,
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED,
        FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, OPEN_EXISTING, READ_CONTROL,
        SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
    },
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn token_sid(token: HANDLE) -> Result<String> {
    let mut length = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut length);
    }
    if length == 0 || length > 64 * 1024 {
        bail!("invalid token user information");
    }
    // usize storage gives TOKEN_USER's pointer fields their required alignment.
    let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let result = unsafe {
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    };
    unsafe {
        LocalFree(text.cast());
    }
    Ok(result)
}
pub fn current_sid() -> Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let token = Handle(token);
    token_sid(token.0)
}
fn process_sid(pid: u32) -> Result<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let process = Handle(process);
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let token = Handle(token);
    token_sid(token.0)
}

/// Tokio's generic-write client builder also requests FILE_APPEND_DATA, which
/// aliases FILE_CREATE_PIPE_INSTANCE. Open with individual rights so a broker
/// can exchange data but cannot create/replace a worker server instance.
fn open_client(name: &str) -> Result<NamedPipeClient> {
    if name.encode_utf16().any(|c| c == 0) {
        bail!("invalid pipe name");
    }
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let rights = FILE_READ_DATA
        | FILE_WRITE_DATA
        | FILE_READ_ATTRIBUTES
        | FILE_WRITE_ATTRIBUTES
        | READ_CONTROL
        | SYNCHRONIZE;
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            rights,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    // Ownership transfers even if Tokio's registration returns an error.
    Ok(unsafe { NamedPipeClient::from_raw_handle(handle.cast()) }?)
}
pub async fn connect(name: &str, expected_sid: &str) -> Result<Stream> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match open_client(name) {
            Ok(pipe) => {
                let mut pid = 0;
                if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle().cast(), &mut pid) }
                    == 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                if process_sid(pid)? != expected_sid {
                    bail!("worker named-pipe server SID mismatch");
                }
                return Ok(Box::pin(pipe));
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.raw_os_error() == Some(231))
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await
            }
            Err(error) => return Err(error),
        }
    }
}
fn create(name: &str, broker_sid: &str, first: bool) -> Result<NamedPipeServer> {
    let owner = current_sid()?;
    Endpoint::NamedPipe {
        http_pipe: name.into(),
        grpc_pipe: format!("{name}-validation"),
        sid: broker_sid.into(),
    }
    .validate()?;
    let descriptor = pipe_descriptor(&owner, broker_sid);
    let descriptor: Vec<u16> = descriptor.encode_utf16().chain(Some(0)).collect();
    let mut security = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            1,
            &mut security,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security,
        bInheritHandle: 0,
    };
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    };
    unsafe {
        LocalFree(security);
    }
    Ok(result.context("create owner-restricted worker pipe")?)
}

fn pipe_descriptor(owner: &str, broker: &str) -> String {
    // 0x120183: READ/WRITE_DATA, READ/WRITE_ATTRIBUTES, READ_CONTROL, SYNCHRONIZE.
    // Broker gets neither CREATE_PIPE_INSTANCE nor ACL/owner mutation rights.
    if owner == broker {
        format!("D:P(A;;GA;;;{owner})")
    } else {
        format!("D:P(A;;GA;;;{owner})(A;;0x120183;;;{broker})")
    }
}

struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

fn sid_string(sid: PSID) -> Result<String> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        bail!("invalid security descriptor SID");
    }
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let _allocated = LocalAllocation(text.cast());
    let mut length = 0;
    while length < 1024 && unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    if length == 1024 {
        bail!("invalid SID string length");
    }
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, length)
    }))
}

fn trusted_sid(sid: &str, current: &str) -> bool {
    sid == current || sid == "S-1-5-18" || sid == "S-1-5-32-544"
}

// Windows assigns ownership of system directories, including the system drive
// root, to this specific privileged OS service. Do not trust the service SID
// prefix: other services do not have the same authority.
const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Conservatively reject all untrusted allow ACEs capable of modifying a file
/// or replacing a directory in its ancestry. Deny ACE ordering cannot turn an
/// otherwise rejected grant into an accepted configuration.
fn validate_acl(owner: PSID, dacl: *const ACL, current: &str, directory: bool) -> Result<()> {
    let owner = sid_string(owner)?;
    if !trusted_sid(&owner, current) && owner != TRUSTED_INSTALLER_SID {
        bail!("broker registry object has an untrusted owner SID");
    }
    if dacl.is_null() || unsafe { IsValidAcl(dacl) } == 0 {
        bail!("broker registry requires an explicit valid DACL");
    }
    let acl = unsafe { &*dacl };
    let acl_start = dacl as usize;
    let acl_end = acl_start
        .checked_add(acl.AclSize as usize)
        .context("invalid ACL length")?;
    // On ancestors, ADD_FILE/ADD_SUBDIRECTORY only permits new siblings, not
    // replacing an existing child whose DELETE right we validate separately.
    let replacement = DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL | GENERIC_WRITE | 0x40;
    let mutation = if directory {
        replacement
    } else {
        replacement | 0x2 | 0x4 | 0x10 | 0x100
    };
    for index in 0..u32::from(acl.AceCount) {
        let mut raw = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut raw) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let address = raw as usize;
        if address < acl_start
            || address
                .checked_add(std::mem::size_of::<ACE_HEADER>())
                .is_none_or(|end| end > acl_end)
        {
            bail!("invalid ACE bounds");
        }
        let header = unsafe { std::ptr::read_unaligned(raw.cast::<ACE_HEADER>()) };
        let size = usize::from(header.AceSize);
        if size < std::mem::size_of::<ACE_HEADER>()
            || address.checked_add(size).is_none_or(|end| end > acl_end)
        {
            bail!("invalid ACE length");
        }
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match header.AceType {
            1 => continue, // ACCESS_DENIED_ACE_TYPE cannot grant authority.
            0 => {}        // ACCESS_ALLOWED_ACE_TYPE, documented winnt.h binary layout.
            _ => bail!("broker registry DACL contains unsupported conditional/object ACEs"),
        }
        if size < 16 {
            bail!("invalid access-allowed ACE");
        }
        let bytes = raw.cast::<u8>();
        let mask = unsafe { std::ptr::read_unaligned(bytes.add(4).cast::<u32>()) };
        if mask & mutation == 0 {
            continue;
        }
        let sid = unsafe { bytes.add(8) };
        let subauthorities = usize::from(unsafe { *sid.add(1) });
        if subauthorities > 15 || 8 + 8 + 4 * subauthorities > size {
            bail!("invalid ACE SID bounds");
        }
        let trustee = sid_string(sid.cast())?;
        if !trusted_sid(&trustee, current) {
            bail!("broker registry can be modified or replaced by another OS identity");
        }
    }
    Ok(())
}

fn validate_handle(handle: HANDLE, current: &str, directory: bool) -> Result<()> {
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        bail!("broker registry must be on a local disk filesystem");
    }
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    if unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        bail!(
            "broker registry and parents must be ordinary files/directories without reparse points"
        );
    }
    let mut owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result as i32).into());
    }
    let _allocated = LocalAllocation(descriptor);
    validate_acl(owner, dacl, current, directory)
}

fn registry_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        bail!("broker registry requires an absolute local path without parent traversal");
    }
    match path.components().next() {
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => {}
        _ => bail!("broker registry cannot use UNC, device, or remote filesystem paths"),
    }
    if path.as_os_str().encode_wide().any(|c| c == 0) {
        bail!("invalid broker registry path");
    }
    Ok(path)
}

/// Reject remote/device paths before opening the registry can initiate remote I/O.
pub fn validate_registry_path(path: &Path) -> Result<()> {
    let path = registry_path(path)?;
    let root: PathBuf = path.components().take(2).collect();
    let root: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    // Win32 DRIVE_FIXED=3, DRIVE_RAMDISK=6; mapped network drive letters are
    // remote even though they have a drive prefix and GetFileType returns DISK.
    if !matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 3 | 6) {
        bail!("broker registry must reside on a local fixed disk");
    }
    Ok(())
}

fn open_directory(path: &Path) -> Result<Handle> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // Deliberately omit FILE_SHARE_DELETE while every ancestor is validated.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Handle(handle))
}

/// Validate the exact nofollow file opened by Broker::load, rather than reopening
/// the registry pathname between its security check and JSON read.
pub fn validate_registry_handle(file: &std::fs::File, path: &Path) -> Result<()> {
    validate_registry_path(path)?;
    let path = registry_path(path)?;
    let current = current_sid()?;
    let mut ancestors: Vec<_> = path
        .parent()
        .context("registry parent directory required")?
        .ancestors()
        .collect();
    ancestors.reverse();
    let mut held = Vec::with_capacity(ancestors.len());
    for parent in ancestors {
        let handle = open_directory(parent)
            .with_context(|| format!("open broker registry ancestor {}", parent.display()))?;
        validate_handle(handle.0, &current, true)
            .with_context(|| format!("insecure broker registry ancestor {}", parent.display()))?;
        held.push(handle);
    }
    validate_handle(file.as_raw_handle().cast(), &current, false)
}

pub fn validate_registry(path: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    validate_registry_path(path)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    validate_registry_handle(&file, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, GetSecurityDescriptorOwner};

    fn check(sddl: &str, directory: bool) -> Result<()> {
        let text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let _allocated = LocalAllocation(descriptor);
        let mut owner = std::ptr::null_mut();
        let mut owner_defaulted = 0;
        let mut dacl = std::ptr::null_mut();
        let mut present = 0;
        let mut dacl_defaulted = 0;
        if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut owner_defaulted) } == 0
            || unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut dacl_defaulted)
            } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if present == 0 {
            bail!("DACL missing");
        }
        validate_acl(owner, dacl, "S-1-5-21-1-2-3-1001", directory)
    }

    #[test]
    fn registry_dacl_allows_readers_but_only_trusted_writers() {
        assert!(check("O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FR;;;WD)", false).is_ok());
        assert!(check(
            "O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;S-1-5-21-1-2-3-1001)",
            false
        )
        .is_ok());
        assert!(check("O:SYD:P(A;;FW;;;WD)(A;;FA;;;SY)", false).is_err());
        assert!(check("O:SYD:P(A;;WD;;;AU)(A;;FA;;;SY)", false).is_err());
        assert!(check("O:SYD:P(A;;WO;;;AU)(A;;FA;;;SY)", false).is_err());
        assert!(check("O:SYD:P(A;;SD;;;AU)(A;;FA;;;SY)", false).is_err());
        assert!(check("O:SYD:NO_ACCESS_CONTROL", false).is_err());
        assert!(check("O:WDD:P(A;;FA;;;SY)", false).is_err());
    }
    #[test]
    fn registry_parent_acl_rejects_replacement_but_allows_new_siblings() {
        assert!(check("O:SYD:P(A;;0x00000004;;;AU)(A;;FA;;;SY)", true).is_ok());
        assert!(check("O:SYD:P(A;;0x00000040;;;AU)(A;;FA;;;SY)", true).is_err());
        assert!(check("O:SYD:P(A;;SD;;;AU)(A;;FA;;;SY)", true).is_err());
        assert!(check("O:SYD:P(A;OIIO;FA;;;CO)(A;;FA;;;SY)", true).is_ok());
        assert!(check(&format!("O:{TRUSTED_INSTALLER_SID}D:P(A;;FA;;;SY)"), true).is_ok());
        assert!(check("O:S-1-5-80-999D:P(A;;FA;;;SY)", true).is_err());
        assert!(check(
            &format!("O:{TRUSTED_INSTALLER_SID}D:P(A;;SD;;;AU)(A;;FA;;;SY)"),
            true
        )
        .is_err());
    }
    #[test]
    fn broker_pipe_rights_exclude_instance_creation_and_acl_mutation() {
        let descriptor = pipe_descriptor("S-1-5-21-1-2-3-1001", "S-1-5-18");
        assert!(descriptor.contains("(A;;0x120183;;;S-1-5-18)"));
        assert_eq!(0x120183u32 & (0x4 | WRITE_DAC | WRITE_OWNER), 0);
    }
    #[test]
    fn registry_paths_reject_remote_device_and_traversal() {
        assert!(registry_path(Path::new(r"C:\ProgramData\ControlPlane\registry.json")).is_ok());
        for invalid in [
            r"\\remote\share\registry.json",
            r"\\?\UNC\remote\share\registry.json",
            r"\\.\pipe\registry",
            r"C:\safe\..\registry.json",
        ] {
            assert!(registry_path(Path::new(invalid)).is_err());
        }
    }
}
pub struct ServerPipe(pub NamedPipeServer);
impl tonic::transport::server::Connected for ServerPipe {
    type ConnectInfo = ();
    fn connect_info(&self) {}
}
impl AsyncRead for ServerPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}
impl AsyncWrite for ServerPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
pub struct PipeListener {
    name: String,
    broker_sid: String,
    next: Option<NamedPipeServer>,
}
impl PipeListener {
    pub fn bind(name: String, broker_sid: String) -> Result<Self> {
        let next = create(&name, &broker_sid, true)?;
        Ok(Self {
            name,
            broker_sid,
            next: Some(next),
        })
    }
}
impl axum::serve::Listener for PipeListener {
    type Io = ServerPipe;
    type Addr = String;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if self.next.is_none() {
                match create(&self.name, &self.broker_sid, false) {
                    Ok(server) => self.next = Some(server),
                    Err(error) => {
                        tracing::warn!(%error,"worker pipe creation failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                }
            }
            let server = self.next.take().unwrap();
            let connected = server.connect().await;
            // Retain this instance even after a connection error until its
            // replacement exists. Dropping it first could free the pipe name
            // and let another identity squat on it during a retry.
            loop {
                match create(&self.name, &self.broker_sid, false) {
                    Ok(next) => {
                        self.next = Some(next);
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error,"worker pipe replacement failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            if let Err(error) = connected {
                tracing::debug!(%error,"worker pipe connection failed");
                continue;
            }
            return (ServerPipe(server), self.name.clone());
        }
    }
    fn local_addr(&self) -> std::io::Result<String> {
        Ok(self.name.clone())
    }
}

/// Validate directory handles with reparse points disabled, holding all ancestors
/// while checking that other OS accounts cannot replace the private directory.
pub fn validate_private_directory(path: &Path) -> Result<()> {
    validate_registry_path(path)?;
    let path = registry_path(path)?;
    let current = current_sid()?;
    let mut parents = path.ancestors().collect::<Vec<_>>();
    parents.reverse();
    let mut held = Vec::new();
    for parent in parents {
        let handle = open_directory(parent)?;
        validate_handle(handle.0, &current, true)?;
        held.push(handle);
    }
    Ok(())
}
pub fn create_private_directory(path: &Path) -> Result<()> {
    let owner = current_sid()?;
    let sddl: Vec<u16> = format!("D:P(A;OICI;FA;;;{owner})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let _allocated = LocalAllocation(descriptor);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if name[..name.len() - 1].contains(&0) {
        bail!("invalid private directory path");
    }
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(name.as_ptr(), &attributes)
    } == 0
    {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(183) {
            return Err(error.into());
        }
    }
    validate_private_directory(path)?;
    let handle = open_directory(path)?;
    let mut object_owner = std::ptr::null_mut();
    let mut dacl = std::ptr::null_mut();
    let mut security = std::ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut object_owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut security,
        )
    };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result as i32).into());
    }
    let _security = LocalAllocation(security);
    if dacl.is_null() || unsafe { IsValidAcl(dacl) } == 0 {
        bail!("private directory requires a valid DACL");
    }
    let acl = unsafe { &*dacl };
    let start = dacl as usize;
    let end = start + usize::from(acl.AclSize);
    for index in 0..u32::from(acl.AceCount) {
        let mut raw = std::ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut raw) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let address = raw as usize;
        if address < start || address + std::mem::size_of::<ACE_HEADER>() > end {
            bail!("invalid private directory ACE");
        }
        let header = unsafe { std::ptr::read_unaligned(raw.cast::<ACE_HEADER>()) };
        let size = usize::from(header.AceSize);
        if size < 4 || address + size > end {
            bail!("invalid private directory ACE size");
        }
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || header.AceType == 1 {
            continue;
        }
        if header.AceType != 0 || size < 16 {
            bail!("unsupported private directory ACE");
        }
        let bytes = raw.cast::<u8>();
        let mask = unsafe { std::ptr::read_unaligned(bytes.add(4).cast::<u32>()) };
        if mask == 0 {
            continue;
        }
        let sid = unsafe { bytes.add(8) };
        let count = usize::from(unsafe { *sid.add(1) });
        if count > 15 || 16 + 4 * count > size {
            bail!("invalid private directory trustee");
        }
        if !trusted_sid(&sid_string(sid.cast())?, &owner) {
            bail!("private directory grants another OS identity access");
        }
    }
    Ok(())
}
