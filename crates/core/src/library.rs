//! Installed-package scanning and old-CCM migration detection.
//!
//! The Library screen is a pure view over store state: every installed
//! revision, annotated with where it is active. Legacy detection reads the
//! old tool's one-line config (decision P2: explicit migration flow).

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::store::Store;

use serde::Serialize;

/// One installed package revision as the Library screen renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LibraryEntry {
    pub id: String,
    pub rev: String,
    /// Slot the package targets (`wol`, `hots`, `lotv`, `nco`).
    pub slot: String,
    /// Slots where this exact revision is currently active; empty = inactive.
    pub active_on: Vec<String>,
}

/// List every installed package revision, annotated with activation status.
pub fn scan(store: &Store) -> Result<Vec<LibraryEntry>> {
    let active = store.active_slots()?;
    Ok(store
        .list_packages()?
        .into_iter()
        .map(|(id, rev, slot)| {
            let active_on = active
                .iter()
                .filter(|(_, pkg, r)| pkg == &id && r == &rev)
                .map(|(slot, _, _)| slot.clone())
                .collect();
            LibraryEntry {
                id,
                rev,
                slot,
                active_on,
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
