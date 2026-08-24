use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use super::{AppState, CommandResult};

const LOG_ROTATE_BYTES: u64 = 256 * 1024;
const LOG_MAX_READ_BYTES: u64 = 512 * 1024;
const LOG_GENERATIONS: usize = 2;
static LOG_MIN_LEVEL: AtomicU8 = AtomicU8::new(0);
static LOG_IO_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub time: String,
    #[serde(default = "default_level")]
    pub level: String,
    pub kind: String,
    pub detail: String,
}

#[derive(Debug)]
struct LogIoError {
    path: PathBuf,
    source: std::io::Error,
}

impl LogIoError {
    fn at(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self {
            path: path.into(),
            source,
        }
    }

    fn unsafe_path(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::at(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into()),
        )
    }

    fn retryable(&self) -> bool {
        matches!(
            self.source.kind(),
            std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
        )
    }
}

fn default_level() -> String {
    "info".into()
}

fn log_path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|directory| directory.join("log.jsonl"))
}

fn log_io_guard() -> MutexGuard<'static, ()> {
    LOG_IO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn with_log_io_lock<T>(
    operation: impl FnOnce() -> svccm_core::error::Result<T>,
) -> svccm_core::error::Result<T> {
    let _guard = log_io_guard();
    operation()
}

pub fn init_log_level(config_path: &Path) {
    let level = svccm_core::config::Config::load(config_path)
        .map(|config| config.log_level)
        .unwrap_or_else(|_| "info".into());
    set_log_level(&level);
}

pub fn set_log_level(level: &str) {
    LOG_MIN_LEVEL.store(level_rank(level), Ordering::Relaxed);
}

fn level_rank(level: &str) -> u8 {
    match level {
        "warn" => 1,
        "error" => 2,
        _ => 0,
    }
}

pub fn log_startup(app: &AppHandle) {
    log_op(
        app,
        "info",
        "startup",
        &format!("StarVault CCM v{}", env!("CARGO_PKG_VERSION")),
    );
}

pub(crate) fn log_op(app: &AppHandle, level: &str, kind: &str, detail: &str) {
    if level_rank(level) < LOG_MIN_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let Some(path) = log_path(app) else {
        return;
    };
    let entry = LogEntry {
        time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default()
            .to_string(),
        level: level.into(),
        kind: kind.into(),
        detail: detail.into(),
    };
    let _guard = log_io_guard();
    let _ = append_entry(&path, &entry);
}

fn append_entry(path: &Path, entry: &LogEntry) -> Result<(), LogIoError> {
    let parent = path.parent().ok_or_else(|| {
        LogIoError::unsafe_path(path, "operation log path has no parent directory")
    })?;
    ensure_real_directory(parent)?;
    validate_log_files(path)?;
    rotate_if_needed(path)?;

    let mut encoded = serde_json::to_vec(entry).map_err(|error| {
        LogIoError::at(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        )
    })?;
    encoded.push(b'\n');

    let mut file = open_verified_append(path)?;
    file.write_all(&encoded)
        .map_err(|error| LogIoError::at(path, error))
}

fn rotate_if_needed(path: &Path) -> Result<(), LogIoError> {
    validate_existing_parent(path)?;
    validate_log_files(path)?;
    let Some(metadata) = inspect_regular_file(path)? else {
        return Ok(());
    };
    if metadata.len() < LOG_ROTATE_BYTES {
        return Ok(());
    }

    let oldest = generation_path(path, LOG_GENERATIONS);
    if inspect_regular_file(&oldest)?.is_some() {
        std::fs::remove_file(&oldest).map_err(|error| LogIoError::at(&oldest, error))?;
    }
    for generation in (1..LOG_GENERATIONS).rev() {
        let source = generation_path(path, generation);
        let target = generation_path(path, generation + 1);
        if inspect_regular_file(&source)?.is_some() {
            std::fs::rename(&source, &target).map_err(|error| LogIoError::at(&source, error))?;
        }
    }
    let first = generation_path(path, 1);
    std::fs::rename(path, &first).map_err(|error| LogIoError::at(path, error))?;
    validate_log_files(path)
}

fn read_entries(path: &Path, limit: usize) -> Result<Vec<LogEntry>, LogIoError> {
    validate_existing_parent(path)?;
    validate_log_files(path)?;
    let Some(mut file) = open_verified_read(path)? else {
        return Ok(Vec::new());
    };
    let length = file
        .metadata()
        .map_err(|error| LogIoError::at(path, error))?
        .len();
    if length > LOG_MAX_READ_BYTES {
        return Err(LogIoError::at(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "operation log exceeds the safe read limit",
            ),
        ));
    }
    let mut encoded = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(LOG_MAX_READ_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|error| LogIoError::at(path, error))?;
    if encoded.len() as u64 > LOG_MAX_READ_BYTES {
        return Err(LogIoError::at(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "operation log grew beyond the safe read limit",
            ),
        ));
    }
    let text = String::from_utf8(encoded).map_err(|error| {
        LogIoError::at(
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, error),
        )
    })?;
    let mut entries: Vec<LogEntry> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit.min(5_000));
    Ok(entries)
}

fn clear_log_files(path: &Path) -> Result<(), LogIoError> {
    validate_existing_parent(path)?;
    let mut entries = Vec::new();
    for candidate in log_files(path) {
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if is_link_or_reparse(&metadata) => {
                entries.push((candidate, ClearKind::Link(metadata)));
            }
            Ok(metadata) if metadata.file_type().is_file() => {
                entries.push((candidate, ClearKind::File));
            }
            Ok(_) => {
                return Err(LogIoError::unsafe_path(
                    candidate,
                    "operation log entry must be a regular file or removable link",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LogIoError::at(candidate, error)),
        }
    }

    for (candidate, kind) in entries {
        match kind {
            ClearKind::File => std::fs::remove_file(&candidate),
            ClearKind::Link(metadata) => remove_link(&candidate, &metadata),
        }
        .map_err(|error| LogIoError::at(&candidate, error))?;
    }
    Ok(())
}

enum ClearKind {
    File,
    Link(Metadata),
}

fn ensure_real_directory(path: &Path) -> Result<(), LogIoError> {
    validate_existing_ancestors(path)?;
    std::fs::create_dir_all(path).map_err(|error| LogIoError::at(path, error))?;
    validate_existing_ancestors(path)?;
    validate_real_directory(path)
}

fn validate_existing_parent(path: &Path) -> Result<(), LogIoError> {
    let parent = path.parent().ok_or_else(|| {
        LogIoError::unsafe_path(path, "operation log path has no parent directory")
    })?;
    validate_existing_ancestors(parent)?;
    match std::fs::symlink_metadata(parent) {
        Ok(_) => validate_real_directory(parent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LogIoError::at(parent, error)),
    }
}

fn validate_existing_ancestors(path: &Path) -> Result<(), LogIoError> {
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(LogIoError::unsafe_path(
                    ancestor,
                    "operation log directory must not be a link, reparse point, or special file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LogIoError::at(ancestor, error)),
        }
    }
    Ok(())
}

fn validate_real_directory(path: &Path) -> Result<(), LogIoError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| LogIoError::at(path, error))?;
    if metadata.is_dir() && !is_link_or_reparse(&metadata) {
        Ok(())
    } else {
        Err(LogIoError::unsafe_path(
            path,
            "operation log directory must be a real directory",
        ))
    }
}

fn validate_log_files(path: &Path) -> Result<(), LogIoError> {
    for candidate in log_files(path) {
        let _ = inspect_regular_file(&candidate)?;
    }
    Ok(())
}

fn inspect_regular_file(path: &Path) -> Result<Option<Metadata>, LogIoError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !is_link_or_reparse(&metadata) => {
            Ok(Some(metadata))
        }
        Ok(_) => Err(LogIoError::unsafe_path(
            path,
            "operation log entry must be a real regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(LogIoError::at(path, error)),
    }
}

fn open_verified_append(path: &Path) -> Result<File, LogIoError> {
    let expected = inspect_regular_file(path)?;
    let expected_identity = capture_open_identity(path, expected.is_some())?;
    let mut options = OpenOptions::new();
    options.append(true);
    if expected.is_none() {
        options.create_new(true);
    }
    let file = options
        .open(path)
        .map_err(|error| LogIoError::at(path, error))?;
    verify_open_file(path, expected.as_ref(), &file, &expected_identity)?;
    Ok(file)
}

fn open_verified_read(path: &Path) -> Result<Option<File>, LogIoError> {
    let Some(expected) = inspect_regular_file(path)? else {
        return Ok(None);
    };
    let expected_identity = capture_open_identity(path, true)?;
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| LogIoError::at(path, error))?;
    verify_open_file(path, Some(&expected), &file, &expected_identity)?;
    Ok(Some(file))
}

#[cfg(windows)]
type OpenIdentity = Option<File>;

#[cfg(not(windows))]
#[derive(Debug, Clone, Copy)]
struct OpenIdentity;

#[cfg(windows)]
fn capture_open_identity(path: &Path, exists: bool) -> Result<OpenIdentity, LogIoError> {
    if !exists {
        return Ok(None);
    }
    let file = open_identity_file(path)?;
    validate_identity_handle(path, &file)?;
    let _ = inspect_regular_file(path)?.ok_or_else(|| {
        LogIoError::unsafe_path(
            path,
            "operation log entry changed while its identity was being captured",
        )
    })?;
    Ok(Some(file))
}

#[cfg(not(windows))]
fn capture_open_identity(_path: &Path, _exists: bool) -> Result<OpenIdentity, LogIoError> {
    Ok(OpenIdentity)
}

fn verify_open_file(
    path: &Path,
    expected: Option<&Metadata>,
    file: &File,
    expected_identity: &OpenIdentity,
) -> Result<(), LogIoError> {
    let opened = file
        .metadata()
        .map_err(|error| LogIoError::at(path, error))?;
    if !opened.file_type().is_file() || is_link_or_reparse(&opened) {
        return Err(LogIoError::unsafe_path(
            path,
            "operation log handle is not a regular file",
        ));
    }
    #[cfg(windows)]
    {
        let _ = expected;
        verify_open_file_windows(path, expected_identity.as_ref(), file)
    }
    #[cfg(not(windows))]
    {
        let _ = expected_identity;
        let current = inspect_regular_file(path)?.ok_or_else(|| {
            LogIoError::unsafe_path(
                path,
                "operation log entry changed while it was being opened",
            )
        })?;
        if !same_file(&current, &opened)
            || expected.is_some_and(|metadata| !same_file(metadata, &opened))
        {
            return Err(LogIoError::unsafe_path(
                path,
                "operation log entry changed while it was being opened",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn open_identity_file(path: &Path) -> Result<File, LogIoError> {
    svccm_core::filesystem::open_reparse_point(path).map_err(|error| LogIoError::at(path, error))
}

#[cfg(windows)]
fn validate_identity_handle(path: &Path, file: &File) -> Result<(), LogIoError> {
    let metadata = file
        .metadata()
        .map_err(|error| LogIoError::at(path, error))?;
    if metadata.file_type().is_file() && !is_link_or_reparse(&metadata) {
        Ok(())
    } else {
        Err(LogIoError::unsafe_path(
            path,
            "operation log identity handle is not a real regular file",
        ))
    }
}

#[cfg(windows)]
fn verify_open_file_windows(
    path: &Path,
    expected: Option<&File>,
    opened: &File,
) -> Result<(), LogIoError> {
    let _current_metadata = inspect_regular_file(path)?.ok_or_else(|| {
        LogIoError::unsafe_path(
            path,
            "operation log entry changed while it was being opened",
        )
    })?;
    let current = open_identity_file(path)?;
    validate_identity_handle(path, &current)?;
    let _ = inspect_regular_file(path)?.ok_or_else(|| {
        LogIoError::unsafe_path(
            path,
            "operation log entry changed while it was being opened",
        )
    })?;
    let expected_matches = match expected {
        Some(expected) => same_file_handles(path, expected, opened)?,
        None => true,
    };
    if !same_file_handles(path, opened, &current)? || !expected_matches {
        return Err(LogIoError::unsafe_path(
            path,
            "operation log entry changed while it was being opened",
        ));
    }
    Ok(())
}

fn generation_path(path: &Path, generation: usize) -> PathBuf {
    path.with_extension(format!("jsonl.{generation}"))
}

fn log_files(path: &Path) -> Vec<PathBuf> {
    std::iter::once(path.to_path_buf())
        .chain((1..=LOG_GENERATIONS).map(|generation| generation_path(path, generation)))
        .collect()
}

fn is_link_or_reparse(metadata: &Metadata) -> bool {
    svccm_core::filesystem::is_link_or_reparse(metadata)
}

#[cfg(unix)]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    svccm_core::filesystem::same_file(left, right)
}

#[cfg(windows)]
fn file_identity(path: &Path, file: &File) -> Result<(u32, u64), LogIoError> {
    svccm_core::filesystem::file_identity(file).map_err(|error| LogIoError::at(path, error))
}

#[cfg(windows)]
fn same_file_handles(path: &Path, left: &File, right: &File) -> Result<bool, LogIoError> {
    Ok(file_identity(path, left)? == file_identity(path, right)?)
}

#[cfg(not(any(unix, windows)))]
fn same_file(_left: &Metadata, _right: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn remove_link(path: &Path, _metadata: &Metadata) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(windows)]
fn remove_link(path: &Path, metadata: &Metadata) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    if metadata.file_type().is_symlink_dir() || metadata.is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(not(any(unix, windows)))]
fn remove_link(path: &Path, _metadata: &Metadata) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[tauri::command]
pub fn read_log(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    limit: usize,
) -> CommandResult<Vec<LogEntry>> {
    let path = state.app_data_path.join("log.jsonl");
    let result = {
        let _guard = log_io_guard();
        read_entries(&path, limit)
    };
    result.map_err(|error| {
        let retryable = error.retryable();
        super::error::report(
            &app,
            &state,
            "read_log",
            svccm_core::error::user_path_err(
                "read_operation_log",
                error.source.to_string(),
                error.path,
                retryable,
            ),
        )
    })
}

#[tauri::command]
pub fn clear_log(app: AppHandle, state: tauri::State<'_, AppState>) -> CommandResult<()> {
    let path = state.app_data_path.join("log.jsonl");
    let result = {
        let _guard = log_io_guard();
        clear_log_files(&path)
    };
    result.map_err(|error| {
        let retryable = error.retryable();
        super::error::report(
            &app,
            &state,
            "clear_log",
            svccm_core::error::user_path_err(
                "clear_operation_log",
                error.source.to_string(),
                error.path,
                retryable,
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(detail: &str) -> LogEntry {
        LogEntry {
            time: "1".into(),
            level: "info".into(),
            kind: "test".into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn append_read_and_rotation_use_regular_files() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("app-data/log.jsonl");

        append_entry(&path, &entry("first")).unwrap();
        assert_eq!(read_entries(&path, 10).unwrap()[0].detail, "first");

        std::fs::write(&path, vec![b'x'; LOG_ROTATE_BYTES as usize]).unwrap();
        append_entry(&path, &entry("after rotation")).unwrap();
        assert_eq!(read_entries(&path, 10).unwrap()[0].detail, "after rotation");
        assert_eq!(
            std::fs::metadata(generation_path(&path, 1)).unwrap().len(),
            LOG_ROTATE_BYTES
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_log_is_never_followed_and_clear_only_unlinks_it() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let app_data = temporary.path().join("app-data");
        let external = temporary.path().join("external-sentinel");
        let path = app_data.join("log.jsonl");
        std::fs::create_dir(&app_data).unwrap();
        std::fs::write(&external, b"keep").unwrap();
        symlink(&external, &path).unwrap();

        assert!(append_entry(&path, &entry("do not write")).is_err());
        assert!(read_entries(&path, 10).is_err());
        assert!(rotate_if_needed(&path).is_err());
        assert_eq!(std::fs::read(&external).unwrap(), b"keep");

        clear_log_files(&path).unwrap();
        assert!(path.symlink_metadata().is_err());
        assert_eq!(std::fs::read(&external).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn linked_generation_blocks_append_without_touching_external_data() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("log.jsonl");
        let external = temporary.path().join("external-sentinel");
        std::fs::write(&path, b"original log").unwrap();
        std::fs::write(&external, b"keep").unwrap();
        symlink(&external, generation_path(&path, 1)).unwrap();

        assert!(append_entry(&path, &entry("do not write")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original log");
        assert_eq!(std::fs::read(&external).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn linked_parent_is_rejected_without_touching_external_data() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let linked_parent = temporary.path().join("app-data");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        symlink(&external, &linked_parent).unwrap();
        let path = linked_parent.join("log.jsonl");

        assert!(append_entry(&path, &entry("do not write")).is_err());
        assert!(read_entries(&path, 10).is_err());
        assert!(clear_log_files(&path).is_err());
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert!(!external.join("log.jsonl").exists());
    }

    #[test]
    fn special_log_entry_is_rejected_and_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("log.jsonl");
        std::fs::create_dir(&path).unwrap();

        assert!(append_entry(&path, &entry("do not write")).is_err());
        assert!(read_entries(&path, 10).is_err());
        assert!(clear_log_files(&path).is_err());
        assert!(path.symlink_metadata().unwrap().is_dir());
    }

    #[test]
    fn oversized_regular_log_is_rejected_before_deserialization() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("log.jsonl");
        std::fs::write(&path, vec![b'x'; LOG_MAX_READ_BYTES as usize + 1]).unwrap();

        let error = read_entries(&path, 10).unwrap_err();

        assert_eq!(error.source.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            LOG_MAX_READ_BYTES + 1
        );
    }

    #[cfg(windows)]
    #[test]
    fn open_log_handle_rejects_a_regular_file_substitution() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("log.jsonl");
        let displaced = temporary.path().join("original.jsonl");
        std::fs::write(&path, b"original").unwrap();
        let expected = inspect_regular_file(&path).unwrap().unwrap();
        let expected_identity = capture_open_identity(&path, true).unwrap();
        let opened = OpenOptions::new().read(true).open(&path).unwrap();
        std::fs::rename(&path, &displaced).unwrap();
        std::fs::write(&path, b"replacement").unwrap();

        assert!(verify_open_file(&path, Some(&expected), &opened, &expected_identity).is_err());
        assert_eq!(std::fs::read(displaced).unwrap(), b"original");
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }

    #[cfg(windows)]
    #[test]
    fn junction_log_is_never_followed_and_clear_only_unlinks_it() {
        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let path = temporary.path().join("log.jsonl");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        junction::create(&external, &path).unwrap();

        assert!(append_entry(&path, &entry("do not write")).is_err());
        assert!(read_entries(&path, 10).is_err());
        assert!(rotate_if_needed(&path).is_err());
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");

        clear_log_files(&path).unwrap();
        assert!(path.symlink_metadata().is_err());
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
    }

    #[cfg(windows)]
    #[test]
    fn junction_parent_is_rejected_without_touching_external_data() {
        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let linked_parent = temporary.path().join("app-data");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        junction::create(&external, &linked_parent).unwrap();
        let path = linked_parent.join("log.jsonl");

        assert!(append_entry(&path, &entry("do not write")).is_err());
        assert!(read_entries(&path, 10).is_err());
        assert!(clear_log_files(&path).is_err());
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert!(!external.join("log.jsonl").exists());
    }
}
