//! Reversible campaign-slot filesystem transitions.
//!
//! This module never writes the ledger. The application workflow prepares all
//! slot changes, journals their paths, applies them, and either rolls them back
//! or finalizes their backups after the atomic ledger commit.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::StrategyChoice;
use crate::error::{internal_err, package_err, user_path_err, Result};
use crate::layout::{SlotId, WindowsLayout, SLOT_OWNED_SIBLINGS};
use crate::operation::{SlotOperationJournal, SlotOperationPaths, SlotStateBinding, SlotStateKind};
use crate::store::{PackageManifest, Store};

const ABSENT_MARKER: &str = ".starvault-absent";
const SHARED_BACKUP_READY: &str = ".starvault-backup-ready";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SharedTreeEntry {
    Directory,
    File { sha256: String, size: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
struct SharedBackupReceipt {
    version: u32,
    entries: BTreeMap<String, SharedTreeEntry>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DedicatedRepairReceipt {
    Absent,
    Directory {
        entries: BTreeMap<String, SharedTreeEntry>,
    },
    Junction {
        target: PathBuf,
        entries: BTreeMap<String, SharedTreeEntry>,
    },
}

#[derive(Debug, Clone)]
pub struct PreparedSlotTransition {
    changes: Vec<SlotChange>,
    repair: bool,
}

#[derive(Debug, Clone)]
struct SlotChange {
    paths: SlotOperationPaths,
    previous: Option<PackageManifest>,
    expected: Option<PackageManifest>,
    previous_state: SlotStateBinding,
    target_state: SlotStateBinding,
}

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

    pub fn with_strategy(mut self, strategy: Option<StrategyChoice>) -> Self {
        self.strategy_override = strategy;
        self
    }

    /// Stage every faction touched by a global previous-to-target transition.
    pub fn prepare(
        &self,
        previous: Option<&PackageManifest>,
        target: Option<&PackageManifest>,
        operation_id: &str,
    ) -> Result<PreparedSlotTransition> {
        validate_operation_id(operation_id)?;
        let mut targets: Vec<(SlotId, Option<&PackageManifest>)> = Vec::new();
        if let Some(previous) = previous {
            targets.push((previous.faction, None));
        }
        if let Some(target) = target {
            if let Some(existing) = targets
                .iter_mut()
                .find(|(faction, _)| *faction == target.faction)
            {
                existing.1 = Some(target);
            } else {
                targets.push((target.faction, Some(target)));
            }
        }

        let mut changes = Vec::with_capacity(targets.len());
        for (faction, expected) in targets {
            let live = self.layout.slot_dir(faction);
            let current = previous.filter(|manifest| manifest.faction == faction);
            self.verify_live_object_identity(faction, &live, current)?;
            verify_slot_tree(faction, &live, current)?;
            let previous_state = capture_slot_state(faction, &live)?;
            let staging = sibling_path(&live, "staging", operation_id);
            let backup = sibling_path(&live, "backup", operation_id);
            ensure_absent(&staging)?;
            ensure_absent(&backup)?;
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent)?;
            }
            self.stage(faction, expected, &staging)?;
            self.verify_live_object_identity(faction, &staging, expected)?;
            let target_state = capture_slot_state(faction, &staging)?;
            changes.push(SlotChange {
                paths: SlotOperationPaths {
                    faction,
                    live,
                    staging,
                    backup,
                },
                previous: current.cloned(),
                expected: expected.cloned(),
                previous_state,
                target_state,
            });
        }
        Ok(PreparedSlotTransition {
            changes,
            repair: false,
        })
    }

    /// Stage an explicit repair of the active slot. The current tree may be
    /// drifted, so it is preserved wholesale as the rollback backup instead
    /// of being accepted as the manifest state.
    pub fn prepare_repair(
        &self,
        manifest: &PackageManifest,
        operation_id: &str,
    ) -> Result<PreparedSlotTransition> {
        validate_operation_id(operation_id)?;
        let live = self.layout.slot_dir(manifest.faction);
        let staging = sibling_path(&live, "staging", operation_id);
        let backup = sibling_path(&live, "backup", operation_id);
        ensure_absent(&staging)?;
        ensure_absent(&backup)?;
        if let Some(parent) = live.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let previous_state = self.repair_backup_binding(manifest, &live)?;
        self.stage_repair(manifest, &staging)?;
        let target_state = capture_slot_state(manifest.faction, &staging)?;
        Ok(PreparedSlotTransition {
            changes: vec![SlotChange {
                paths: SlotOperationPaths {
                    faction: manifest.faction,
                    live,
                    staging,
                    backup,
                },
                previous: None,
                expected: Some(manifest.clone()),
                previous_state,
                target_state,
            }],
            repair: true,
        })
    }

    fn repair_backup_binding(
        &self,
        manifest: &PackageManifest,
        live: &Path,
    ) -> Result<SlotStateBinding> {
        let state = capture_slot_state(manifest.faction, live)?;
        if state.kind == SlotStateKind::Junction {
            let _ = self.owned_deployment_link_target_exists(live, manifest)?;
        }
        Ok(state)
    }

    fn verify_live_object_identity(
        &self,
        faction: SlotId,
        path: &Path,
        manifest: Option<&PackageManifest>,
    ) -> Result<()> {
        if faction == SlotId::Wol {
            return Ok(());
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(user_path_err(
                    "inspect_campaign_slot",
                    error.to_string(),
                    path,
                    true,
                ));
            }
        };
        if !is_link(&metadata) {
            return if metadata.is_dir() {
                Ok(())
            } else {
                Err(package_err(
                    "slot_drift",
                    "campaign slot root is not a regular directory",
                ))
            };
        }
        let Some(manifest) = manifest else {
            return Err(package_err(
                "unowned_campaign_slot_link",
                format!("{faction} campaign slot is an unowned link or junction"),
            ));
        };
        if self.owned_deployment_link_target_exists(path, manifest)? {
            Ok(())
        } else {
            Err(package_err(
                "slot_drift",
                "the active StarVault campaign deployment target is missing",
            ))
        }
    }

    fn owned_deployment_link_target_exists(
        &self,
        path: &Path,
        manifest: &PackageManifest,
    ) -> Result<bool> {
        let deployed = self
            .store
            .deploy_dir(manifest.faction, &manifest.revision)?;
        let actual_target = canonicalize_slot_link(path)?;
        let expected_target = canonicalize_target_allow_missing(&deployed)?;
        if actual_target != expected_target {
            return Err(package_err(
                "unowned_campaign_slot_link",
                "campaign slot points outside StarVault's owned deployment cache",
            ));
        }
        match std::fs::symlink_metadata(&deployed) {
            Ok(metadata) if !is_link(&metadata) && metadata.is_dir() => Ok(true),
            Ok(_) => Err(package_err(
                "unowned_campaign_slot_link",
                "StarVault campaign deployment is not a real directory",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(user_path_err(
                "inspect_slot_deployment",
                error.to_string(),
                &deployed,
                true,
            )),
        }
    }

    fn stage_repair(&self, manifest: &PackageManifest, staging: &Path) -> Result<()> {
        // A live StarVault junction can point at the normal deployment cache.
        // Rebuilding that cache here would mutate the rollback source before
        // the journal exists, so repair always stages an independent copy.
        self.store.materialize_slot(manifest, staging)?;
        verify_slot_tree(manifest.faction, staging, Some(manifest))
    }

    fn stage(
        &self,
        faction: SlotId,
        manifest: Option<&PackageManifest>,
        staging: &Path,
    ) -> Result<()> {
        let Some(manifest) = manifest else {
            std::fs::create_dir_all(staging)?;
            return Ok(());
        };
        if manifest.faction != faction {
            return Err(internal_err(
                "slot_manifest_faction_mismatch",
                "StarVault could not prepare the campaign slot",
                format!(
                    "manifest {} targets {}, transition targets {}",
                    manifest.id, manifest.faction, faction
                ),
            ));
        }
        if faction == SlotId::Wol || self.strategy_override == Some(StrategyChoice::Copy) {
            self.store.materialize_slot(manifest, staging)?;
            verify_slot_tree(faction, staging, Some(manifest))?;
            return Ok(());
        }

        let deployed = self.store.deploy_dir(faction, &manifest.revision)?;
        if deployed.symlink_metadata().is_ok()
            && verify_slot_tree(faction, &deployed, Some(manifest)).is_err()
        {
            remove_entry(&deployed)?;
        }
        if deployed.symlink_metadata().is_err() {
            self.store.materialize_slot(manifest, &deployed)?;
        }
        verify_slot_tree(faction, &deployed, Some(manifest))?;

        match make_junction(staging, &deployed) {
            Ok(()) => Ok(()),
            Err(error) if self.strategy_override.is_none() => {
                tracing::info!(
                    faction = faction.as_str(),
                    error = %error,
                    "junction staging unavailable; using copy strategy"
                );
                remove_entry_if_exists(staging)?;
                self.store.materialize_slot(manifest, staging)?;
                verify_slot_tree(faction, staging, Some(manifest))
            }
            Err(error) => Err(user_path_err(
                "junction_creation_failed",
                error.to_string(),
                staging,
                true,
            )),
        }
    }

    pub fn verify_current(&self, manifest: Option<&PackageManifest>) -> Result<()> {
        for faction in SlotId::ALL {
            let expected = manifest.filter(|manifest| manifest.faction == faction);
            self.verify_live_object_identity(faction, &self.layout.slot_dir(faction), expected)?;
            if let Err(error) = verify_slot_tree(faction, &self.layout.slot_dir(faction), expected)
            {
                if expected.is_none() && error.code() == "slot_drift" {
                    return Err(package_err(
                        "unowned_campaign_files",
                        format!(
                            "{} campaign slot contains files not owned by the active StarVault campaign",
                            faction
                        ),
                    ));
                }
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn verify_target(&self, transition: &PreparedSlotTransition) -> Result<()> {
        for change in &transition.changes {
            verify_slot_tree(
                change.paths.faction,
                &change.paths.live,
                change.expected.as_ref(),
            )?;
            verify_repair_live_identity(&change.paths, &change.target_state)?;
            self.verify_live_object_identity(
                change.paths.faction,
                &change.paths.live,
                change.expected.as_ref(),
            )?;
        }
        Ok(())
    }
}

impl PreparedSlotTransition {
    pub fn journal_paths(&self) -> SlotOperationJournal {
        SlotOperationJournal::new(
            self.changes
                .iter()
                .map(|change| change.paths.clone())
                .collect(),
            self.changes
                .iter()
                .map(|change| change.previous_state.clone())
                .collect(),
            self.changes
                .iter()
                .map(|change| change.target_state.clone())
                .collect(),
        )
    }

    pub fn apply(&self) -> Result<()> {
        self.apply_with_local_rollback(true)
    }

    /// Apply under the application workflow's durable journal. Failures leave
    /// backups and any completed sub-steps in place so the workflow can first
    /// re-check that SC2 is stopped, then perform one cross-resource rollback.
    pub(crate) fn apply_journaled(&self) -> Result<()> {
        self.apply_with_local_rollback(false)
    }

    fn apply_with_local_rollback(&self, rollback_on_error: bool) -> Result<()> {
        let mut applied: Vec<&SlotChange> = Vec::new();
        for change in &self.changes {
            if let Err(error) = apply_change(
                &change.paths,
                change.expected.as_ref(),
                &change.previous_state,
                &change.target_state,
                rollback_on_error,
            ) {
                if rollback_on_error {
                    for prior in applied.into_iter().rev() {
                        rollback_bound_change(prior)?;
                    }
                }
                return Err(error);
            }
            applied.push(change);
        }
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        let paths = self.journal_paths();
        if self.repair {
            rollback_repair_paths_checked(
                &paths,
                self.changes
                    .first()
                    .and_then(|change| change.expected.as_ref()),
            )
        } else {
            rollback_paths_checked(
                &paths,
                self.changes
                    .iter()
                    .find_map(|change| change.previous.as_ref()),
                self.changes
                    .iter()
                    .find_map(|change| change.expected.as_ref()),
            )
        }
    }

    pub fn finalize(&self) -> Result<()> {
        finalize_bound_paths(&self.journal_paths())
    }
}

/// Recovery rollback with manifest checks before any live campaign tree is
/// removed. This prevents a user edit made after an interruption from being
/// mistaken for the staged target and silently deleted.
pub fn rollback_paths_checked(
    paths: &SlotOperationJournal,
    previous: Option<&PackageManifest>,
    target: Option<&PackageManifest>,
) -> Result<()> {
    verify_rollback_paths_checked(paths, previous, target)?;
    let mut first_error = None;
    for slot in paths.iter().rev() {
        let previous_manifest = previous.filter(|manifest| manifest.faction == slot.faction);
        let target_manifest = target.filter(|manifest| manifest.faction == slot.faction);
        let previous_state = previous_binding_for(paths, slot)?;
        let target_state = target_binding_for(paths, slot)?;
        if let Err(error) = verify_bound_rollback_change(
            slot,
            previous_state,
            target_state,
            previous_manifest,
            target_manifest,
            false,
        )
        .and_then(|()| rollback_change(slot))
        .and_then(|()| verify_recovery_live_identity(slot, previous_state))
        .and_then(|()| verify_slot_tree(slot.faction, &slot.live, previous_manifest))
        {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub fn verify_rollback_paths_checked(
    paths: &SlotOperationJournal,
    previous: Option<&PackageManifest>,
    target: Option<&PackageManifest>,
) -> Result<()> {
    validate_journal_bindings(paths, true)?;
    for slot in paths.iter().rev() {
        verify_bound_rollback_change(
            slot,
            previous_binding_for(paths, slot)?,
            target_binding_for(paths, slot)?,
            previous.filter(|manifest| manifest.faction == slot.faction),
            target.filter(|manifest| manifest.faction == slot.faction),
            false,
        )?;
    }
    Ok(())
}

/// Repair backups intentionally contain a drifted tree. Their exact object
/// kind and content are matched to the independently journaled pre-repair
/// fingerprint, while the live side is limited to the target and preserved
/// backup content. When no backup exists, the live tree must still match the
/// pre-repair fingerprint before staging can be removed.
pub fn verify_repair_rollback_paths(
    paths: &SlotOperationJournal,
    target: Option<&PackageManifest>,
) -> Result<()> {
    validate_journal_bindings(paths, true)?;
    for slot in paths.iter().rev() {
        verify_bound_rollback_change(
            slot,
            previous_binding_for(paths, slot)?,
            target_binding_for(paths, slot)?,
            None,
            target.filter(|manifest| manifest.faction == slot.faction),
            true,
        )?;
    }
    Ok(())
}

/// Atomically revalidates journal-bound repair evidence immediately before
/// each destructive rollback step. Callers recovering a repair journal should
/// use this instead of pairing `verify_repair_rollback_paths` with
/// `rollback_paths`.
pub fn rollback_repair_paths_checked(
    paths: &SlotOperationJournal,
    target: Option<&PackageManifest>,
) -> Result<()> {
    verify_repair_rollback_paths(paths, target)?;
    for slot in paths.iter().rev() {
        let previous_state = previous_binding_for(paths, slot)?;
        verify_bound_rollback_change(
            slot,
            previous_state,
            target_binding_for(paths, slot)?,
            None,
            target.filter(|manifest| manifest.faction == slot.faction),
            true,
        )?;
        rollback_change(slot)?;
        verify_recovery_live_identity(slot, previous_state)?;
    }
    Ok(())
}

pub fn finalize_paths(paths: &SlotOperationJournal) -> Result<()> {
    let mut first_error = None;
    for paths in paths {
        for artifact in [&paths.staging, &paths.backup] {
            if let Err(error) = remove_entry_if_exists(artifact) {
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Remove committed or rollback-verified artifacts only while they still
/// match the object identities captured in the atomic operation journal.
/// Missing artifacts are accepted so cleanup can resume after interruption.
pub(crate) fn finalize_bound_paths(paths: &SlotOperationJournal) -> Result<()> {
    verify_finalize_bound_paths(paths)?;
    for slot in paths {
        if slot_artifact_exists(&slot.staging)? {
            verify_finalize_bound_paths(paths)?;
            verify_staging_state_identity(slot, target_binding_for(paths, slot)?)?;
            remove_entry(&slot.staging)?;
        }
        if slot_artifact_exists(&slot.backup)? {
            verify_finalize_bound_paths(paths)?;
            verify_repair_backup_identity(slot, previous_binding_for(paths, slot)?)?;
            remove_entry(&slot.backup)?;
        }
    }
    Ok(())
}

pub(crate) fn verify_finalize_bound_paths(paths: &SlotOperationJournal) -> Result<()> {
    validate_journal_bindings(paths, true)?;
    for slot in paths {
        if slot_artifact_exists(&slot.staging)? {
            verify_staging_state_identity(slot, target_binding_for(paths, slot)?)?;
        }
        if slot_artifact_exists(&slot.backup)? {
            verify_repair_backup_identity(slot, previous_binding_for(paths, slot)?)?;
        }
    }
    Ok(())
}

fn slot_artifact_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_path_err(
            "inspect_slot_artifact",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn apply_change(
    paths: &SlotOperationPaths,
    target: Option<&PackageManifest>,
    previous_state: &SlotStateBinding,
    target_state: &SlotStateBinding,
    rollback_on_error: bool,
) -> Result<()> {
    verify_repair_live_identity(paths, previous_state)?;
    verify_staging_state_identity(paths, target_state)?;
    if paths.faction == SlotId::Wol {
        apply_shared_root(
            paths,
            target,
            previous_state,
            target_state,
            rollback_on_error,
        )
    } else {
        apply_dedicated_slot(paths, previous_state, target_state, rollback_on_error)
    }
}

fn apply_dedicated_slot(
    paths: &SlotOperationPaths,
    previous_state: &SlotStateBinding,
    target_state: &SlotStateBinding,
    rollback_on_error: bool,
) -> Result<()> {
    ensure_absent(&paths.backup)?;
    match std::fs::symlink_metadata(&paths.live) {
        Ok(_) => rename_with_retry(&paths.live, &paths.backup)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&paths.backup)?;
            std::fs::write(paths.backup.join(ABSENT_MARKER), [])?;
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_campaign_slot",
                error.to_string(),
                &paths.live,
                true,
            ));
        }
    }
    if let Err(error) = verify_repair_backup_identity(paths, previous_state) {
        if rollback_on_error {
            restore_dedicated_backup(paths)?;
            verify_recovery_live_identity(paths, previous_state)?;
        }
        return Err(error);
    }
    verify_staging_state_identity(paths, target_state)?;
    if let Err(error) = rename_with_retry(&paths.staging, &paths.live) {
        if rollback_on_error {
            restore_dedicated_backup(paths)?;
            verify_recovery_live_identity(paths, previous_state)?;
        }
        return Err(error);
    }
    if let Err(error) = verify_repair_live_identity(paths, target_state) {
        if rollback_on_error {
            restore_dedicated_backup(paths)?;
            verify_repair_live_identity(paths, previous_state)?;
        }
        return Err(error);
    }
    Ok(())
}

fn apply_shared_root(
    paths: &SlotOperationPaths,
    target: Option<&PackageManifest>,
    previous_state: &SlotStateBinding,
    target_state: &SlotStateBinding,
    rollback_on_error: bool,
) -> Result<()> {
    ensure_absent(&paths.backup)?;
    ensure_shared_live_directory(&paths.live)?;
    let previous_inventory = shared_live_inventory(&paths.live)?;
    let _ = shared_artifact_inventory(&paths.staging)?;
    std::fs::create_dir_all(&paths.backup)?;
    if let Err(error) = copy_shared_entries(&paths.live, &paths.backup, true, false) {
        let _ = remove_shared_artifact_if_exists(&paths.backup);
        return Err(error);
    }
    if shared_artifact_inventory(&paths.backup)? != previous_inventory {
        let _ = remove_shared_artifact_if_exists(&paths.backup);
        return Err(package_err(
            "slot_backup_verification_failed",
            "StarVault could not verify the Wings of Liberty slot backup",
        ));
    }
    write_shared_backup_receipt(paths, &previous_inventory)?;
    if let Err(error) = verify_repair_backup_identity(paths, previous_state) {
        let _ = remove_shared_artifact_if_exists(&paths.backup);
        return Err(error);
    }
    let applied = (|| -> Result<()> {
        verify_repair_live_identity(paths, previous_state)?;
        verify_staging_state_identity(paths, target_state)?;
        clear_shared_live(&paths.live)?;
        for entry in read_dir(&paths.staging)? {
            rename_with_retry(&entry.path(), &paths.live.join(entry.file_name()))?;
        }
        remove_entry_if_exists(&paths.staging)
    })();
    if let Err(error) = applied {
        if rollback_on_error {
            verify_shared_live_subset(paths, target, &previous_inventory)?;
            verify_repair_backup_identity(paths, previous_state)?;
            rollback_shared_root(paths)?;
            verify_repair_live_identity(paths, previous_state)?;
        }
        return Err(error);
    }
    if let Err(error) = verify_repair_live_identity(paths, target_state) {
        if rollback_on_error {
            verify_shared_live_subset(paths, target, &previous_inventory)?;
            verify_repair_backup_identity(paths, previous_state)?;
            rollback_shared_root(paths)?;
            verify_repair_live_identity(paths, previous_state)?;
        }
        return Err(error);
    }
    Ok(())
}

fn rollback_change(paths: &SlotOperationPaths) -> Result<()> {
    if paths.faction == SlotId::Wol {
        rollback_shared_root(paths)
    } else {
        rollback_dedicated_slot(paths)
    }
}

fn verify_bound_rollback_change(
    paths: &SlotOperationPaths,
    previous_state: &SlotStateBinding,
    target_state: &SlotStateBinding,
    previous: Option<&PackageManifest>,
    target: Option<&PackageManifest>,
    repair: bool,
) -> Result<()> {
    match std::fs::symlink_metadata(&paths.backup) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            verify_optional_staging_identity(paths, target_state, target, false)?;
            verify_repair_live_identity(paths, previous_state)?;
            if !repair {
                verify_slot_tree(paths.faction, &paths.live, previous)?;
            }
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_slot_backup",
                error.to_string(),
                &paths.backup,
                true,
            ));
        }
        Ok(_) => {
            verify_repair_backup_identity(paths, previous_state)?;
            verify_optional_staging_identity(paths, target_state, target, true)?;
            if paths.faction == SlotId::Wol {
                if repair {
                    verify_shared_repair_rollback_change(paths, target)?;
                } else {
                    verify_shared_rollback_change(paths, previous, target)?;
                }
                return Ok(());
            }
            if !repair && previous_state.kind != SlotStateKind::Absent {
                verify_slot_tree(paths.faction, &paths.backup, previous)?;
            }
            match std::fs::symlink_metadata(&paths.live) {
                Ok(_) => {
                    verify_recovery_live_identity(paths, target_state)?;
                    verify_slot_tree(paths.faction, &paths.live, target)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(user_path_err(
                        "inspect_campaign_slot",
                        error.to_string(),
                        &paths.live,
                        true,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn verify_optional_staging_identity(
    paths: &SlotOperationPaths,
    target_state: &SlotStateBinding,
    target: Option<&PackageManifest>,
    swap_started: bool,
) -> Result<()> {
    match std::fs::symlink_metadata(&paths.staging) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_path_err(
            "inspect_slot_artifact",
            error.to_string(),
            &paths.staging,
            true,
        )),
        Ok(_) if paths.faction == SlotId::Wol && swap_started => {
            let actual = inventory_shared_tree(&paths.staging, true, false, true)?;
            let expected = manifest_shared_inventory(target);
            if actual
                .iter()
                .all(|(relative, entry)| expected.get(relative) == Some(entry))
            {
                Ok(())
            } else {
                Err(package_err(
                    "slot_drift",
                    "Wings of Liberty staging contains files outside the target campaign",
                ))
            }
        }
        Ok(_) => {
            verify_staging_state_identity(paths, target_state)?;
            verify_slot_tree(paths.faction, &paths.staging, target)
        }
    }
}

fn rollback_bound_change(change: &SlotChange) -> Result<()> {
    verify_bound_rollback_change(
        &change.paths,
        &change.previous_state,
        &change.target_state,
        change.previous.as_ref(),
        change.expected.as_ref(),
        false,
    )?;
    rollback_change(&change.paths)?;
    verify_recovery_live_identity(&change.paths, &change.previous_state)?;
    verify_slot_tree(
        change.paths.faction,
        &change.paths.live,
        change.previous.as_ref(),
    )
}

fn rollback_dedicated_slot(paths: &SlotOperationPaths) -> Result<()> {
    match std::fs::symlink_metadata(&paths.backup) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_entry_if_exists(&paths.staging)?;
            return Ok(());
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_slot_backup",
                error.to_string(),
                &paths.backup,
                true,
            ));
        }
    }
    remove_entry_if_exists(&paths.live)?;
    if paths.backup.join(ABSENT_MARKER).is_file() {
        remove_entry(&paths.backup)?;
    } else {
        rename_with_retry(&paths.backup, &paths.live)?;
    }
    remove_entry_if_exists(&paths.staging)
}

fn restore_dedicated_backup(paths: &SlotOperationPaths) -> Result<()> {
    remove_entry_if_exists(&paths.live)?;
    if paths.backup.join(ABSENT_MARKER).is_file() {
        remove_entry(&paths.backup)
    } else {
        match std::fs::symlink_metadata(&paths.backup) {
            Ok(_) => rename_with_retry(&paths.backup, &paths.live),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(user_path_err(
                "inspect_slot_backup",
                error.to_string(),
                &paths.backup,
                true,
            )),
        }
    }
}

fn rollback_shared_root(paths: &SlotOperationPaths) -> Result<()> {
    let backup_exists = shared_artifact_exists(&paths.backup)?;
    if !backup_exists {
        remove_shared_artifact_if_exists(&paths.staging)?;
        return Ok(());
    }
    let Some(backup_inventory) = shared_backup_inventory(paths)? else {
        remove_shared_artifact_if_exists(&paths.staging)?;
        remove_shared_artifact_if_exists(&paths.backup)?;
        return Ok(());
    };

    ensure_shared_live_directory(&paths.live)?;
    remove_shared_artifact_if_exists(&paths.staging)?;
    std::fs::create_dir_all(&paths.staging)?;
    copy_shared_entries(&paths.backup, &paths.staging, false, true)?;
    if shared_artifact_inventory(&paths.staging)? != backup_inventory {
        return Err(package_err(
            "slot_restore_staging_verification_failed",
            "StarVault could not verify the staged Wings of Liberty rollback",
        ));
    }

    clear_shared_live(&paths.live)?;
    for entry in read_dir(&paths.staging)? {
        rename_with_retry(&entry.path(), &paths.live.join(entry.file_name()))?;
    }
    remove_shared_artifact_if_exists(&paths.staging)?;
    if shared_live_inventory(&paths.live)? != backup_inventory {
        return Err(package_err(
            "slot_restore_verification_failed",
            "StarVault could not verify the restored Wings of Liberty slot",
        ));
    }

    // Moving the verified backup before recursive cleanup leaves recovery an
    // unambiguous state if the process stops during deletion: backup absent,
    // restored live tree complete, and only disposable staging remains.
    rename_with_retry(&paths.backup, &paths.staging)?;
    remove_shared_artifact_if_exists(&paths.staging)
}

fn verify_shared_rollback_change(
    paths: &SlotOperationPaths,
    previous: Option<&PackageManifest>,
    target: Option<&PackageManifest>,
) -> Result<()> {
    validate_shared_artifact_if_exists(&paths.staging)?;
    let Some(backup) = shared_backup_inventory(paths)? else {
        let _ = shared_artifact_inventory(&paths.backup)?;
        return verify_slot_tree(paths.faction, &paths.live, previous);
    };
    if backup != manifest_shared_inventory(previous) {
        return Err(package_err(
            "slot_drift",
            "Wings of Liberty campaign-slot backup does not match the previous campaign",
        ));
    }
    verify_shared_live_subset(paths, target, &backup)
}

fn verify_shared_repair_rollback_change(
    paths: &SlotOperationPaths,
    target: Option<&PackageManifest>,
) -> Result<()> {
    validate_shared_artifact_if_exists(&paths.staging)?;
    let Some(backup) = shared_backup_inventory(paths)? else {
        let _ = shared_artifact_inventory(&paths.backup)?;
        let _ = shared_live_inventory(&paths.live)?;
        return Ok(());
    };
    verify_shared_live_subset(paths, target, &backup)
}

pub(crate) fn validate_journal_bindings(
    journal: &SlotOperationJournal,
    require_complete: bool,
) -> Result<()> {
    let previous = journal.previous_states();
    let target = journal.target_states();
    if !require_complete {
        return if previous.is_empty() && target.is_empty() {
            Ok(())
        } else {
            Err(package_err(
                "unsafe_operation_journal",
                "preparing operation unexpectedly contains campaign-slot state identities",
            ))
        };
    }
    let mut factions = Vec::with_capacity(journal.len());
    for slot in journal.iter() {
        if factions.contains(&slot.faction) {
            return Err(package_err(
                "unsafe_operation_journal",
                "operation journal contains duplicate campaign-slot paths",
            ));
        }
        factions.push(slot.faction);
    }
    if previous.len() != journal.len() || target.len() != journal.len() {
        return Err(package_err(
            "unsafe_operation_journal",
            "operation journal has incomplete campaign-slot state identities",
        ));
    }
    for slot in journal.iter() {
        let previous = unique_binding_for(previous, slot, "previous")?;
        let target = unique_binding_for(target, slot, "target")?;
        validate_repair_digest(&previous.sha256)?;
        validate_repair_digest(&target.sha256)?;
        if slot.faction == SlotId::Wol
            && (previous.kind != SlotStateKind::SharedDirectory
                || target.kind != SlotStateKind::SharedDirectory)
        {
            return Err(package_err(
                "unsafe_operation_journal",
                "Wings of Liberty state identities must describe the shared directory",
            ));
        }
        if slot.faction != SlotId::Wol
            && (previous.kind == SlotStateKind::SharedDirectory
                || target.kind == SlotStateKind::SharedDirectory
                || target.kind == SlotStateKind::Absent)
        {
            return Err(package_err(
                "unsafe_operation_journal",
                "dedicated campaign-slot state identity has an invalid object kind",
            ));
        }
    }
    Ok(())
}

fn unique_binding_for<'a>(
    bindings: &'a [SlotStateBinding],
    slot: &SlotOperationPaths,
    label: &str,
) -> Result<&'a SlotStateBinding> {
    let mut matches = bindings
        .iter()
        .filter(|binding| binding.faction == slot.faction);
    let binding = matches.next().ok_or_else(|| {
        package_err(
            "unsafe_operation_journal",
            format!("operation journal is missing the {label} state for a campaign slot"),
        )
    })?;
    if matches.next().is_some() {
        return Err(package_err(
            "unsafe_operation_journal",
            format!("operation journal has duplicate {label} campaign-slot states"),
        ));
    }
    Ok(binding)
}

fn previous_binding_for<'a>(
    journal: &'a SlotOperationJournal,
    slot: &SlotOperationPaths,
) -> Result<&'a SlotStateBinding> {
    let binding = unique_binding_for(journal.previous_states(), slot, "previous")?;
    validate_repair_digest(&binding.sha256)?;
    Ok(binding)
}

fn target_binding_for<'a>(
    journal: &'a SlotOperationJournal,
    slot: &SlotOperationPaths,
) -> Result<&'a SlotStateBinding> {
    let binding = unique_binding_for(journal.target_states(), slot, "target")?;
    validate_repair_digest(&binding.sha256)?;
    Ok(binding)
}

fn validate_repair_digest(digest: &str) -> Result<()> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(package_err(
        "unsafe_operation_journal",
        "repair journal contains an invalid campaign-slot backup digest",
    ))
}

fn verify_repair_backup_identity(
    paths: &SlotOperationPaths,
    binding: &SlotStateBinding,
) -> Result<()> {
    if binding.faction != paths.faction {
        return Err(package_err(
            "unsafe_operation_journal",
            "repair backup identity targets the wrong campaign slot",
        ));
    }
    let actual = match binding.kind {
        SlotStateKind::SharedDirectory => {
            if paths.faction != SlotId::Wol {
                return Err(package_err(
                    "unsafe_operation_journal",
                    "shared repair backup identity targets a dedicated campaign slot",
                ));
            }
            let marker = paths.backup.join(SHARED_BACKUP_READY);
            let metadata = std::fs::symlink_metadata(&marker).map_err(|error| {
                user_path_err(
                    "unsafe_slot_artifact",
                    format!("Wings of Liberty repair receipt is missing: {error}"),
                    &marker,
                    false,
                )
            })?;
            if is_link(&metadata) || !metadata.is_file() {
                return Err(user_path_err(
                    "unsafe_slot_artifact",
                    "Wings of Liberty repair receipt is not a regular file",
                    &marker,
                    false,
                ));
            }
            let bytes = std::fs::read(&marker).map_err(|error| {
                user_path_err("read_slot_backup_marker", error.to_string(), &marker, true)
            })?;
            let inventory = shared_backup_inventory(paths)?;
            if inventory.is_none() {
                return Err(user_path_err(
                    "unsafe_slot_artifact",
                    "Wings of Liberty repair receipt is missing",
                    &marker,
                    false,
                ));
            }
            sha256_bytes(&bytes)
        }
        SlotStateKind::Absent | SlotStateKind::Directory | SlotStateKind::Junction => {
            if paths.faction == SlotId::Wol {
                return Err(package_err(
                    "unsafe_operation_journal",
                    "dedicated repair backup identity targets Wings of Liberty",
                ));
            }
            dedicated_receipt_digest(&paths.backup, binding.kind)?
        }
    };
    verify_repair_digest_match(&binding.sha256, &actual, &paths.backup)
}

fn verify_repair_live_identity(
    paths: &SlotOperationPaths,
    binding: &SlotStateBinding,
) -> Result<()> {
    if binding.faction != paths.faction {
        return Err(package_err(
            "unsafe_operation_journal",
            "repair backup identity targets the wrong campaign slot",
        ));
    }
    let actual = match binding.kind {
        SlotStateKind::SharedDirectory => {
            let entries = shared_live_inventory(&paths.live)?;
            sha256_bytes(&shared_backup_receipt_bytes(&entries)?)
        }
        SlotStateKind::Absent => match std::fs::symlink_metadata(&paths.live) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                sha256_dedicated_receipt(&DedicatedRepairReceipt::Absent)?
            }
            Err(error) => {
                return Err(user_path_err(
                    "inspect_campaign_slot",
                    error.to_string(),
                    &paths.live,
                    true,
                ));
            }
            Ok(_) => {
                return Err(user_path_err(
                    "unsafe_slot_artifact",
                    "campaign slot should be absent for repair rollback",
                    &paths.live,
                    false,
                ));
            }
        },
        SlotStateKind::Directory | SlotStateKind::Junction => {
            dedicated_receipt_digest(&paths.live, binding.kind)?
        }
    };
    verify_repair_digest_match(&binding.sha256, &actual, &paths.live)
}

fn verify_recovery_live_identity(
    paths: &SlotOperationPaths,
    binding: &SlotStateBinding,
) -> Result<()> {
    verify_repair_live_identity(paths, binding).map_err(|error| {
        if error.code() == "unsafe_slot_artifact" {
            package_err(
                "slot_drift",
                format!("{} campaign slot changed during recovery", paths.faction),
            )
        } else {
            error
        }
    })
}

fn dedicated_receipt_digest(path: &Path, kind: SlotStateKind) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_slot_artifact", error.to_string(), path, true))?;
    let receipt = match kind {
        SlotStateKind::Absent => {
            if is_link(&metadata) || !metadata.is_dir() {
                return Err(unsafe_repair_artifact(path));
            }
            let entries = read_dir(path)?;
            if entries.len() != 1 || entries[0].file_name() != ABSENT_MARKER {
                return Err(unsafe_repair_artifact(path));
            }
            let marker = entries[0].path();
            let marker_metadata = std::fs::symlink_metadata(&marker).map_err(|error| {
                user_path_err("inspect_slot_artifact", error.to_string(), &marker, true)
            })?;
            if is_link(&marker_metadata) || !marker_metadata.is_file() || marker_metadata.len() != 0
            {
                return Err(unsafe_repair_artifact(&marker));
            }
            DedicatedRepairReceipt::Absent
        }
        SlotStateKind::Directory => {
            if is_link(&metadata) || !metadata.is_dir() {
                return Err(unsafe_repair_artifact(path));
            }
            DedicatedRepairReceipt::Directory {
                entries: inventory_shared_tree(path, false, false, true)?,
            }
        }
        SlotStateKind::Junction => {
            if !is_link(&metadata) {
                return Err(unsafe_repair_artifact(path));
            }
            dedicated_junction_receipt(path)?
        }
        SlotStateKind::SharedDirectory => {
            return Err(package_err(
                "unsafe_operation_journal",
                "shared repair identity used for a dedicated campaign slot",
            ));
        }
    };
    sha256_dedicated_receipt(&receipt)
}

fn binding_from_dedicated_receipt(
    faction: SlotId,
    kind: SlotStateKind,
    receipt: &DedicatedRepairReceipt,
) -> Result<SlotStateBinding> {
    Ok(SlotStateBinding {
        faction,
        kind,
        sha256: sha256_dedicated_receipt(receipt)?,
    })
}

fn sha256_dedicated_receipt(receipt: &DedicatedRepairReceipt) -> Result<String> {
    let bytes = serde_json::to_vec(receipt).map_err(|error| {
        internal_err(
            "serialize_slot_repair_receipt",
            "StarVault could not record the campaign-slot repair backup",
            error.to_string(),
        )
    })?;
    Ok(sha256_bytes(&bytes))
}

fn verify_repair_digest_match(expected: &str, actual: &str, path: &Path) -> Result<()> {
    if expected == actual {
        return Ok(());
    }
    Err(user_path_err(
        "unsafe_slot_artifact",
        "campaign-slot repair backup does not match the operation journal",
        path,
        false,
    ))
}

fn unsafe_repair_artifact(path: &Path) -> crate::Error {
    user_path_err(
        "unsafe_slot_artifact",
        "campaign-slot repair backup has an unexpected object type",
        path,
        false,
    )
}

fn canonicalize_slot_link(path: &Path) -> Result<PathBuf> {
    let target = std::fs::read_link(path).map_err(|error| {
        user_path_err(
            "unsafe_slot_artifact",
            format!("campaign-slot link or junction target cannot be read: {error}"),
            path,
            false,
        )
    })?;
    let target = if target.is_absolute() {
        target
    } else {
        path.parent()
            .ok_or_else(|| unsafe_repair_artifact(path))?
            .join(target)
    };
    canonicalize_target_allow_missing(&target)
}

fn canonicalize_target_allow_missing(path: &Path) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(user_path_err(
                "unsafe_slot_artifact",
                format!("campaign-slot target cannot be resolved: {error}"),
                path,
                false,
            ));
        }
    }
    let mut ancestor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&ancestor) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| unsafe_repair_artifact(path))?
                    .to_os_string();
                missing.push(name);
                if !ancestor.pop() {
                    return Err(unsafe_repair_artifact(path));
                }
            }
            Err(error) => {
                return Err(user_path_err(
                    "unsafe_slot_artifact",
                    format!("campaign-slot target ancestor cannot be resolved: {error}"),
                    &ancestor,
                    false,
                ));
            }
        }
    }
}

fn dedicated_junction_receipt(path: &Path) -> Result<DedicatedRepairReceipt> {
    let target = canonicalize_slot_link(path)?;
    let entries = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => inventory_shared_tree(path, false, false, true)?,
        Ok(_) => return Err(unsafe_repair_artifact(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => {
            return Err(user_path_err(
                "inspect_slot_artifact",
                error.to_string(),
                path,
                true,
            ));
        }
    };
    Ok(DedicatedRepairReceipt::Junction { target, entries })
}

fn capture_slot_state(faction: SlotId, path: &Path) -> Result<SlotStateBinding> {
    if faction == SlotId::Wol {
        let entries = shared_live_inventory(path)?;
        return Ok(SlotStateBinding {
            faction,
            kind: SlotStateKind::SharedDirectory,
            sha256: sha256_bytes(&shared_backup_receipt_bytes(&entries)?),
        });
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return binding_from_dedicated_receipt(
                faction,
                SlotStateKind::Absent,
                &DedicatedRepairReceipt::Absent,
            );
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_campaign_slot",
                error.to_string(),
                path,
                true,
            ));
        }
    };
    let receipt = if is_link(&metadata) {
        dedicated_junction_receipt(path)?
    } else if metadata.is_dir() {
        DedicatedRepairReceipt::Directory {
            entries: inventory_shared_tree(path, false, false, true)?,
        }
    } else {
        return Err(package_err(
            "slot_drift",
            "campaign slot root is not a regular directory",
        ));
    };
    let kind = match receipt {
        DedicatedRepairReceipt::Junction { .. } => SlotStateKind::Junction,
        DedicatedRepairReceipt::Directory { .. } => SlotStateKind::Directory,
        DedicatedRepairReceipt::Absent => SlotStateKind::Absent,
    };
    binding_from_dedicated_receipt(faction, kind, &receipt)
}

fn verify_staging_state_identity(
    paths: &SlotOperationPaths,
    binding: &SlotStateBinding,
) -> Result<()> {
    if binding.faction != paths.faction || binding.kind == SlotStateKind::Absent {
        return Err(package_err(
            "unsafe_operation_journal",
            "campaign-slot target identity does not match its staging path",
        ));
    }
    let metadata = std::fs::symlink_metadata(&paths.staging).map_err(|error| {
        user_path_err(
            "inspect_slot_artifact",
            error.to_string(),
            &paths.staging,
            true,
        )
    })?;
    let actual = if binding.kind == SlotStateKind::SharedDirectory {
        if is_link(&metadata) || !metadata.is_dir() || paths.faction != SlotId::Wol {
            return Err(user_path_err(
                "unsafe_slot_artifact",
                "campaign-slot staging has an unexpected object type",
                &paths.staging,
                false,
            ));
        }
        let entries = inventory_shared_tree(&paths.staging, true, false, true)?;
        sha256_bytes(&shared_backup_receipt_bytes(&entries)?)
    } else {
        if paths.faction == SlotId::Wol {
            return Err(package_err(
                "unsafe_operation_journal",
                "dedicated target identity is assigned to Wings of Liberty",
            ));
        }
        dedicated_receipt_digest(&paths.staging, binding.kind)?
    };
    verify_repair_digest_match(&binding.sha256, &actual, &paths.staging)
}

fn verify_shared_live_subset(
    paths: &SlotOperationPaths,
    target: Option<&PackageManifest>,
    backup: &BTreeMap<String, SharedTreeEntry>,
) -> Result<()> {
    let target = manifest_shared_inventory(target);
    for (relative, actual) in shared_live_inventory(&paths.live)? {
        if target.get(&relative) == Some(&actual) || backup.get(&relative) == Some(&actual) {
            continue;
        }
        return Err(package_err(
            "slot_drift",
            format!(
                "Wings of Liberty campaign slot contains changed or unknown entry `{relative}`"
            ),
        ));
    }
    Ok(())
}

fn manifest_shared_inventory(
    manifest: Option<&PackageManifest>,
) -> BTreeMap<String, SharedTreeEntry> {
    let mut inventory = BTreeMap::new();
    for file in manifest
        .into_iter()
        .flat_map(|manifest| manifest.files.iter())
    {
        let Some(relative) = file.path.strip_prefix("slot/") else {
            continue;
        };
        let relative = relative.replace('\\', "/");
        let segments = relative.split('/').collect::<Vec<_>>();
        for end in 1..segments.len() {
            inventory.insert(segments[..end].join("/"), SharedTreeEntry::Directory);
        }
        inventory.insert(
            relative,
            SharedTreeEntry::File {
                sha256: file.sha256.clone(),
                size: file.size,
            },
        );
    }
    inventory
}

fn ensure_shared_live_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) || !metadata.is_dir() => Err(package_err(
            "slot_drift",
            "Wings of Liberty campaign slot root is not a regular directory",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path)
            .map_err(|error| user_path_err("create_campaign_slot", error.to_string(), path, true)),
        Err(error) => Err(user_path_err(
            "inspect_campaign_slot",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn shared_artifact_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) || !metadata.is_dir() => Err(user_path_err(
            "unsafe_slot_artifact",
            "campaign-slot recovery artifact is not a regular directory",
            path,
            false,
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_path_err(
            "inspect_slot_artifact",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn write_shared_backup_receipt(
    paths: &SlotOperationPaths,
    entries: &BTreeMap<String, SharedTreeEntry>,
) -> Result<()> {
    let bytes = shared_backup_receipt_bytes(entries)?;
    crate::atomic_file::write(&paths.backup.join(SHARED_BACKUP_READY), &bytes)
}

fn shared_backup_receipt_bytes(entries: &BTreeMap<String, SharedTreeEntry>) -> Result<Vec<u8>> {
    let receipt = SharedBackupReceipt {
        version: 1,
        entries: entries.clone(),
    };
    serde_json::to_vec_pretty(&receipt).map_err(|error| {
        internal_err(
            "serialize_slot_backup_receipt",
            "StarVault could not record the Wings of Liberty slot backup",
            error.to_string(),
        )
    })
}

fn shared_backup_inventory(
    paths: &SlotOperationPaths,
) -> Result<Option<BTreeMap<String, SharedTreeEntry>>> {
    if !shared_artifact_exists(&paths.backup)? {
        return Ok(None);
    }
    let marker = paths.backup.join(SHARED_BACKUP_READY);
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) if is_link(&metadata) || !metadata.is_file() => Err(user_path_err(
            "unsafe_slot_artifact",
            "Wings of Liberty backup marker is not a regular file",
            marker,
            false,
        )),
        Ok(_) => {
            let bytes = std::fs::read(&marker).map_err(|error| {
                user_path_err("read_slot_backup_marker", error.to_string(), &marker, true)
            })?;
            let receipt: SharedBackupReceipt = serde_json::from_slice(&bytes).map_err(|error| {
                user_path_err(
                    "unsafe_slot_artifact",
                    format!("Wings of Liberty backup receipt is unreadable: {error}"),
                    &marker,
                    false,
                )
            })?;
            if receipt.version != 1 {
                return Err(user_path_err(
                    "unsafe_slot_artifact",
                    "Wings of Liberty backup receipt has an unsupported version",
                    &marker,
                    false,
                ));
            }
            let actual = shared_artifact_inventory_ignoring_ready(&paths.backup)?;
            if actual != receipt.entries {
                return Err(user_path_err(
                    "unsafe_slot_artifact",
                    "Wings of Liberty backup contents have changed",
                    &paths.backup,
                    false,
                ));
            }
            Ok(Some(receipt.entries))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(user_path_err(
            "inspect_slot_backup_marker",
            error.to_string(),
            marker,
            true,
        )),
    }
}

fn shared_live_inventory(path: &Path) -> Result<BTreeMap<String, SharedTreeEntry>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link(&metadata) || !metadata.is_dir() => {
            return Err(package_err(
                "slot_drift",
                "Wings of Liberty campaign slot root is not a regular directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_campaign_slot",
                error.to_string(),
                path,
                true,
            ));
        }
    }
    inventory_shared_tree(path, true, false, false)
}

fn shared_artifact_inventory(path: &Path) -> Result<BTreeMap<String, SharedTreeEntry>> {
    if !shared_artifact_exists(path)? {
        return Err(user_path_err(
            "missing_slot_artifact",
            "campaign-slot recovery artifact is missing",
            path,
            false,
        ));
    }
    inventory_shared_tree(path, false, false, true)
}

fn shared_artifact_inventory_ignoring_ready(
    path: &Path,
) -> Result<BTreeMap<String, SharedTreeEntry>> {
    if !shared_artifact_exists(path)? {
        return Err(user_path_err(
            "missing_slot_artifact",
            "campaign-slot recovery artifact is missing",
            path,
            false,
        ));
    }
    inventory_shared_tree(path, false, true, true)
}

fn validate_shared_artifact_if_exists(path: &Path) -> Result<()> {
    if shared_artifact_exists(path)? {
        let _ = inventory_shared_tree(path, false, false, true)?;
    }
    Ok(())
}

fn inventory_shared_tree(
    root: &Path,
    protect_shared_root: bool,
    ignore_ready_marker: bool,
    artifact: bool,
) -> Result<BTreeMap<String, SharedTreeEntry>> {
    let mut inventory = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in read_dir(&directory)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if directory == root
                && ((protect_shared_root && protected_shared_entry(&name))
                    || (ignore_ready_marker && name == SHARED_BACKUP_READY))
            {
                continue;
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                user_path_err("inspect_slot_tree_entry", error.to_string(), &path, true)
            })?;
            if is_link(&metadata) {
                return Err(unsafe_shared_tree_entry(&path, artifact));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|error| {
                    internal_err(
                        "slot_path_outside_root",
                        "StarVault could not verify the campaign slot",
                        error.to_string(),
                    )
                })?
                .to_string_lossy()
                .replace('\\', "/");
            if metadata.is_dir() {
                inventory.insert(relative, SharedTreeEntry::Directory);
                stack.push(path);
            } else if metadata.is_file() {
                inventory.insert(
                    relative,
                    SharedTreeEntry::File {
                        sha256: hash_file(&path)?,
                        size: metadata.len(),
                    },
                );
            } else {
                return Err(unsafe_shared_tree_entry(&path, artifact));
            }
        }
    }
    Ok(inventory)
}

fn unsafe_shared_tree_entry(path: &Path, artifact: bool) -> crate::Error {
    if artifact {
        user_path_err(
            "unsafe_slot_artifact",
            "campaign-slot recovery artifact contains a link or unsupported object",
            path,
            false,
        )
    } else {
        package_err(
            "slot_drift",
            "Wings of Liberty campaign slot contains a link or unsupported object",
        )
    }
}

fn copy_shared_entries(
    source: &Path,
    destination: &Path,
    protect_shared_root: bool,
    skip_ready_marker: bool,
) -> Result<()> {
    for entry in read_dir(source)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if (protect_shared_root && protected_shared_entry(&name))
            || (skip_ready_marker && name == SHARED_BACKUP_READY)
        {
            continue;
        }
        copy_shared_entry(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_shared_entry(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source).map_err(|error| {
        user_path_err("inspect_slot_copy_source", error.to_string(), source, true)
    })?;
    if is_link(&metadata) {
        return Err(user_path_err(
            "unsafe_slot_artifact",
            "campaign-slot tree contains a link",
            source,
            false,
        ));
    }
    if metadata.is_dir() {
        std::fs::create_dir(destination).map_err(|error| {
            user_path_err(
                "create_slot_copy_directory",
                error.to_string(),
                destination,
                true,
            )
        })?;
        for child in read_dir(source)? {
            copy_shared_entry(&child.path(), &destination.join(child.file_name()))?;
        }
        Ok(())
    } else if metadata.is_file() {
        std::fs::copy(source, destination)
            .map(|_| ())
            .map_err(|error| {
                user_path_err("copy_campaign_slot", error.to_string(), destination, true)
            })
    } else {
        Err(user_path_err(
            "unsafe_slot_artifact",
            "campaign-slot tree contains an unsupported object",
            source,
            false,
        ))
    }
}

fn clear_shared_live(live: &Path) -> Result<()> {
    let _ = shared_live_inventory(live)?;
    for entry in read_dir(live)? {
        if protected_shared_entry(&entry.file_name().to_string_lossy()) {
            continue;
        }
        remove_entry(&entry.path())?;
    }
    Ok(())
}

fn remove_shared_artifact_if_exists(path: &Path) -> Result<()> {
    if !shared_artifact_exists(path)? {
        return Ok(());
    }
    let _ = shared_artifact_inventory(path)?;
    remove_entry(path)
}

fn verify_slot_tree(
    faction: SlotId,
    root: &Path,
    manifest: Option<&PackageManifest>,
) -> Result<()> {
    let expected: BTreeMap<String, (&str, u64)> = manifest
        .into_iter()
        .flat_map(|manifest| &manifest.files)
        .filter_map(|file| {
            file.path
                .strip_prefix("slot/")
                .map(|path| (path.replace('\\', "/"), (file.sha256.as_str(), file.size)))
        })
        .collect();
    let actual = inventory_slot_files(faction, root)?;
    if actual.len() != expected.len() {
        return Err(package_err(
            "slot_drift",
            format!(
                "{} campaign slot has {} files; {} were expected",
                faction,
                actual.len(),
                expected.len()
            ),
        ));
    }
    for (relative, path) in actual {
        let Some((sha256, size)) = expected.get(&relative) else {
            return Err(package_err(
                "slot_drift",
                format!(
                    "{} campaign slot contains unowned file `{relative}`",
                    faction
                ),
            ));
        };
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            user_path_err("inspect_campaign_slot_file", error.to_string(), &path, true)
        })?;
        if !metadata.file_type().is_file() || metadata.len() != *size {
            return Err(package_err(
                "slot_drift",
                format!("{} campaign slot file `{relative}` has changed", faction),
            ));
        }
        if hash_file(&path)? != *sha256 {
            return Err(package_err(
                "slot_drift",
                format!("{} campaign slot file `{relative}` has changed", faction),
            ));
        }
    }
    Ok(())
}

fn inventory_slot_files(faction: SlotId, root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    match root.symlink_metadata() {
        Ok(metadata) => {
            if faction == SlotId::Wol && (is_link(&metadata) || !metadata.is_dir()) {
                return Err(package_err(
                    "slot_drift",
                    "Wings of Liberty campaign slot root is not a regular directory",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_campaign_slot",
                error.to_string(),
                root,
                true,
            ));
        }
    }
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in read_dir(&directory)? {
            if faction == SlotId::Wol && directory == root {
                let name = entry.file_name().to_string_lossy().into_owned();
                if protected_shared_entry(&name) {
                    continue;
                }
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                user_path_err("inspect_campaign_slot", error.to_string(), &path, true)
            })?;
            if is_link(&metadata) {
                return Err(package_err(
                    "slot_drift",
                    format!("{} campaign slot contains an unsupported link", faction),
                ));
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|error| {
                        internal_err(
                            "slot_path_outside_root",
                            "StarVault could not verify the campaign slot",
                            error.to_string(),
                        )
                    })?
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, path);
            } else {
                return Err(package_err(
                    "slot_drift",
                    format!("{} campaign slot contains an unsupported link", faction),
                ));
            }
        }
    }
    Ok(files)
}

fn protected_shared_entry(name: &str) -> bool {
    SLOT_OWNED_SIBLINGS
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(name))
        || SLOT_OWNED_SIBLINGS
            .iter()
            .filter(|owned| !owned.eq_ignore_ascii_case("voidprologue"))
            .any(|owned| {
                ["staging", "backup"]
                    .into_iter()
                    .any(|kind| shared_operation_artifact(name, owned, kind))
            })
}

fn shared_operation_artifact(name: &str, owned: &str, kind: &str) -> bool {
    let prefix = format!("{owned}.{kind}-");
    let Some(actual_prefix) = name.get(..prefix.len()) else {
        return false;
    };
    let Some(operation_id) = name.get(prefix.len()..) else {
        return false;
    };
    actual_prefix.eq_ignore_ascii_case(&prefix) && valid_operation_id(operation_id)
}

fn sibling_path(path: &Path, kind: &str, operation_id: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{kind}-{operation_id}"))
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if !valid_operation_id(operation_id) {
        return Err(internal_err(
            "invalid_operation_id",
            "StarVault could not prepare the campaign slot",
            "operation id is not a safe path component",
        ));
    }
    Ok(())
}

fn valid_operation_id(operation_id: &str) -> bool {
    !operation_id.is_empty()
        && operation_id.len() <= 96
        && operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn ensure_absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(user_path_err(
            "operation_path_collision",
            "a previous operation left a staging or backup path",
            path,
            false,
        )),
        Err(error) => Err(user_path_err(
            "inspect_operation_path",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn read_dir(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| user_path_err("read_campaign_slot", error.to_string(), path, true))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| user_path_err("read_campaign_slot", error.to_string(), path, true))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn rename_with_retry(source: &Path, destination: &Path) -> Result<()> {
    retry_filesystem(destination, || std::fs::rename(source, destination))
}

fn retry_filesystem(path: &Path, mut operation: impl FnMut() -> std::io::Result<()>) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..4 {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable = matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                );
                if !retryable || attempt == 3 {
                    return Err(user_path_err(
                        "campaign_slot_locked",
                        error.to_string(),
                        path,
                        retryable,
                    ));
                }
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
    Err(user_path_err(
        "campaign_slot_locked",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "filesystem operation failed".into()),
        path,
        true,
    ))
}

fn remove_entry_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => remove_entry(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_path_err(
            "inspect_slot_artifact",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn remove_entry(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_slot_artifact", error.to_string(), path, true))?;
    let result = if is_link(&metadata) {
        remove_link(path)
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|error| {
        user_path_err(
            "remove_slot_artifact",
            error.to_string(),
            path,
            matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
            ),
        )
    })
}

#[cfg(windows)]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path)
        .map_err(|error| user_path_err("open_slot_file", error.to_string(), path, true))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| user_path_err("read_slot_file", error.to_string(), path, true))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(bytes))
}

#[cfg(windows)]
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    junction::create(target, link)
}

#[cfg(not(windows))]
fn make_junction(_link: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "directory junctions require Windows",
    ))
}

#[cfg(windows)]
fn remove_link(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(path)
}

#[cfg(not(windows))]
fn remove_link(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_entries_are_exact_owned_factions_or_operation_artifacts() {
        assert!(protected_shared_entry("swarm"));
        assert!(protected_shared_entry("VOIDPROLOGUE"));
        assert!(protected_shared_entry("SwArM.staging-op-1"));
        assert!(protected_shared_entry("NOVA.backup-A1"));

        assert!(!protected_shared_entry(SHARED_BACKUP_READY));
        assert!(!protected_shared_entry("voidprologue.backup-A1"));
        assert!(!protected_shared_entry("Campaign.backup-op"));
        assert!(!protected_shared_entry("rogue.staging-op"));
        assert!(!protected_shared_entry("rogue.backup-op"));
        assert!(!protected_shared_entry("myvoid.backup-op"));
        assert!(!protected_shared_entry("void.user.backup-op"));
        assert!(!protected_shared_entry("void.backup-"));
        assert!(!protected_shared_entry("void.backup-invalid.op"));
        assert!(!protected_shared_entry(&format!(
            "void.backup-{}",
            "a".repeat(97)
        )));
        assert!(!protected_shared_entry("custom.SC2Map"));
    }

    #[test]
    fn dedicated_absent_slot_rolls_back_to_absent() {
        let directory = tempfile::tempdir().unwrap();
        let live = directory.path().join("void");
        let staging = directory.path().join("void.staging-op");
        let backup = directory.path().join("void.backup-op");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("target"), b"target").unwrap();
        let paths = SlotOperationPaths {
            faction: SlotId::LotV,
            live: live.clone(),
            staging,
            backup,
        };
        let previous_state = capture_slot_state(SlotId::LotV, &paths.live).unwrap();
        let target_state = capture_slot_state(SlotId::LotV, &paths.staging).unwrap();
        apply_dedicated_slot(&paths, &previous_state, &target_state, true).unwrap();
        assert!(live.join("target").is_file());
        rollback_dedicated_slot(&paths).unwrap();
        assert!(live.symlink_metadata().is_err());
    }

    #[test]
    fn finalize_attempts_every_artifact_after_an_earlier_cleanup_error() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(not(windows))]
        let non_directory = directory.path().join("not-a-directory");
        #[cfg(not(windows))]
        std::fs::write(&non_directory, b"file").unwrap();
        #[cfg(not(windows))]
        let failing_staging = non_directory.join("child");
        #[cfg(windows)]
        let failing_staging = {
            let path = directory.path().join("locked-staging");
            std::fs::write(&path, b"locked for deletion").unwrap();
            path
        };
        #[cfg(windows)]
        let locked_staging = {
            use std::os::windows::fs::OpenOptionsExt;

            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&failing_staging)
                .unwrap()
        };
        let first_backup = directory.path().join("first-backup");
        let second_staging = directory.path().join("second-staging");
        let second_backup = directory.path().join("second-backup");
        for artifact in [&first_backup, &second_staging, &second_backup] {
            std::fs::create_dir(artifact).unwrap();
        }
        let paths = SlotOperationJournal::new(
            vec![
                SlotOperationPaths {
                    faction: SlotId::HotS,
                    live: directory.path().join("first-live"),
                    staging: failing_staging.clone(),
                    backup: first_backup.clone(),
                },
                SlotOperationPaths {
                    faction: SlotId::LotV,
                    live: directory.path().join("second-live"),
                    staging: second_staging.clone(),
                    backup: second_backup.clone(),
                },
            ],
            Vec::new(),
            Vec::new(),
        );

        assert!(finalize_paths(&paths).is_err());
        assert!(!first_backup.exists());
        assert!(!second_staging.exists());
        assert!(!second_backup.exists());
        #[cfg(windows)]
        {
            drop(locked_staging);
            std::fs::remove_file(failing_staging).unwrap();
        }
    }

    #[test]
    fn journal_binding_validation_rejects_preparing_evidence_and_duplicate_factions() {
        let directory = tempfile::tempdir().unwrap();
        let slot = SlotOperationPaths {
            faction: SlotId::LotV,
            live: directory.path().join("void"),
            staging: directory.path().join("void.staging-op"),
            backup: directory.path().join("void.backup-op"),
        };
        let previous = SlotStateBinding {
            faction: SlotId::LotV,
            kind: SlotStateKind::Absent,
            sha256: "a".repeat(64),
        };
        let target = SlotStateBinding {
            faction: SlotId::LotV,
            kind: SlotStateKind::Directory,
            sha256: "b".repeat(64),
        };
        let populated = SlotOperationJournal::new(
            vec![slot.clone()],
            vec![previous.clone()],
            vec![target.clone()],
        );
        let error = validate_journal_bindings(&populated, false).unwrap_err();
        assert_eq!(error.code(), "unsafe_operation_journal");

        let duplicate = SlotOperationJournal::new(
            vec![
                slot.clone(),
                SlotOperationPaths {
                    live: directory.path().join("other-live"),
                    staging: directory.path().join("other-staging"),
                    backup: directory.path().join("other-backup"),
                    ..slot
                },
            ],
            vec![previous.clone(), previous],
            vec![target.clone(), target],
        );
        let error = validate_journal_bindings(&duplicate, true).unwrap_err();
        assert_eq!(error.code(), "unsafe_operation_journal");
    }
}
