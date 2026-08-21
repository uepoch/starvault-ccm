//! Archive import: zip extraction, preview, and the K2 confirm flow.
//!
//! Import is interactive (decision K2): analyze produces a preview the user
//! confirms — detected metadata, slot guess with its basis, warnings — before
//! anything is ingested. Any zip layout is accepted (K4); the normalizer
//! derives structure.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{pkg_err, Result};
use crate::package::metadata::SlotGuessKind;
use crate::package::normalize::PackagePlan;

/// What the user sees before confirming an import (K2).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ImportPreview {
    /// Package id derived from the title (or archive name as fallback).
    pub suggested_id: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    /// `unknown`, or one of `wol` / `hots` / `lotv` / `nco`.
    pub slot_guess: String,
    /// The legacy pattern that produced the guess; shown as its basis.
    pub matched_pattern: Option<&'static str>,
    pub warnings: Vec<String>,
    pub file_count: usize,
}

/// Progress for long import phases. `false` from the callback cancels.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ImportProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub current_file: String,
}

/// Extract a package zip into `dest`, reporting per-file progress.
///
/// Returns `Ok(false)` when cancelled. Entry paths are validated against
/// path traversal (`../`, absolute paths, Windows drive prefixes).
pub fn extract_archive(
    zip_path: &Path,
    dest: &Path,
    mut on_progress: impl FnMut(ImportProgress) -> bool,
) -> Result<bool> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| pkg_err(zip_path.display().to_string(), e.to_string()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| pkg_err(zip_path.display().to_string(), format!("read zip: {e}")))?;

    let total = archive.len() as u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| {
            pkg_err(
                zip_path.display().to_string(),
                format!("zip entry {i}: {e}"),
            )
        })?;
        let name = entry.name().to_string();
        let target = safe_join(dest, &name)?;

        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;

        if !on_progress(ImportProgress {
            files_done: i as u64 + 1,
            files_total: total,
            current_file: name,
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Analyze an extracted package tree: what would be imported, and as what.
pub fn preview_plan(plan: &PackagePlan) -> ImportPreview {
    let meta = plan.metadata.as_ref();
    let title = meta.and_then(|m| m.title.clone());
    let fallback = "imported-package";
    let suggested_id = slug(
        title
            .as_deref()
            .or(meta.and_then(|m| m.author.clone()).as_deref())
            .unwrap_or(fallback),
    );
    ImportPreview {
        suggested_id,
        title,
        author: meta.and_then(|m| m.author.clone()),
        version: meta.and_then(|m| m.version.clone()),
        slot_guess: match plan.slot_guess.kind {
            SlotGuessKind::Unknown => "unknown".into(),
            k => k.as_str().into(),
        },
        matched_pattern: plan.slot_guess.matched_pattern,
        warnings: plan.warnings.clone(),
        file_count: plan.files.len(),
    }
}

/// Join an archive member name onto `dest`, rejecting traversal.
fn safe_join(dest: &Path, name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute()
        || name.contains(':')
        || path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(pkg_err(name, "unsafe path in archive"));
    }
    Ok(dest.join(path))
}

/// Lowercase ASCII slug: letters, digits, dashes.
fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(out.as_bytes().last(), Some(b) if b.is_ascii_alphanumeric()) {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}
