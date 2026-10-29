//! Shared native assets remain live across isolated session homes.
use std::path::Path;

pub fn link_directory(source: &Path, destination: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(source.is_dir(), "shared asset source is not a directory");
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, destination)?;
    #[cfg(windows)]
    create_junction(source, destination)?;
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("shared asset directories are unsupported on this platform");
    Ok(())
}

#[cfg(windows)]
fn create_junction(source: &Path, destination: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;

    // A junction is a mount-point reparse record. Creating it in process
    // avoids launching PowerShell and never reparses user paths as commands.
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xa000_0003;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00a4;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            device: windows_sys::Win32::Foundation::HANDLE,
            code: u32,
            input: *const core::ffi::c_void,
            input_len: u32,
            output: *mut core::ffi::c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
    }

    let canonical = source.canonicalize()?;
    let source = canonical
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("shared asset path is not Unicode"))?;
    let (substitute, print) = if let Some(unc) = source.strip_prefix(r"\\?\UNC\") {
        (format!(r"\??\UNC\{unc}"), format!(r"\\{unc}"))
    } else if let Some(local) = source.strip_prefix(r"\\?\") {
        (format!(r"\??\{local}"), local.to_owned())
    } else {
        anyhow::bail!("canonical shared asset path is not a Windows verbatim path");
    };
    let substitute: Vec<u16> = std::ffi::OsStr::new(&substitute).encode_wide().collect();
    let print: Vec<u16> = std::ffi::OsStr::new(&print).encode_wide().collect();
    let substitute_bytes = u16::try_from(substitute.len() * 2)
        .map_err(|_| anyhow::anyhow!("shared asset path is too long"))?;
    let print_bytes = u16::try_from(print.len() * 2)
        .map_err(|_| anyhow::anyhow!("shared asset path is too long"))?;
    let print_offset = substitute_bytes
        .checked_add(2)
        .ok_or_else(|| anyhow::anyhow!("shared asset path is too long"))?;
    let mut path = Vec::with_capacity(substitute.len() + print.len() + 2);
    path.extend_from_slice(&substitute);
    path.push(0);
    path.extend_from_slice(&print);
    path.push(0);
    let path_bytes = path.len() * 2;
    let data_length = u16::try_from(8usize + path_bytes)
        .map_err(|_| anyhow::anyhow!("shared asset path is too long"))?;
    let mut buffer = Vec::with_capacity(16 + path_bytes);
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&data_length.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&substitute_bytes.to_le_bytes());
    buffer.extend_from_slice(&print_offset.to_le_bytes());
    buffer.extend_from_slice(&print_bytes.to_le_bytes());
    for unit in path {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }

    std::fs::create_dir(destination)?;
    let operation = (|| -> anyhow::Result<()> {
        let directory = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(destination)?;
        use std::os::windows::io::AsRawHandle;
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                directory.as_raw_handle(),
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                u32::try_from(buffer.len())?,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    })();
    if operation.is_err() {
        let _ = std::fs::remove_dir(destination);
    }
    operation.context("shared asset junction creation failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_directory_reads_updates_without_copying() {
        let root = std::env::temp_dir().join(format!("cutex-assets-{}", uuid::Uuid::new_v4()));
        let source = root.join("source with spaces");
        let link = root.join("linked assets");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "first").unwrap();
        link_directory(&source, &link).unwrap();
        assert_eq!(std::fs::read_to_string(link.join("SKILL.md")).unwrap(), "first");
        std::fs::write(source.join("SKILL.md"), "updated").unwrap();
        assert_eq!(std::fs::read_to_string(link.join("SKILL.md")).unwrap(), "updated");
        #[cfg(unix)]
        std::fs::remove_file(&link).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir(&link).unwrap();
        assert!(source.join("SKILL.md").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
