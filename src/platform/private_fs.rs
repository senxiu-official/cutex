//! Owner-private Windows filesystem primitives for durable service state.
//!
//! Windows does not provide `openat(2)`. These operations combine an
//! owner-only protected DACL with reparse-point rejection, handle identity
//! checks before and after pathname operations, and write-through replacement.
//! Once a root has been secured, identities other than the service SID cannot
//! rename entries within it, which closes the pathname race available to an
//! untrusted local identity.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, AddAccessAllowedAceEx, CreateWellKnownSid, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl, IsValidSid, TokenGroups, TokenUser, TokenOwner, TOKEN_GROUPS, TOKEN_OWNER, TOKEN_INFORMATION_CLASS,
    WinBuiltinAdministratorsSid, SECURITY_MAX_SID_SIZE,
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, FileAttributeTagInfo, FileIdInfo, FlushFileBuffers,
    GetFileInformationByHandleEx, MoveFileExW, CREATE_NEW, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL,
    WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub(crate) volume: u64,
    pub(crate) file_id: [u8; 16],
}

impl FileIdentity {
    /// A 128-bit digest of the volume and full file ID for portable publication receipts.
    /// The legacy receipt's device/inode fields carry this digest on Windows.
    pub fn publication_key(self) -> (u64, u64) {
        use sha2::{Digest, Sha256};
        let digest = Sha256::new().chain_update(self.volume.to_le_bytes())
            .chain_update(self.file_id).finalize();
        (u64::from_le_bytes(digest[..8].try_into().unwrap()),
         u64::from_le_bytes(digest[8..16].try_into().unwrap()))
    }
}

#[derive(Debug)]
pub enum PrivateFsError {
    Io(io::Error),
    InvalidName,
    ReparsePoint,
    WrongType,
    OwnerMismatch,
    DaclNotPrivate,
    BindingChanged,
}

impl std::fmt::Display for PrivateFsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::InvalidName => formatter.write_str("invalid private child name"),
            Self::ReparsePoint => formatter.write_str("reparse points are not allowed"),
            Self::WrongType => formatter.write_str("unexpected filesystem object type"),
            Self::OwnerMismatch => formatter.write_str("object is not owned by the service SID"),
            Self::DaclNotPrivate => formatter.write_str("object DACL is not service-private"),
            Self::BindingChanged => formatter.write_str("private root binding changed"),
        }
    }
}

impl std::error::Error for PrivateFsError {}

impl From<io::Error> for PrivateFsError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl PrivateFsError {
    pub fn io_kind(&self) -> io::ErrorKind {
        match self {
            Self::Io(error) => error.kind(),
            Self::InvalidName => io::ErrorKind::InvalidInput,
            Self::BindingChanged => io::ErrorKind::NotFound,
            _ => io::ErrorKind::PermissionDenied,
        }
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn open_path(
    path: &Path,
    desired_access: u32,
    disposition: u32,
    directory: bool,
    share_delete: bool,
) -> Result<File, PrivateFsError> {
    let path = wide(path.as_os_str());
    let mut share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    if share_delete {
        share |= FILE_SHARE_DELETE;
    }
    let mut attributes = FILE_FLAG_OPEN_REPARSE_POINT;
    if directory {
        attributes |= FILE_FLAG_BACKUP_SEMANTICS;
    } else {
        attributes |= FILE_ATTRIBUTE_NORMAL;
    }
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            desired_access,
            share,
            null(),
            disposition,
            attributes,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn attributes(file: &File) -> Result<FILE_ATTRIBUTE_TAG_INFO, PrivateFsError> {
    let mut info: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(info)
}

fn validate_type(file: &File, directory: bool) -> Result<(), PrivateFsError> {
    let info = attributes(file)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(PrivateFsError::ReparsePoint);
    }
    if (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory {
        return Err(PrivateFsError::WrongType);
    }
    Ok(())
}

pub fn identity(file: &File) -> Result<FileIdentity, PrivateFsError> {
    let mut info: FILE_ID_INFO = unsafe { zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(FileIdentity {
        volume: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

fn current_user_sid() -> Result<Vec<u8>, PrivateFsError> {
    token_sid(TokenUser)
}

/// The `GetTokenInformation` buffer of the current process token (u64
/// storage keeps the structures it holds pointer-aligned).
fn token_information(class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u64>, PrivateFsError> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let token = OwnedHandle(token);
    let mut required = 0;
    unsafe {
        GetTokenInformation(token.0, class, null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut buffer = vec![0u64; (required as usize).div_ceil(8)];
    let size = (buffer.len() * 8) as u32;
    if unsafe { GetTokenInformation(token.0, class, buffer.as_mut_ptr().cast(), size, &mut required) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(buffer)
}

fn administrators_sid() -> Result<Vec<u8>, PrivateFsError> {
    let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut length = SECURITY_MAX_SID_SIZE;
    if unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, null_mut(), sid.as_mut_ptr().cast(), &mut length) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    sid.truncate(length as usize);
    Ok(sid)
}

/// The token lists `sid` as a group, enabled or deny-only (a non-elevated
/// administrator's filtered token keeps Administrators as deny-only).
fn token_has_group(sid: &[u8]) -> Result<bool, PrivateFsError> {
    let buffer = token_information(TokenGroups)?;
    let groups = buffer.as_ptr().cast::<TOKEN_GROUPS>();
    let count = unsafe { (*groups).GroupCount } as usize;
    let first = unsafe { std::ptr::addr_of!((*groups).Groups).cast::<windows_sys::Win32::Security::SID_AND_ATTRIBUTES>() };
    for index in 0..count {
        let group = unsafe { *first.add(index) };
        if !group.Sid.is_null() && unsafe { EqualSid(group.Sid, sid.as_ptr() as PSID) } != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn token_sid(class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u8>, PrivateFsError> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let token = OwnedHandle(token);
    let mut required = 0;
    unsafe {
        GetTokenInformation(token.0, class, null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut buffer = vec![0u8; required as usize];
    if unsafe {
        GetTokenInformation(
            token.0,
            class,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let sid = unsafe {
        if class == TokenOwner { (*(buffer.as_ptr().cast::<TOKEN_OWNER>())).Owner }
        else { (*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    };
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(PrivateFsError::OwnerMismatch);
    }
    let length = unsafe { GetLengthSid(sid) } as usize;
    let mut owned = vec![0u8; length];
    unsafe {
        std::ptr::copy_nonoverlapping(sid.cast::<u8>(), owned.as_mut_ptr(), length);
    }
    Ok(owned)
}

struct SecurityDescriptor(*mut std::ffi::c_void);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0);
        }
    }
}

fn security_info(file: &File) -> Result<(PSID, *mut ACL, SecurityDescriptor), PrivateFsError> {
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32).into());
    }
    Ok((owner, dacl, SecurityDescriptor(descriptor)))
}

fn validate_owner(file: &File) -> Result<(), PrivateFsError> {
    let (owner, _, _descriptor) = security_info(file)?;
    validate_token_owner(owner)
}

fn validate_token_owner(owner: PSID) -> Result<(), PrivateFsError> {
    use super::owner_policy::{owner_accepted, OwnerFacts};
    if owner.is_null() { return Err(PrivateFsError::OwnerMismatch); }
    let user = current_user_sid()?;
    let mut facts = OwnerFacts { is_user: unsafe { EqualSid(owner, user.as_ptr() as PSID) } != 0, ..OwnerFacts::default() };
    if owner_accepted(facts) { return Ok(()); }
    // Elevated Windows tokens create objects owned by their default owner
    // (usually Administrators). Keep accepting existing user-owned objects
    // across elevation; the protected DACL is always restricted to TokenUser.
    let default_owner = token_sid(TokenOwner)?;
    facts.is_default_owner = unsafe { EqualSid(owner, default_owner.as_ptr() as PSID) } != 0;
    if owner_accepted(facts) { return Ok(()); }
    // Left behind by an elevated process of this administrator account
    // (owner_policy.rs); re-owned to the user by secure_owned_handle.
    let administrators = administrators_sid()?;
    facts.is_administrators = unsafe { EqualSid(owner, administrators.as_ptr() as PSID) } != 0;
    if facts.is_administrators {
        facts.user_in_administrators = token_has_group(&administrators)?;
    }
    if owner_accepted(facts) { Ok(()) } else { Err(PrivateFsError::OwnerMismatch) }
}

fn private_acl(sid: PSID, directory: bool) -> Result<Vec<u8>, PrivateFsError> {
    let sid_length = unsafe { GetLengthSid(sid) } as usize;
    let length = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_length;
    let mut acl = vec![0u8; length];
    let acl_pointer = acl.as_mut_ptr().cast::<ACL>();
    if unsafe { InitializeAcl(acl_pointer, length as u32, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let inheritance = if directory {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    if unsafe { AddAccessAllowedAceEx(acl_pointer, ACL_REVISION, inheritance, GENERIC_ALL, sid) }
        == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(acl)
}

fn secure_owned_handle(file: &File, directory: bool) -> Result<(), PrivateFsError> {
    let sid = current_user_sid()?;
    let sid_pointer = sid.as_ptr() as PSID;
    validate_owner(file)?;
    // TokenOwner may be Administrators under SSH/elevation. Persist TokenUser
    // instead, so the same account can reopen the object from a normal terminal.
    let acl = private_acl(sid_pointer, directory)?;
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            sid_pointer,
            null_mut(),
            acl.as_ptr().cast(),
            null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32).into());
    }
    Ok(())
}

fn validate_private_acl(file: &File, directory: bool) -> Result<(), PrivateFsError> {
    let sid = current_user_sid()?;
    let sid_pointer = sid.as_ptr() as PSID;
    let (owner, dacl, descriptor) = security_info(file)?;
    validate_token_owner(owner)?;
    if dacl.is_null() {
        return Err(PrivateFsError::DaclNotPrivate);
    }
    let mut control = 0;
    let mut revision = 0;
    let control_ok =
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) };
    if control_ok == 0 || control & SE_DACL_PROTECTED == 0 {
        return Err(PrivateFsError::DaclNotPrivate);
    }
    let mut size: ACL_SIZE_INFORMATION = unsafe { zeroed() };
    let acl_info_ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if acl_info_ok == 0 || size.AceCount == 0 {
        return Err(PrivateFsError::DaclNotPrivate);
    }
    let mut effective_full_access = false;
    let mut inheritable_full_access = !directory;
    for index in 0..size.AceCount {
        let mut ace = null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
            return Err(PrivateFsError::DaclNotPrivate);
        }
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        let header = unsafe { (*ace).Header };
        let mask = unsafe { (*ace).Mask };
        let ace_sid = unsafe { (&mut (*ace).SidStart as *mut u32).cast() };
        if header.AceType != 0 || unsafe { EqualSid(ace_sid, sid_pointer) } == 0 {
            return Err(PrivateFsError::DaclNotPrivate);
        }
        let flags = u32::from(header.AceFlags);
        let full_access = mask == GENERIC_ALL || mask == FILE_ALL_ACCESS;
        effective_full_access |= full_access && flags & INHERIT_ONLY_ACE == 0;
        inheritable_full_access |=
            full_access && flags & OBJECT_INHERIT_ACE != 0 && flags & CONTAINER_INHERIT_ACE != 0;
    }
    if !effective_full_access || !inheritable_full_access {
        return Err(PrivateFsError::DaclNotPrivate);
    }
    Ok(())
}

pub fn secure_directory(path: &Path) -> Result<(File, FileIdentity), PrivateFsError> {
    let file = open_path(
        path,
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        OPEN_EXISTING,
        true,
        true,
    )?;
    validate_type(&file, true)?;
    secure_owned_handle(&file, true)?;
    validate_private_acl(&file, true)?;
    let identity = identity(&file)?;
    Ok((file, identity))
}

pub fn open_validated_directory(path: &Path) -> Result<(File, FileIdentity), PrivateFsError> {
    let file = open_path(
        path,
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
        OPEN_EXISTING,
        true,
        true,
    )?;
    validate_type(&file, true)?;
    validate_private_acl(&file, true)?;
    let identity = identity(&file)?;
    Ok((file, identity))
}

pub fn validate_binding(path: &Path, expected: FileIdentity) -> Result<(), PrivateFsError> {
    let (_, actual) = open_validated_directory(path).map_err(|_| PrivateFsError::BindingChanged)?;
    if actual != expected {
        return Err(PrivateFsError::BindingChanged);
    }
    Ok(())
}

pub fn validate_private_file(file: &File) -> Result<(), PrivateFsError> {
    validate_type(file, false)?;
    validate_private_acl(file, false)
}

fn validate_name(name: &str) -> Result<(), PrivateFsError> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(PrivateFsError::InvalidName);
    }
    Ok(())
}

pub fn open_child(
    root_path: &Path,
    root_identity: FileIdentity,
    name: &str,
    flags: i32,
    share_delete: bool,
) -> Result<File, PrivateFsError> {
    validate_name(name)?;
    validate_binding(root_path, root_identity)?;
    let access_mode = flags & 3;
    let mut access = READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES;
    if access_mode == libc::O_RDONLY || access_mode == libc::O_RDWR {
        access |= GENERIC_READ;
    }
    if access_mode == libc::O_WRONLY || access_mode == libc::O_RDWR {
        access |= GENERIC_WRITE;
    }
    let disposition = if flags & libc::O_CREAT != 0 && flags & libc::O_EXCL != 0 {
        CREATE_NEW
    } else if flags & libc::O_CREAT != 0 {
        OPEN_ALWAYS
    } else {
        OPEN_EXISTING
    };
    let file = open_path(
        &root_path.join(name),
        access,
        disposition,
        false,
        share_delete,
    )?;
    validate_type(&file, false)?;
    secure_owned_handle(&file, false)?;
    validate_private_acl(&file, false)?;
    if validate_binding(root_path, root_identity).is_err() {
        drop(file);
        return Err(PrivateFsError::BindingChanged);
    }
    Ok(file)
}

pub fn replace_child(
    root_path: &Path,
    root_identity: FileIdentity,
    source: &str,
    target: &str,
) -> Result<(), PrivateFsError> {
    validate_name(source)?;
    validate_name(target)?;
    validate_binding(root_path, root_identity)?;
    let source_file = open_child(root_path, root_identity, source, libc::O_RDONLY, true)?;
    drop(source_file);
    match open_child(root_path, root_identity, target, libc::O_RDONLY, true) {
        Ok(file) => drop(file),
        Err(PrivateFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let source = wide(root_path.join(source).as_os_str());
    let target = wide(root_path.join(target).as_os_str());
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    validate_binding(root_path, root_identity)
}

pub fn unlink_child(
    root_path: &Path,
    root_identity: FileIdentity,
    name: &str,
) -> Result<(), PrivateFsError> {
    validate_name(name)?;
    validate_binding(root_path, root_identity)?;
    let file = match open_child(root_path, root_identity, name, libc::O_RDONLY, true) {
        Ok(file) => file,
        Err(PrivateFsError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    drop(file);
    let path = wide(root_path.join(name).as_os_str());
    if unsafe { DeleteFileW(path.as_ptr()) } == 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    validate_binding(root_path, root_identity)
}

pub fn sync_directory(file: &File) -> Result<(), PrivateFsError> {
    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn open_private_file_path(
    path: &Path,
    create: bool,
    share_delete: bool,
) -> Result<File, PrivateFsError> {
    let disposition = if create { OPEN_ALWAYS } else { OPEN_EXISTING };
    let file = open_path(
        path,
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        disposition,
        false,
        share_delete,
    )?;
    validate_type(&file, false)?;
    secure_owned_handle(&file, false)?;
    validate_private_acl(&file, false)?;
    Ok(file)
}

pub fn secure_tree(path: &Path) -> Result<(), PrivateFsError> {
    let (_, root_identity) = secure_directory(path)?;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| PrivateFsError::InvalidName)?;
        validate_name(&name)?;
        let child_path = entry.path();
        let probe = open_path(
            &child_path,
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
            OPEN_EXISTING,
            true,
            true,
        );
        match probe {
            Ok(handle) => match validate_type(&handle, true) {
                Ok(()) => {
                    drop(handle);
                    secure_tree(&child_path)?;
                }
                Err(PrivateFsError::WrongType) => {
                    drop(handle);
                    let file = open_child(path, root_identity, &name, libc::O_RDWR, true)?;
                    drop(file);
                }
                Err(error) => return Err(error),
            },
            Err(_) => {
                let file = open_child(path, root_identity, &name, libc::O_RDWR, true)?;
                drop(file);
            }
        }
    }
    validate_binding(path, root_identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::io::{Read, Write};
    use std::process::Command;

    struct TestRoot(PathBuf);

    use std::path::PathBuf;

    impl TestRoot {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("cutex-private-fs-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn secured_objects_are_owned_by_user_not_elevated_default_owner() {
        let fixture = TestRoot::new("stable-owner");
        let (directory, root_identity) = secure_directory(&fixture.0).unwrap();
        let child = open_child(&fixture.0, root_identity, "child.json",
            libc::O_RDWR | libc::O_CREAT, true).unwrap();
        let standalone = open_private_file_path(&fixture.0.join("standalone.json"), true, true).unwrap();
        let user = current_user_sid().unwrap();
        for file in [&directory, &child, &standalone] {
            let (owner, _, _descriptor) = security_info(file).unwrap();
            assert_ne!(unsafe { EqualSid(owner, user.as_ptr() as PSID) }, 0);
        }
        open_validated_directory(&fixture.0).unwrap();
        validate_private_file(&child).unwrap();
    }

    #[test]
    fn secures_owner_private_root_and_file() {
        let fixture = TestRoot::new("dacl");
        assert!(open_validated_directory(&fixture.0).is_err());
        let (_, root_identity) = secure_directory(&fixture.0).unwrap();
        let file = open_child(
            &fixture.0,
            root_identity,
            "private.lock",
            libc::O_RDWR | libc::O_CREAT,
            false,
        )
        .unwrap();
        validate_private_file(&file).unwrap();
        open_validated_directory(&fixture.0).unwrap();
    }

    #[test]
    fn rejects_junction_root() {
        let fixture = TestRoot::new("junction");
        let target = fixture.0.join("target");
        let junction = fixture.0.join("junction");
        std::fs::create_dir(&target).unwrap();
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(output.status.success(), "mklink /J failed: {output:?}");
        assert!(matches!(
            secure_directory(&junction),
            Err(PrivateFsError::ReparsePoint)
        ));
        std::fs::remove_dir(&junction).unwrap();
    }

    #[test]
    fn detects_root_replacement() {
        let fixture = TestRoot::new("binding");
        let root = fixture.0.join("root");
        let moved = fixture.0.join("moved");
        std::fs::create_dir(&root).unwrap();
        let (directory, original) = secure_directory(&root).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        secure_directory(&root).unwrap();
        assert!(matches!(
            validate_binding(&root, original),
            Err(PrivateFsError::BindingChanged)
        ));
        drop(directory);
    }

    #[test]
    fn write_through_replace_preserves_private_target() {
        let fixture = TestRoot::new("replace");
        let (directory, root_identity) = secure_directory(&fixture.0).unwrap();
        let mut old = open_child(
            &fixture.0,
            root_identity,
            "state.json",
            libc::O_RDWR | libc::O_CREAT,
            true,
        )
        .unwrap();
        old.write_all(b"old").unwrap();
        old.sync_all().unwrap();
        drop(old);
        let mut temp = open_child(
            &fixture.0,
            root_identity,
            "state.tmp",
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            true,
        )
        .unwrap();
        temp.write_all(b"new").unwrap();
        temp.sync_all().unwrap();
        drop(temp);
        replace_child(&fixture.0, root_identity, "state.tmp", "state.json").unwrap();
        sync_directory(&directory).unwrap();
        let mut target = open_child(
            &fixture.0,
            root_identity,
            "state.json",
            libc::O_RDONLY,
            true,
        )
        .unwrap();
        let mut bytes = Vec::new();
        target.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"new");
        assert!(!fixture.0.join("state.tmp").exists());
        validate_private_file(&target).unwrap();
    }

    #[test]
    fn lock_file_is_exclusive_and_not_delete_shared() {
        let fixture = TestRoot::new("lock");
        let (_, root_identity) = secure_directory(&fixture.0).unwrap();
        let first = open_child(
            &fixture.0,
            root_identity,
            "owner.lock",
            libc::O_RDWR | libc::O_CREAT,
            false,
        )
        .unwrap();
        let second =
            open_child(&fixture.0, root_identity, "owner.lock", libc::O_RDWR, false).unwrap();
        first.try_lock_exclusive().unwrap();
        assert!(second.try_lock_exclusive().is_err());
        assert!(unlink_child(&fixture.0, root_identity, "owner.lock").is_err());
        FileExt::unlock(&first).unwrap();
    }
}

/// Restrict a runtime process or job object to the account across elevation/session changes.
pub fn secure_runtime_object(handle: HANDLE) -> Result<(), PrivateFsError> {
    let sid = current_user_sid()?;
    let acl = private_acl(sid.as_ptr() as PSID, false)?;
    let status = unsafe { SetSecurityInfo(handle,
        windows_sys::Win32::Security::Authorization::SE_KERNEL_OBJECT,
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
        null_mut(), null_mut(), acl.as_ptr().cast(), null()) };
    if status != 0 { return Err(io::Error::from_raw_os_error(status as i32).into()); }
    Ok(())
}

/// Keep manually launched services controllable by the same user without UAC.
pub fn secure_current_runtime_process() -> Result<(), PrivateFsError> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, OpenProcess};
    let handle = unsafe { OpenProcess(READ_CONTROL | WRITE_DAC, 0, GetCurrentProcessId()) };
    if handle.is_null() { return Err(io::Error::last_os_error().into()); }
    let owned = OwnedHandle(handle);
    secure_runtime_object(owned.0)
}
