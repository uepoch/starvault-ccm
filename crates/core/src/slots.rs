//! Campaign slot switching.
//!
//! Transaction state machine and strategies per `docs/design/slot-manager.md`.
//! The copy strategy is complete here; junctions land in M2 behind the same
//! interface.

use std::path::{Path, PathBuf};

use crate::error::{pkg_err, Result};
use crate::layout::{GameLayout, SlotId, WindowsLayout};
use crate::store::{PackageManifest, Store};

/// Phases of a switch transaction (for progress events later).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchPhase {
    Staging,
    Verified,
    Committed,
    RolledBack,
}

/// Which package revision a slot points at. `None` = plain campaign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotState {
    pub slot: SlotId,
    pub active: Option<(String, String)>,
}

/// Slot operations over a game install, backed by the store.
///
/// All mutations go: stage → verify → swap → commit, with rollback on any
/// failure after the first mutation.
pub struct SlotManager<'a> {
    layout: &'a WindowsLayout,
    store: &'a Store,
}

impl<'a> SlotManager<'a> {
    pub fn new(layout: &'a WindowsLayout, store: &'a Store) -> Self {
        Self { layout, store }
    }

    /// Activate `(id, rev)` on `slot`, replacing whatever is there.
    ///
    /// Also deploys the union of `mods/**` for all would-be-active packages;
    /// a genuine cross-slot conflict aborts before anything is touched (M5).
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
        let union = self.store.plan_mods_union(&refs)?;

        // --- stage ---------------------------------------------------------
        let slot_dir = self.layout.slot_dir(slot);
        let staging = sibling_path(&slot_dir, "staging");
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        self.store.materialize_slot(&manifest, &staging)?;
        self.verify_staged(&manifest, &staging)?;

        // --- swap ----------------------------------------------------------
        // WoL's slot is the shared Maps\Campaign root; its sibling campaign
        // directories must survive. Dedicated slots get a clean dir swap.
        let shared_root = slot == SlotId::Wol;
        let backup = sibling_path(&slot_dir, "backup");

        let mut rolled_back = false;
        let result = (|| -> Result<()> {
            if backup.exists() {
                std::fs::remove_dir_all(&backup)?;
            }
            if shared_root {
                clear_dir_contents(&slot_dir, &PROTECTED_SIBLINGS)?;
                std::fs::rename(&staging, &slot_dir).or_else(|_| copy_tree(&staging, &slot_dir))?;
            } else {
                if slot_dir.exists() {
                    std::fs::rename(&slot_dir, &backup)?;
                }
                if let Err(e) = std::fs::rename(&staging, &slot_dir) {
                    // restore original before surfacing
                    if backup.exists() && !slot_dir.exists() {
                        std::fs::rename(&backup, &slot_dir)?;
                        rolled_back = true;
                    }
                    return Err(e.into());
                }
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.store.set_active_slot(slot, id, rev)?;
                self.store
                    .apply_mods_union(&union, &self.layout.mods_dir())?;
                cleanup_if_exists(&staging);
                Ok(())
            }
            Err(e) => {
                if !rolled_back && backup.exists() && !shared_root && !slot_dir.exists() {
                    let _ = std::fs::rename(&backup, &slot_dir);
                }
                cleanup_if_exists(&staging);
                Err(e)
            }
        }
    }

    /// Return a slot to its plain Blizzard state.
    pub fn restore(&self, slot: SlotId) -> Result<()> {
        let slot_dir = self.layout.slot_dir(slot);
        if slot == SlotId::Wol {
            clear_dir_contents(&slot_dir, &PROTECTED_SIBLINGS)?;
        } else if slot_dir.exists() {
            std::fs::remove_dir_all(&slot_dir)?;
            std::fs::create_dir_all(&slot_dir)?;
        }
        self.store.clear_active_slot(slot)?;
        Ok(())
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
            for entry in std::fs::read_dir(&dir)? {
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

/// Sibling scratch path: `<parent>/<name>.<kind>-<pid>`.
fn sibling_path(dir: &Path, kind: &str) -> PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.with_file_name(format!("{name}.{kind}-{}", std::process::id()))
}

fn cleanup_if_exists(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
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

fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
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
