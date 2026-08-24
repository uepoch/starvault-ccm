//! Archive import: bounded extraction, preview, and cancellable progress.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{internal_err, pkg_err, user_path_err, EnvironmentError, Result};
use crate::filesystem::is_link_or_reparse;
use crate::identity::PackageId;
use crate::package::metadata::SlotGuessKind;
use crate::package::normalize::PackagePlan;

/// Production archive limits. Tests may pass smaller limits through
/// [`extract_archive_with`] without allocating multi-gigabyte fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveLimits {
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_path_bytes: usize,
    pub reserve_bytes: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            max_total_bytes: 8 * 1024 * 1024 * 1024,
            max_path_bytes: 512,
            reserve_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// No extraction or ingestion cancellation check is farther apart than this.
pub const CANCELLATION_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Stable import state vocabulary shared with the desktop adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImportOperationState {
    Analyzing,
    Ready,
    Ingesting,
    Cancelled,
    Failed,
    Completed,
}

/// Serializable view of one import operation. The app owns worker handles and
/// cancellation tokens; neither crosses the IPC boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportOperationSnapshot {
    pub op_id: String,
    pub state: ImportOperationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<ImportPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// What the user sees before confirming an import.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportPreview {
    pub suggested_id: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    pub desc: Option<String>,
    /// `unknown`, or one of `wol` / `hots` / `lotv` / `nco`.
    pub slot_guess: String,
    pub matched_pattern: Option<String>,
    pub warnings: Vec<String>,
    pub file_count: usize,
}

/// Progress for long import phases. Returning `false` from a callback cancels.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ImportProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub current_file: String,
}

#[derive(Debug)]
struct ArchiveEntry {
    index: usize,
    name: String,
    relative_path: PathBuf,
    comparison_path: String,
    is_dir: bool,
    declared_size: u64,
}

/// Extract a ZIP after checking its declared sizes and real free space.
///
/// `dest` must be absent or empty. Cancellation and every error remove it, so
/// retrying cannot inherit files from a failed attempt.
pub fn extract_archive(
    zip_path: &Path,
    dest: &Path,
    on_progress: impl FnMut(ImportProgress) -> bool,
) -> Result<bool> {
    extract_archive_with(
        zip_path,
        dest,
        ArchiveLimits::default(),
        available_space,
        on_progress,
    )
}

/// Configurable extraction entry point for deterministic limit and disk-space
/// tests. The space probe receives the nearest existing destination ancestor.
pub fn extract_archive_with(
    zip_path: &Path,
    dest: &Path,
    limits: ArchiveLimits,
    space_probe: impl FnOnce(&Path) -> Result<u64>,
    mut on_progress: impl FnMut(ImportProgress) -> bool,
) -> Result<bool> {
    let dest = prepare_destination(dest)?;
    let result = extract_archive_inner(zip_path, &dest, limits, space_probe, &mut on_progress);
    if !matches!(result, Ok(true)) {
        if let Err(cleanup) = cleanup_destination(&dest) {
            return match result {
                Err(primary) => Err(internal_err(
                    "import_scratch_cleanup_failed",
                    "StarVault could not safely clean up an incomplete import",
                    format!("import failed: {primary}; cleanup failed: {cleanup}"),
                )),
                Ok(false) => Err(cleanup),
                Ok(true) => unreachable!("successful imports are not cleaned up"),
            };
        }
    }
    result
}

fn extract_archive_inner(
    zip_path: &Path,
    dest: &Path,
    limits: ArchiveLimits,
    space_probe: impl FnOnce(&Path) -> Result<u64>,
    on_progress: &mut impl FnMut(ImportProgress) -> bool,
) -> Result<bool> {
    let file = std::fs::File::open(zip_path)
        .map_err(|error| user_path_err("open_archive", error.to_string(), zip_path, false))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| pkg_err("archive", format!("read ZIP: {error}")))?;
    let (entries, declared_total) = inspect_archive(&mut archive, limits)?;

    let required_space = declared_total
        .checked_add(limits.reserve_bytes)
        .ok_or_else(|| pkg_err("archive", "declared size plus reserve overflows u64"))?;
    let space_path = nearest_existing_ancestor(dest);
    if space_probe(space_path)? < required_space {
        return Err(EnvironmentError::InsufficientSpace {
            path: Some(space_path.to_path_buf()),
        }
        .into());
    }

    let files_total = entries.iter().filter(|entry| !entry.is_dir).count() as u64;
    let mut files_done = 0_u64;
    let mut actual_total = 0_u64;
    let mut buffer = vec![0_u8; CANCELLATION_CHUNK_BYTES];

    for planned in entries {
        let target = dest.join(&planned.relative_path);
        if planned.is_dir {
            ensure_scratch_directory(&target, true)?;
            continue;
        }

        if let Some(parent) = target.parent() {
            ensure_scratch_directory(parent, true)?;
        }
        let mut input = archive.by_index(planned.index).map_err(|error| {
            pkg_err(
                planned.comparison_path.clone(),
                format!("read ZIP entry: {error}"),
            )
        })?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| {
                user_path_err("create_archive_file", error.to_string(), &target, false)
            })?;
        let mut file_bytes = 0_u64;

        loop {
            if !on_progress(ImportProgress {
                files_done,
                files_total,
                current_file: planned.name.clone(),
            }) {
                return Ok(false);
            }
            let read = input.read(&mut buffer).map_err(|error| {
                pkg_err(
                    planned.comparison_path.clone(),
                    format!("decompress ZIP entry: {error}"),
                )
            })?;
            if read == 0 {
                break;
            }
            file_bytes = file_bytes
                .checked_add(read as u64)
                .ok_or_else(|| pkg_err(&planned.comparison_path, "file size overflows u64"))?;
            actual_total = actual_total
                .checked_add(read as u64)
                .ok_or_else(|| pkg_err("archive", "total extracted size overflows u64"))?;
            if file_bytes > planned.declared_size || file_bytes > limits.max_file_bytes {
                return Err(pkg_err(
                    &planned.comparison_path,
                    "extracted data exceeds the declared or allowed file size",
                ));
            }
            if actual_total > declared_total || actual_total > limits.max_total_bytes {
                return Err(pkg_err(
                    "archive",
                    "extracted data exceeds the declared or allowed total size",
                ));
            }
            output.write_all(&buffer[..read]).map_err(|error| {
                user_path_err("write_archive_file", error.to_string(), &target, false)
            })?;
        }
        if file_bytes != planned.declared_size {
            return Err(pkg_err(
                planned.comparison_path,
                format!(
                    "declared {} bytes but extracted {file_bytes}",
                    planned.declared_size
                ),
            ));
        }
        output.flush().map_err(|error| {
            user_path_err("flush_archive_file", error.to_string(), &target, false)
        })?;
        files_done += 1;
    }
    Ok(true)
}

fn inspect_archive(
    archive: &mut zip::ZipArchive<std::fs::File>,
    limits: ArchiveLimits,
) -> Result<(Vec<ArchiveEntry>, u64)> {
    if archive.len() > limits.max_entries {
        return Err(package_error(
            "archive_entry_limit",
            format!(
                "contains {} entries; the limit is {}",
                archive.len(),
                limits.max_entries
            ),
        ));
    }

    let mut entries = Vec::with_capacity(archive.len());
    let mut declared_total = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| pkg_err("archive", format!("read ZIP entry {index}: {error}")))?;
        let name = entry.name().to_string();
        let (relative_path, comparison_path) = safe_relative_path(&name, limits.max_path_bytes)?;
        let is_dir = entry.is_dir();
        let declared_size = if is_dir { 0 } else { entry.size() };
        if declared_size > limits.max_file_bytes {
            return Err(package_error(
                "archive_file_limit",
                format!(
                    "`{comparison_path}` declares {declared_size} bytes; the per-file limit is {}",
                    limits.max_file_bytes
                ),
            ));
        }
        declared_total = declared_total
            .checked_add(declared_size)
            .ok_or_else(|| pkg_err("archive", "declared total size overflows u64"))?;
        if declared_total > limits.max_total_bytes {
            return Err(package_error(
                "archive_total_limit",
                format!(
                    "declares {declared_total} bytes; the total limit is {}",
                    limits.max_total_bytes
                ),
            ));
        }
        entries.push(ArchiveEntry {
            index,
            name,
            relative_path,
            comparison_path,
            is_dir,
            declared_size,
        });
    }

    let mut sorted: Vec<&ArchiveEntry> = entries.iter().collect();
    sorted.sort_by(|left, right| left.comparison_path.cmp(&right.comparison_path));
    for pair in sorted.windows(2) {
        let right = pair[1];
        if pair[0].comparison_path == right.comparison_path {
            return Err(package_error(
                "archive_path_collision",
                "duplicate archive path under Windows case-insensitive rules",
            ));
        }
    }
    let kinds: BTreeMap<&str, bool> = entries
        .iter()
        .map(|entry| (entry.comparison_path.as_str(), entry.is_dir))
        .collect();
    for entry in &entries {
        let mut ancestor = entry.comparison_path.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once('/') {
            if kinds.get(parent).is_some_and(|is_dir| !is_dir) {
                return Err(package_error(
                    "archive_path_collision",
                    format!("file `{parent}` blocks `{}`", entry.comparison_path),
                ));
            }
            ancestor = parent;
        }
    }
    Ok((entries, declared_total))
}

fn safe_relative_path(name: &str, max_path_bytes: usize) -> Result<(PathBuf, String)> {
    if name.is_empty() || name.len() > max_path_bytes {
        return Err(package_error(
            "archive_path_limit",
            format!("archive path must contain 1 to {max_path_bytes} bytes"),
        ));
    }
    if name.starts_with(['/', '\\']) || name.contains(':') || name.contains('\0') {
        return Err(package_error(
            "unsafe_archive_path",
            "unsafe rooted or device path in archive",
        ));
    }

    let trimmed = name.trim_end_matches(['/', '\\']);
    let mut relative = PathBuf::new();
    let mut canonical = Vec::new();
    for segment in trimmed.split(['/', '\\']) {
        if !is_safe_package_path_segment(segment) {
            return Err(package_error(
                "unsafe_archive_path",
                "unsafe path segment in archive",
            ));
        }
        relative.push(segment);
        canonical.push(segment.to_ascii_lowercase());
    }
    if canonical.is_empty() {
        return Err(package_error(
            "unsafe_archive_path",
            "archive path has no usable segments",
        ));
    }
    Ok((relative, canonical.join("/")))
}

/// Apply the path-segment rules shared by archive extraction and persisted
/// package manifests. Windows treats DOS device basenames as devices even
/// when an extension is present, so these names are unsafe on every platform.
pub(crate) fn is_safe_package_path_segment(segment: &str) -> bool {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.ends_with(['.', ' '])
        || segment.bytes().any(|byte| {
            byte < 0x20
                || byte == 0x7f
                || matches!(
                    byte,
                    b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*'
                )
        })
    {
        return false;
    }

    let basename = segment
        .split_once('.')
        .map_or(segment, |(basename, _)| basename)
        .trim_end_matches(['.', ' ']);
    if ["con", "prn", "aux", "nul"]
        .iter()
        .any(|reserved| basename.eq_ignore_ascii_case(reserved))
    {
        return false;
    }

    let bytes = basename.as_bytes();
    !(bytes.len() == 4
        && matches!(bytes[3], b'1'..=b'9')
        && (bytes[..3].eq_ignore_ascii_case(b"com") || bytes[..3].eq_ignore_ascii_case(b"lpt")))
}

fn prepare_destination(dest: &Path) -> Result<PathBuf> {
    let dest = absolute_scratch_path(dest)?;
    ensure_scratch_directory(&dest, true)?;
    if std::fs::read_dir(&dest)
        .map_err(|error| user_path_err("read_import_scratch", error.to_string(), &dest, false))?
        .next()
        .is_some()
    {
        return Err(user_path_err(
            "import_scratch_not_empty",
            "import scratch directory is not empty",
            &dest,
            false,
        ));
    }
    Ok(dest)
}

fn absolute_scratch_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                user_path_err("resolve_import_scratch", error.to_string(), path, false)
            })?
            .join(path)
    };
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(invalid_scratch_path(&path));
    }
    Ok(path)
}

fn ensure_scratch_directory(path: &Path, create: bool) -> Result<bool> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                current.push(component.as_os_str());
            }
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => return Err(invalid_scratch_path(path)),
            std::path::Component::Normal(_) => {
                current.push(component.as_os_str());
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => {}
                    Ok(_) => return Err(invalid_scratch_path(&current)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                        return Ok(false);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match std::fs::create_dir(&current) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                            Err(error) => {
                                return Err(user_path_err(
                                    "create_import_scratch",
                                    error.to_string(),
                                    &current,
                                    false,
                                ));
                            }
                        }
                        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
                            user_path_err(
                                "inspect_import_scratch",
                                error.to_string(),
                                &current,
                                false,
                            )
                        })?;
                        if !metadata.is_dir() || is_link_or_reparse(&metadata) {
                            return Err(invalid_scratch_path(&current));
                        }
                    }
                    Err(error) => {
                        return Err(user_path_err(
                            "inspect_import_scratch",
                            error.to_string(),
                            &current,
                            false,
                        ));
                    }
                }
            }
        }
    }
    Ok(true)
}

fn cleanup_destination(dest: &Path) -> Result<()> {
    if !ensure_scratch_directory(dest, false)? {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(dest)
        .map_err(|error| user_path_err("inspect_import_scratch", error.to_string(), dest, false))?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(invalid_scratch_path(dest));
    }
    remove_scratch_entry(dest, true)
}

fn remove_scratch_entry(path: &Path, root: bool) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(user_path_err(
                "inspect_import_scratch",
                error.to_string(),
                path,
                false,
            ));
        }
    };
    if is_link_or_reparse(&metadata) {
        if root {
            return Err(invalid_scratch_path(path));
        }
        return remove_scratch_link(path, &metadata);
    }
    if metadata.is_dir() {
        let entries = std::fs::read_dir(path)
            .map_err(|error| user_path_err("read_import_scratch", error.to_string(), path, false))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                user_path_err("read_import_scratch", error.to_string(), path, false)
            })?;
        for entry in entries {
            remove_scratch_entry(&entry.path(), false)?;
        }
        std::fs::remove_dir(path)
            .map_err(|error| user_path_err("remove_import_scratch", error.to_string(), path, false))
    } else {
        std::fs::remove_file(path)
            .map_err(|error| user_path_err("remove_import_scratch", error.to_string(), path, false))
    }
}

#[cfg(unix)]
fn remove_scratch_link(path: &Path, _metadata: &std::fs::Metadata) -> Result<()> {
    std::fs::remove_file(path).map_err(|error| {
        user_path_err("remove_import_scratch_link", error.to_string(), path, false)
    })
}

#[cfg(windows)]
fn remove_scratch_link(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let result = if metadata.file_type().is_symlink_dir() || metadata.is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|error| {
        user_path_err("remove_import_scratch_link", error.to_string(), path, false)
    })
}

#[cfg(not(any(unix, windows)))]
fn remove_scratch_link(path: &Path, _metadata: &std::fs::Metadata) -> Result<()> {
    std::fs::remove_file(path).map_err(|error| {
        user_path_err("remove_import_scratch_link", error.to_string(), path, false)
    })
}

fn invalid_scratch_path(path: &Path) -> crate::Error {
    user_path_err(
        "invalid_import_scratch",
        "import scratch paths must not cross links, reparse points, or non-directory ancestors",
        path,
        false,
    )
}

fn nearest_existing_ancestor(path: &Path) -> &Path {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return candidate;
        }
        let Some(parent) = candidate.parent() else {
            return path;
        };
        candidate = parent;
    }
}

/// Require space for an ingestion target as well as for archive extraction.
/// The cache and package store may live on different volumes, so checking the
/// extraction destination alone is not sufficient.
pub(crate) fn require_available_space(
    path: &Path,
    content_bytes: u64,
    reserve_bytes: u64,
) -> Result<()> {
    let required = content_bytes
        .checked_add(reserve_bytes)
        .ok_or_else(|| pkg_err("package", "content size plus reserve overflows u64"))?;
    let space_path = nearest_existing_ancestor(path);
    if available_space(space_path)? < required {
        Err(EnvironmentError::InsufficientSpace {
            path: Some(space_path.to_path_buf()),
        }
        .into())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
#[allow(
    clippy::unnecessary_cast,
    reason = "statvfs field widths vary across Unix targets"
)]
fn available_space(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let encoded = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| pkg_err("archive", "free-space path contains a NUL byte"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(encoded.as_ptr(), stats.as_mut_ptr()) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(user_path_err(
            "query_free_space",
            error.to_string(),
            path,
            false,
        ));
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(windows)]
fn available_space(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }
    let encoded: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0_u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            encoded.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(user_path_err(
            "query_free_space",
            std::io::Error::last_os_error().to_string(),
            path,
            false,
        ))
    } else {
        Ok(available)
    }
}

#[cfg(not(any(unix, windows)))]
fn available_space(_path: &Path) -> Result<u64> {
    Err(EnvironmentError::UnsupportedPlatform.into())
}

/// Analyze an extracted package tree.
pub fn preview_plan(plan: &PackagePlan, archive_name: Option<&str>) -> ImportPreview {
    let meta = plan.metadata.as_ref();
    let title = meta.and_then(|metadata| metadata.title.clone());
    let author = meta.and_then(|metadata| metadata.author.clone());
    let id_source = title
        .as_deref()
        .or(author.as_deref())
        .or(archive_name)
        .unwrap_or("imported-package");
    ImportPreview {
        suggested_id: suggested_package_id(id_source),
        title,
        author: meta.and_then(|metadata| metadata.author.clone()),
        version: meta.and_then(|metadata| metadata.version.clone()),
        desc: meta.and_then(|metadata| metadata.desc.clone()),
        slot_guess: match plan.slot_guess.kind {
            SlotGuessKind::Unknown => "unknown".into(),
            kind => kind.as_str().into(),
        },
        matched_pattern: plan.slot_guess.matched_pattern.map(str::to_string),
        warnings: plan.warnings.clone(),
        file_count: plan.files.len(),
    }
}

fn suggested_package_id(text: &str) -> String {
    let mut candidate = slug(text);
    candidate.truncate(PackageId::MAX_LEN);
    while candidate.ends_with('-') {
        candidate.pop();
    }
    if PackageId::parse(&candidate).is_ok() {
        candidate
    } else {
        "imported-package".into()
    }
}

fn slug(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
        } else if matches!(output.as_bytes().last(), Some(byte) if byte.is_ascii_alphanumeric()) {
            output.push('-');
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    output
}

fn package_error(code: &str, message: impl Into<String>) -> crate::Error {
    crate::error::package_err(code, message)
}
