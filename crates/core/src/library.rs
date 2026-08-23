//! Installed-package inventory and old-CCM migration detection.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::contracts::{Health, HealthIssue, HealthState, LibrarySnapshot};
use crate::error::Result;
use crate::identity::PackageId;
use crate::layout::SlotId;
use crate::layout::WindowsLayout;
use crate::store::Store;

/// One installed package. A package has exactly one current revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LibraryEntry {
    pub id: PackageId,
    pub revision: String,
    pub faction: SlotId,
    pub title: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    pub desc: Option<String>,
    pub imported_at: Option<u64>,
}

/// Build the Library response without hiding corrupt packages.
pub fn scan(store: &Store) -> Result<LibrarySnapshot> {
    let active_campaign = store.active_campaign()?;
    let inventory = store.inventory()?;
    let mut issues: Vec<HealthIssue> = inventory
        .corrupt
        .iter()
        .map(|corrupt| HealthIssue {
            code: corrupt.code.clone(),
            message: corrupt.message.clone(),
            path: Some(corrupt.manifest_path.display().to_string()),
            repairable: false,
        })
        .collect();
    let entries: Vec<LibraryEntry> = inventory
        .packages
        .into_iter()
        .map(|manifest| LibraryEntry {
            id: manifest.id,
            revision: manifest.revision,
            faction: manifest.faction,
            title: manifest.title,
            author: manifest.author,
            version: manifest.version,
            desc: manifest.desc,
            imported_at: manifest.imported_at,
        })
        .collect();

    let active_matches = active_campaign.as_ref().is_none_or(|active| {
        entries.iter().any(|entry| {
            entry.id == active.id
                && entry.revision == active.revision
                && entry.faction == active.faction
        })
    });
    let state = if !active_matches {
        issues.push(HealthIssue {
            code: "active_campaign_manifest_missing".into(),
            message: "The active campaign does not match an installed package manifest".into(),
            path: None,
            // Repair needs the active manifest as its trusted source. If the
            // manifest is missing or mismatched, only operator recovery can
            // establish which content should be deployed.
            repairable: false,
        });
        HealthState::RecoveryRequired
    } else if issues.is_empty() {
        HealthState::Ready
    } else {
        HealthState::Drifted
    };
    Ok(LibrarySnapshot {
        entries,
        active_campaign,
        health: Health { state, issues },
    })
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
    /// Opaque token accepted by the migration command after fresh discovery.
    pub candidate_id: String,
    /// Directory name; the default package id after slugging.
    pub name: String,
    #[serde(skip)]
    path: PathBuf,
}

impl MigrationCandidate {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Custom campaign dirs in `Maps\Campaign` that are not reserved faction
/// override directories — i.e. what an old CCM install deployed there.
pub fn migration_candidates(layout: &WindowsLayout) -> Vec<MigrationCandidate> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(layout.slot_dir(crate::layout::SlotId::Wol)) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.is_dir() || is_link_or_reparse_point(&metadata) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if crate::layout::SLOT_OWNED_SIBLINGS.contains(&lower.as_str()) {
            continue; // slot-owned locations, never migrations
        }
        // Exact dedicated-slot crash artifacts are not campaigns. A legacy
        // campaign may legitimately contain these words in its own name.
        if is_slot_operation_artifact(&lower) {
            continue;
        }
        // A campaign must contain at least one container somewhere.
        if !contains_container(&entry.path()) {
            continue;
        }
        let path = entry.path();
        out.push(MigrationCandidate {
            candidate_id: migration_candidate_id(&path),
            name,
            path,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn migration_candidate_id(path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"starvault-migration-candidate\0");
    hasher.update(path.to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())
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
            let Ok(metadata) = std::fs::symlink_metadata(&p) else {
                return false;
            };
            if is_link_or_reparse_point(&metadata) {
                return false;
            }
            let is_container = p.extension().is_some_and(|e| {
                e.eq_ignore_ascii_case("sc2map") || e.eq_ignore_ascii_case("sc2mod")
            });
            // SC2 containers may be packed files or unpacked directories.
            if is_container && (metadata.is_file() || metadata.is_dir()) {
                return true;
            }
            if metadata.is_dir() {
                stack.push(p);
            } else if !metadata.is_file() {
                return false;
            }
        }
    }
    false
}

fn is_slot_operation_artifact(name: &str) -> bool {
    ["swarm", "void", "nova"].into_iter().any(|slot| {
        ["staging", "backup"].into_iter().any(|kind| {
            let prefix = format!("{slot}.{kind}-");
            name.strip_prefix(&prefix).is_some_and(valid_operation_id)
        })
    })
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
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
