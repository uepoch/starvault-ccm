//! Crash-safe replacement for small JSON, TOML, and manifest files.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{user_path_err, Result};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        user_path_err(
            "invalid_output_path",
            "output path has no parent directory",
            path,
            false,
        )
    })?;
    ensure_real_parent(parent)?;
    let (temporary, mut file) = create_temporary(path)?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)
            .map_err(|error| io_at("write_temporary_file", &temporary, error))?;
        file.flush()
            .map_err(|error| io_at("flush_temporary_file", &temporary, error))?;
        file.sync_all()
            .map_err(|error| io_at("sync_temporary_file", &temporary, error))?;
        drop(file);
        replace(&temporary, path).map_err(|error| io_at("replace_file", path, error))?;
        sync_parent(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn ensure_real_parent(parent: &Path) -> Result<()> {
    validate_existing_ancestors(parent)?;
    std::fs::create_dir_all(parent)?;
    validate_existing_ancestors(parent)
}

fn validate_existing_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        let metadata = match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_at("inspect_output_directory", ancestor, error)),
        };
        if !metadata.is_dir() || is_link_or_reparse_point(&metadata) {
            return Err(user_path_err(
                "unsafe_output_directory",
                "refusing to write through a linked or non-directory output path",
                ancestor,
                false,
            ));
        }
    }
    Ok(())
}

fn create_temporary(path: &Path) -> Result<(PathBuf, std::fs::File)> {
    for _ in 0..32 {
        let temporary = temporary_path(path);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_at("create_temporary_file", &temporary, error)),
        }
    }
    Err(user_path_err(
        "temporary_file_collisions",
        "could not reserve a unique temporary output file",
        path,
        true,
    ))
}

fn temporary_path(path: &Path) -> PathBuf {
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp-{}-{sequence}", std::process::id()))
}

fn io_at(code: &str, path: &Path, error: std::io::Error) -> crate::Error {
    user_path_err(code, error.to_string(), path, is_retryable(&error))
}

fn is_retryable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
    )
}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(not(windows))]
fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_at("sync_parent_directory", parent, error))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_content_without_leaving_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        write(&path, b"one").unwrap();
        write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_linked_ancestor_before_creating_external_directories() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let linked = directory.path().join("linked");
        symlink(external.path(), &linked).unwrap();

        let output = linked.join("not-created/config.json");
        let error = write(&output, b"secret").unwrap_err();

        assert_eq!(error.code(), "unsafe_output_directory");
        assert!(!external.path().join("not-created").exists());
    }
}
