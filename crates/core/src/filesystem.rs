//! Shared filesystem primitives with no domain-specific policy.

use std::fs::Metadata;
use std::path::{Path, PathBuf};

pub fn is_link_or_reparse(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub fn is_safe_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub fn operation_sibling(path: &Path, kind: &str, operation_id: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{kind}-{operation_id}"))
}

#[cfg(unix)]
pub fn same_file(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
pub fn open_reparse_point(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000)
        .open(path)
}

#[cfg(windows)]
pub fn file_identity(file: &std::fs::File) -> std::io::Result<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation: FileTime,
        last_access: FileTime,
        last_write: FileTime,
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetFileInformationByHandle"]
        fn get_file_information_by_handle(
            file: *mut std::ffi::c_void,
            information: *mut FileInformation,
        ) -> i32;
    }

    let mut information = FileInformation::default();
    // SAFETY: `file` owns a valid handle and the output structure has Win32's
    // BY_HANDLE_FILE_INFORMATION layout.
    let result = unsafe {
        get_file_information_by_handle(file.as_raw_handle(), std::ptr::addr_of_mut!(information))
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        information.volume_serial,
        (u64::from(information.index_high) << 32) | u64::from(information.index_low),
    ))
}
