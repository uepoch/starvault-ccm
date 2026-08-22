//! Campaign slot switching.
//!
//! Transaction state machine per `docs/design/slot-manager.md`. Dedicated
//! slots (HotS/LotV/NCO) junction to a materialized deploy tree when the
//! volume supports it; WoL's shared `Maps\Campaign` root and every fallback
//! use the copy strategy. Both paths run stage → verify → swap → commit with
//! rollback on any post-mutation failure.

use std::path::{Path, PathBuf};

use crate::config::StrategyChoice;
use crate::error::{pkg_err, Result};
use crate::layout::{GameLayout, SlotId, WindowsLayout};
use crate::store::{PackageManifest, Store};

/// Which package revision a slot points at. `None` = plain campaign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotState {
    pub slot: SlotId,
    pub active: Option<(String, String)>,
}

/// Slot operations over a game install, backed by the store.
pub struct SlotManager<'a> {
    layout: &'a WindowsLayout,
    store: &'a Store,
    strategy_override: Option<StrategyChoice>,
}

impl<'a> SlotManager<'a> {
    pub fn new(layout: &'a WindowsLayout, store: &'a Store) -> Self {
        Self {
            layout,
            store,
            strategy_override: None,
        }
    }

    /// Per-install strategy override; None = auto (junction first for
    /// dedicated slots, automatic fallback to copy on any failure).
    pub fn with_strategy(mut self, choice: Option<StrategyChoice>) -> Self {
        self.strategy_override = choice;
        self
    }

    /// Activate `(id, rev)` on `slot`, replacing whatever is there.
    ///
    /// Also deploys the union of `mods/**` for all would-be-active packages;
    /// a genuine cross-slot conflict aborts before anything is touched (M5).
    #[tracing::instrument(skip_all, fields(slot = slot.as_str(), pkg = id, rev = rev))]
    pub fn activate(&self, slot: SlotId, id: &str, rev: &str) -> Result<()> {
        let manifest = self.store.load_manifest(id, rev)?;

        // M5: compute the mods union across all active slots plus this one.
        let mut manifests: Vec<PackageManifest> = Vec::new();
        for (active_slot, active_id, active_rev) in self.store.active_slots()? {
            if active_slot == slot.as_str() {
                continue; // being replaced
            }
            manifests.push(self.store.load_manifest(&active_id, &active_rev)?);
        }
        manifests.push(manifest.clone());
        let refs: Vec<&PackageManifest> = manifests.iter().collect();
        let (union, conflicts) = self.store.plan_mods_union(&refs);
        if let Some(c) = conflicts.first() {
            return Err(crate::error::Error::User(crate::UserError {
                message: format!(
                    "dependency conflict on {}: `{}` and `{}` ship different content",
                    c.target, c.first, c.second
                ),
                path: None,
            }));
        }

        // --- swap ----------------------------------------------------------
        let slot_dir = self.layout.slot_dir(slot);
        let backup = sibling_path(&slot_dir, "backup");
        let swapped = self.swap(slot, &manifest, &backup);

        match swapped {
            Ok(()) => {
                self.store.set_active_slot(slot, id, rev)?;
                // The switch is committed; a union deployment failure must
                // not read as "nothing happened".
                self.store
                    .apply_mods_union(&union, &self.layout.mods_dir())
                    .map_err(|e| {
                        pkg_err(
                            slot.as_str(),
                            format!(
                                "activated, but deploying shared Mods\\ dependencies failed: {e}"
                            ),
                        )
                    })?;
                cleanup_if_exists(&backup);
                let _ = self.reclaim_leftovers(slot);
                Ok(())
            }
            Err(e) => {
                if slot_dir.symlink_metadata().is_err() && backup.symlink_metadata().is_ok() {
                    let _ = std::fs::rename(&backup, &slot_dir);
                }
                let _ = self.reclaim_leftovers(slot);
                Err(e)
            }
        }
    }

    #[tracing::instrument(skip_all, fields(pkg = manifest.id))]
    fn swap(&self, slot: SlotId, manifest: &PackageManifest, backup: &Path) -> Result<()> {
        if self.wants_junction(slot) {
            match self.swap_junction(slot, manifest, backup) {
                Ok(()) => Ok(()),
                Err(junction_err) => {
                    // Automatic fallback (design §strategies): non-Windows,
                    // unsupported volume, or any junction failure. The failed
                    // attempt restored the original state before returning.
                    self.swap_copy(slot, manifest, backup).map_err(|copy_err| {
                        pkg_err(
                            slot.as_str(),
                            format!("junction ({junction_err}) and copy ({copy_err}) both failed"),
                        )
                    })
                }
            }
        } else {
            self.swap_copy(slot, manifest, backup)
        }
    }

    /// Auto mode wants junctions for dedicated slots; WoL's slot is the
    /// shared `Maps\Campaign` root — junctioning it would hide sibling
    /// campaigns (`swarm`, `void`, …), so it always copies.
    fn wants_junction(&self, slot: SlotId) -> bool {
        slot != SlotId::Wol && self.strategy_override != Some(StrategyChoice::Copy)
    }

    /// Materialize the revision once under `<store>/deploy/<slot>-<rev>` and
    /// point a directory junction at it. Re-materialized only when missing,
    /// so switching back to a known revision is instant.
    fn swap_junction(&self, slot: SlotId, manifest: &PackageManifest, backup: &Path) -> Result<()> {
        let deployed =
            self.store
                .root()
                .join("deploy")
                .join(format!("{}-{}", slot.as_str(), manifest.rev));
        if !deployed.exists() {
            self.store.materialize_slot(manifest, &deployed)?;
        }
        self.verify_staged(manifest, &deployed)?;

        let slot_dir = self.layout.slot_dir(slot);
        if slot_dir.symlink_metadata().is_ok() {
            std::fs::rename(&slot_dir, backup).or_else(|_| {
                // Renaming an existing link may fail where removing works.
                std::fs::remove_dir_all(&slot_dir)
                    .map_err(|e| pkg_err(slot_dir.display().to_string(), e.to_string()))
            })?;
        }
        // Defensive: never hand the junction API an occupied path.
        if slot_dir.symlink_metadata().is_ok() {
            let _ = remove_junction(&slot_dir);
        }
        if let Err(e) = make_junction(&slot_dir, &deployed) {
            // Restore the previous state; the caller may fall back to copy.
            if backup.symlink_metadata().is_ok() && slot_dir.symlink_metadata().is_err() {
                let _ = std::fs::rename(backup, &slot_dir);
            }
            return Err(pkg_err(
                slot_dir.display().to_string(),
                format!("create junction: {e}"),
            ));
        }
        Ok(())
    }

    /// Copy strategy: materialize into a staging sibling, verify, rename in.
    fn swap_copy(&self, slot: SlotId, manifest: &PackageManifest, backup: &Path) -> Result<()> {
        let slot_dir = self.layout.slot_dir(slot);
        let staging = sibling_path(&slot_dir, "staging");
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        self.store.materialize_slot(manifest, &staging)?;
        self.verify_staged(manifest, &staging)?;

        let shared_root = slot == SlotId::Wol;
        let result = (|| -> Result<()> {
            if backup.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(backup)?;
            }
            if shared_root {
                clear_dir_contents(&slot_dir, &PROTECTED_SIBLINGS)?;
                std::fs::rename(&staging, &slot_dir).or_else(|_| copy_tree(&staging, &slot_dir))?;
            } else {
                // A leftover junction counts as occupied even when its target
                // is gone (exists() misses that) — this was a real crash.
                if slot_dir.symlink_metadata().is_ok() {
                    std::fs::rename(&slot_dir, backup)?;
                }
                if let Err(e) = std::fs::rename(&staging, &slot_dir) {
                    // restore original before surfacing
                    if backup.symlink_metadata().is_ok() && slot_dir.symlink_metadata().is_err() {
                        std::fs::rename(backup, &slot_dir)?;
                    }
                    return Err(e.into());
                }
            }
            Ok(())
        })();

        cleanup_if_exists(&staging);
        result
    }

    /// Return a slot to its plain Blizzard state.
    #[tracing::instrument(skip_all, fields(slot = slot.as_str()))]
    pub fn restore(&self, slot: SlotId) -> Result<()> {
        let slot_dir = self.layout.slot_dir(slot);
        if slot == SlotId::Wol {
            clear_dir_contents(&slot_dir, &PROTECTED_SIBLINGS)?;
        } else if slot_dir.exists() || symlink_or_junction_exists(&slot_dir) {
            // A junction is removed as a link — never delete the store
            // target it points at.
            if std::fs::symlink_metadata(&slot_dir)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                remove_junction(&slot_dir)?;
            } else {
                std::fs::remove_dir_all(&slot_dir)?;
            }
            std::fs::create_dir_all(&slot_dir)?;
        }
        self.store.clear_active_slot(slot)?;
        Ok(())
    }

    /// Startup reconciliation (design §crash recovery): remove dangling
    /// links, reclaim stale staging dirs, restore backups newer than the
    /// ledger. Returns human-readable repair notes for the log screen.
    pub fn reconcile(&self) -> Result<Vec<String>> {
        let mut report = Vec::new();
        for slot in SlotId::ALL {
            let slot_dir = self.layout.slot_dir(slot);

            // Dangling junction: target gone means content gone. Clear the
            // ledger so reported state matches reality; the user re-activates.
            if let Ok(meta) = std::fs::symlink_metadata(&slot_dir) {
                if meta.file_type().is_symlink()
                    && std::fs::read_link(&slot_dir)
                        .map(|t| !t.exists())
                        .unwrap_or(true)
                {
                    remove_junction(&slot_dir)?;
                    self.store.clear_active_slot(slot)?;
                    report.push(format!(
                        "{}: dangling junction removed; activate again",
                        slot.as_str()
                    ));
                }
            }

            report.extend(self.reclaim_leftovers(slot));
        }
        Ok(report)
    }

    /// Remove leftover `.staging-*`; a `.backup-*` with no live slot dir is
    /// a crash mid-swap — restore it, otherwise it is committed garbage.
    fn reclaim_leftovers(&self, slot: SlotId) -> Vec<String> {
        let mut report = Vec::new();
        let slot_dir = self.layout.slot_dir(slot);
        let Some(parent) = slot_dir.parent() else {
            return report;
        };
        let name = slot_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging_prefix = format!("{name}.staging-");
        let backup_prefix = format!("{name}.backup-");
        let Ok(entries) = std::fs::read_dir(parent) else {
            return report;
        };
        for entry in entries.flatten() {
            let entry_name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if entry_name.starts_with(&staging_prefix) {
                if cleanup_if_exists(&path) {
                    report.push(format!("reclaimed {}", path.display()));
                }
            } else if entry_name.starts_with(&backup_prefix) {
                if slot_dir.symlink_metadata().is_err() {
                    if std::fs::rename(&path, &slot_dir).is_ok() {
                        report.push(format!("restored {} from crash backup", slot.as_str()));
                    }
                } else {
                    cleanup_if_exists(&path);
                }
            }
        }
        report
    }

    fn verify_staged(&self, manifest: &PackageManifest, staged: &Path) -> Result<()> {
        let expected = manifest
            .files
            .iter()
            .filter(|f| f.path.starts_with("slot/"))
            .count();
        let mut actual = 0usize;
        let mut stack = vec![staged.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    stack.push(entry.path());
                } else {
                    actual += 1;
                }
            }
        }
        if actual != expected {
            return Err(pkg_err(
                staged.display().to_string(),
                format!("staged file count {actual} != expected {expected}"),
            ));
        }
        Ok(())
    }
}

/// NTFS directory junctions need no admin rights and exist only on Windows;
/// other platforms always fail, driving the automatic copy fallback.
#[cfg(windows)]
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    junction::create(target, link)
}

#[cfg(not(windows))]
fn make_junction(_link: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("junctions require Windows"))
}

/// Remove a junction/link itself without touching its target. A junction is
/// a directory reparse point: `remove_file` fails with access-denied on
/// Windows — it must be removed like a directory.
fn remove_junction(path: &Path) -> Result<()> {
    let result = if cfg!(windows) {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|e| pkg_err(path.display().to_string(), format!("remove junction: {e}")))
}

/// A directory exists, or a link exists in its place (rename over a plain
/// `exists()` misses junctions whose target was checked first).
fn symlink_or_junction_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok() && !path.is_dir()
}

/// Sibling scratch path: `<parent>/<name>.<kind>-<pid>`.
fn sibling_path(dir: &Path, kind: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.with_file_name(format!("{name}.{kind}-{}", std::process::id()))
}

fn cleanup_if_exists(path: &Path) -> bool {
    // symlink_metadata so dangling junctions/links count as existing;
    // remove_dir_all follows junctions on Windows, so links go through
    // remove_junction (target untouched).
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return remove_junction(path).is_ok();
        }
        return std::fs::remove_dir_all(path).is_ok();
    }
    false
}

/// Directories inside `Maps\Campaign` that belong to other slots and must
/// survive a WoL clear (same protection list as the old tool, minus bugs).
const PROTECTED_SIBLINGS: [&str; 5] = ["swarm", "swarm\\evolution", "void", "voidprologue", "nova"];

fn clear_dir_contents(dir: &Path, protect: &[&str]) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        if protect.iter().any(|p| p.eq_ignore_ascii_case(&lower)) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

pub(crate) fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
