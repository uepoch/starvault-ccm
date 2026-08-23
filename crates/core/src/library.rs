//! Installed-package scanning and old-CCM migration detection.
//!
//! The Library screen is a pure view over store state: every installed
//! revision, annotated with where it is active. Legacy detection reads the
//! old tool's one-line config (decision P2: explicit migration flow).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Result;
use crate::layout::WindowsLayout;
use crate::store::Store;

/// One installed package revision as the Library screen renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LibraryEntry {
    pub id: String,
    pub rev: String,
    /// Slot the package targets (`wol`, `hots`, `lotv`, `nco`).
    pub slot: String,
    /// Slots where this exact revision is currently active; empty = inactive.
    pub active_on: Vec<String>,
    /// Metadata from the manifest, when the package carried any.
    pub title: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    pub desc: Option<String>,
    /// Unix seconds when this revision was imported.
    pub imported_at: Option<u64>,
}

/// List every installed package revision, annotated with activation status.
pub fn scan(store: &Store) -> Result<Vec<LibraryEntry>> {
    let active = store.active_slots()?;
    Ok(store
        .all_manifests()?
        .into_iter()
        .map(|m| {
            let (id, rev) = (m.id.clone(), m.rev.clone());
            let active_on = active
                .iter()
                .filter(|(_, pkg, r)| pkg == &id && r == &rev)
                .map(|(slot, _, _)| slot.clone())
                .collect();
            LibraryEntry {
                id,
                rev,
                slot: m.slot,
                active_on,
                title: m.title,
                author: m.author,
                version: m.version,
                desc: m.desc,
                imported_at: m.imported_at,
            }
        })
        .collect())
}

/// Detects a legacy SC2CCM configuration at
/// `%APPDATA%\SC2CCM\SC2CCM.txt` (decision P2: explicit migration flow).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyCcmInstall {
    /// First line of the old config: path to StarCraft II.exe.
    pub exe_hint: Option<String>,
}

impl LegacyCcmInstall {
    /// Path of the legacy config under an `%APPDATA%` root.
    pub fn config_path(appdata: impl AsRef<Path>) -> PathBuf {
        appdata.as_ref().join("SC2CCM").join("SC2CCM.txt")
    }

    /// Detect an old install under `appdata`; `None` when there is none.
    pub fn detect(appdata: impl AsRef<Path>) -> Option<Self> {
        let text = std::fs::read_to_string(Self::config_path(appdata)).ok()?;
        Some(Self {
            exe_hint: text
                .lines()
                .next()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string),
        })
    }
}

/// One custom campaign directory an old SC2CCM install left behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationCandidate {
    pub path: String,
    /// Directory name; the default package id after slugging.
    pub name: String,
}

/// Custom campaign dirs in `Maps\Campaign` that are not one of the four
/// slots' own locations — i.e. what an old CCM install deployed there.
pub fn migration_candidates(layout: &WindowsLayout) -> Vec<MigrationCandidate> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(layout.slot_dir(crate::layout::SlotId::Wol)) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.path().is_dir() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if crate::layout::SLOT_OWNED_SIBLINGS.contains(&lower.as_str()) {
            continue; // slot-owned locations, never migrations
        }
        // Crash-recovery leftovers are not campaigns.
        if lower.contains(".backup-") || lower.contains(".staging-") {
            continue;
        }
        // A campaign must contain at least one container somewhere.
        if !contains_container(&entry.path()) {
            continue;
        }
        out.push(MigrationCandidate {
            path: entry.path().display().to_string(),
            name,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// A campaign candidate holds at least one `.SC2Map`/`.SC2Mod` container.
fn contains_container(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let is_container = p.extension().is_some_and(|e| {
                e.eq_ignore_ascii_case("sc2map") || e.eq_ignore_ascii_case("sc2mod")
            });
            if is_container {
                return true;
            }
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
    false
}
