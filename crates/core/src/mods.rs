//! Reversible deployment of the single active campaign's `Mods` files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{internal_err, package_err, user_path_err, Result};
use crate::store::{ManagedMod, ManagedModDisposition, PackageManifest, Store};

const PLAN_FILE: &str = "mods-plan.json";
const BACKUP_FILES: &str = "files";
const TEMPORARY_PREFIX: &str = ".mods-copy-";
const APPLY_STARTED: &str = ".apply-started";
const APPLY_COMPLETE: &str = ".apply-complete";
const MAX_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const RETRY_ATTEMPTS: usize = 8;
const RETRY_BASE_MS: u64 = 25;

#[cfg(windows)]
type PlanOpenIdentity = File;

#[cfg(not(windows))]
struct PlanOpenIdentity;

#[derive(Debug, Clone)]
pub struct PreparedModsTransition {
    mods_root: PathBuf,
    staging: PathBuf,
    backup: PathBuf,
    plan_sha256: String,
    plan: ModsPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalModsPolicy {
    Reject,
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModsPlan {
    previous: Vec<ManagedMod>,
    target: Vec<ManagedMod>,
    backed_up: Vec<String>,
    #[serde(default)]
    repair: bool,
    #[serde(default)]
    repair_originals: Vec<RepairOriginal>,
    /// External files the user explicitly allowed this operation to replace.
    /// They remain in the operation backup for rollback, then are discarded
    /// once the activation commits.
    #[serde(default)]
    replaced_external: Vec<RepairOriginal>,
    /// Complete top-level `.SC2Mod` files or directories that can be renamed
    /// from the sibling staging tree instead of copied a second time.
    #[serde(default)]
    atomic_units: Vec<AtomicModUnit>,
    /// Complete live `.SC2Mod` files or directories moved into the operation
    /// backup when apply starts. Rollback renames them back into place.
    #[serde(default)]
    backup_units: Vec<AtomicModUnit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepairOriginal {
    path: String,
    sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AtomicModUnitKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AtomicModUnit {
    path: String,
    kind: AtomicModUnitKind,
}

impl PreparedModsTransition {
    pub fn prepare(
        store: &Store,
        mods_root: &Path,
        previous: &[ManagedMod],
        target: Option<&PackageManifest>,
        operation_id: &str,
    ) -> Result<Self> {
        Self::prepare_with_policy(
            store,
            mods_root,
            previous,
            target,
            operation_id,
            ExternalModsPolicy::Reject,
        )
    }

    pub fn prepare_with_policy(
        store: &Store,
        mods_root: &Path,
        previous: &[ManagedMod],
        target: Option<&PackageManifest>,
        operation_id: &str,
        external_policy: ExternalModsPolicy,
    ) -> Result<Self> {
        Self::prepare_with_policy_and_verification(
            store,
            mods_root,
            previous,
            target,
            operation_id,
            external_policy,
            true,
        )
    }

    pub(crate) fn prepare_preverified_with_policy(
        store: &Store,
        mods_root: &Path,
        previous: &[ManagedMod],
        target: Option<&PackageManifest>,
        operation_id: &str,
        external_policy: ExternalModsPolicy,
    ) -> Result<Self> {
        Self::prepare_with_policy_and_verification(
            store,
            mods_root,
            previous,
            target,
            operation_id,
            external_policy,
            false,
        )
    }

    fn prepare_with_policy_and_verification(
        store: &Store,
        mods_root: &Path,
        previous: &[ManagedMod],
        target: Option<&PackageManifest>,
        operation_id: &str,
        external_policy: ExternalModsPolicy,
        verify_contents: bool,
    ) -> Result<Self> {
        validate_operation_id(operation_id)?;
        verify_mods_root(mods_root)?;
        if verify_contents {
            verify_managed(mods_root, previous)?;
        } else {
            verify_managed_shape(mods_root, previous)?;
        }
        let staging = sibling_path(mods_root, "staging", operation_id);
        let backup = sibling_path(mods_root, "backup", operation_id);
        ensure_absent(&staging)?;
        ensure_absent(&backup)?;
        std::fs::create_dir_all(&staging)?;
        std::fs::create_dir_all(backup.join(BACKUP_FILES))?;

        let prepared = (|| -> Result<Self> {
            let (target_rows, mut replaced_external) =
                plan_target(mods_root, previous, target, external_policy)?;
            for original in &replaced_external {
                let source = join_relative(mods_root, &original.path)?;
                let destination = join_relative(&backup.join(BACKUP_FILES), &original.path)?;
                copy_managed_file(mods_root, &source, &backup, &destination)?;
                ensure_artifact_regular_file(&backup, &destination, "Mods backup")?;
                if hash_file(&destination)? != original.sha256.as_deref().unwrap_or_default() {
                    return Err(internal_err(
                        "mods_backup_verification_failed",
                        "StarVault could not back up the external Mods file",
                        format!("backup of `{}` changed", original.path),
                    ));
                }
            }
            replaced_external.sort_by_key(|original| key(&original.path));
            if let Some(target) = target {
                store.materialize_mods(target, &staging)?;
            }
            let (backup_units, atomic_units) =
                plan_atomic_units(mods_root, previous, &target_rows, &replaced_external)?;
            let target_by_key: BTreeMap<String, &ManagedMod> = target_rows
                .iter()
                .map(|managed| (key(&managed.path), managed))
                .collect();
            let mut backed_up = Vec::new();
            for managed in previous {
                if managed.disposition != ManagedModDisposition::Created {
                    continue;
                }
                if backup_units
                    .iter()
                    .any(|unit| atomic_unit_contains(&unit.path, &managed.path))
                {
                    continue;
                }
                let changed = target_by_key
                    .get(&key(&managed.path))
                    .is_none_or(|target| target.sha256 != managed.sha256);
                if !changed {
                    continue;
                }
                let source = join_relative(mods_root, &managed.path)?;
                let destination = join_relative(&backup.join(BACKUP_FILES), &managed.path)?;
                copy_managed_file(mods_root, &source, &backup, &destination)?;
                ensure_artifact_regular_file(&backup, &destination, "Mods backup")?;
                if hash_file(&destination)? != managed.sha256 {
                    return Err(internal_err(
                        "mods_backup_verification_failed",
                        "StarVault could not back up the active Mods files",
                        format!("backup of `{}` did not match its ledger hash", managed.path),
                    ));
                }
                backed_up.push(managed.path.clone());
            }
            backed_up.sort_by_key(|path| key(path));
            let plan = ModsPlan {
                previous: previous.to_vec(),
                target: target_rows,
                backed_up,
                repair: false,
                repair_originals: Vec::new(),
                replaced_external,
                atomic_units,
                backup_units,
            };
            persist_plan(&backup, &plan)?;
            let plan_path = backup.join(PLAN_FILE);
            ensure_artifact_regular_file(&backup, &plan_path, "Mods backup")?;
            let plan_sha256 = hash_file(&plan_path)?;
            Ok(Self {
                mods_root: mods_root.to_path_buf(),
                staging: staging.clone(),
                backup: backup.clone(),
                plan_sha256,
                plan,
            })
        })();
        if prepared.is_err() {
            let _ = remove_entry_if_exists(&staging);
            let _ = remove_entry_if_exists(&backup);
        }
        prepared
    }

    /// Stage an explicit repair of created files. Borrowed files remain
    /// external property and must still match; a changed borrowed file blocks
    /// repair instead of being overwritten.
    pub fn prepare_repair(
        store: &Store,
        mods_root: &Path,
        previous: &[ManagedMod],
        target: &PackageManifest,
        operation_id: &str,
    ) -> Result<Self> {
        validate_operation_id(operation_id)?;
        verify_mods_root(mods_root)?;
        let staging = sibling_path(mods_root, "staging", operation_id);
        let backup = sibling_path(mods_root, "backup", operation_id);
        ensure_absent(&staging)?;
        ensure_absent(&backup)?;
        std::fs::create_dir_all(&staging)?;
        std::fs::create_dir_all(backup.join(BACKUP_FILES))?;

        let prepared = (|| -> Result<Self> {
            store.materialize_mods(target, &staging)?;
            let (target_rows, replaced_external) = plan_target(
                mods_root,
                previous,
                Some(target),
                ExternalModsPolicy::Reject,
            )?;
            debug_assert!(replaced_external.is_empty());
            if target_rows.len() != previous.len()
                || target_rows.iter().zip(previous).any(|(target, previous)| {
                    key(&target.path) != key(&previous.path)
                        || target.sha256 != previous.sha256
                        || target.disposition != previous.disposition
                })
            {
                return Err(internal_err(
                    "managed_mods_manifest_mismatch",
                    "StarVault could not repair the active Mods files",
                    "managed Mods rows do not match the active package manifest",
                ));
            }

            let mut originals = Vec::new();
            for managed in previous {
                let path = join_relative(mods_root, &managed.path)?;
                if managed.disposition == ManagedModDisposition::Borrowed {
                    verify_managed(mods_root, std::slice::from_ref(managed))?;
                    continue;
                }
                ensure_managed_ancestors(mods_root, &path)?;
                match std::fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        originals.push(RepairOriginal {
                            path: managed.path.clone(),
                            sha256: None,
                        });
                    }
                    Err(error) => {
                        return Err(user_path_err(
                            "inspect_managed_mod",
                            error.to_string(),
                            &path,
                            true,
                        ));
                    }
                    Ok(metadata) if metadata.file_type().is_file() && !is_link(&metadata) => {
                        let actual_hash = hash_file(&path)?;
                        let destination = join_relative(&backup.join(BACKUP_FILES), &managed.path)?;
                        copy_managed_file(mods_root, &path, &backup, &destination)?;
                        ensure_artifact_regular_file(&backup, &destination, "Mods backup")?;
                        if hash_file(&destination)? != actual_hash {
                            return Err(internal_err(
                                "mods_backup_verification_failed",
                                "StarVault could not back up the drifted Mods files",
                                format!("repair backup of `{}` changed", managed.path),
                            ));
                        }
                        originals.push(RepairOriginal {
                            path: managed.path.clone(),
                            sha256: Some(actual_hash),
                        });
                    }
                    Ok(_) => {
                        return Err(package_err(
                            "repair_unsafe_managed_entry",
                            format!(
                                "managed Mods path `{}` is no longer a regular file",
                                managed.path
                            ),
                        ));
                    }
                }
            }
            originals.sort_by_key(|original| key(&original.path));
            let plan = ModsPlan {
                previous: previous.to_vec(),
                target: target_rows,
                backed_up: Vec::new(),
                repair: true,
                repair_originals: originals,
                replaced_external: Vec::new(),
                atomic_units: Vec::new(),
                backup_units: Vec::new(),
            };
            persist_plan(&backup, &plan)?;
            let plan_path = backup.join(PLAN_FILE);
            ensure_artifact_regular_file(&backup, &plan_path, "Mods backup")?;
            let plan_sha256 = hash_file(&plan_path)?;
            Ok(Self {
                mods_root: mods_root.to_path_buf(),
                staging: staging.clone(),
                backup: backup.clone(),
                plan_sha256,
                plan,
            })
        })();
        if prepared.is_err() {
            let _ = remove_entry_if_exists(&staging);
            let _ = remove_entry_if_exists(&backup);
        }
        prepared
    }

    pub fn staging_path(&self) -> PathBuf {
        self.staging.clone()
    }

    pub fn backup_path(&self) -> PathBuf {
        self.backup.clone()
    }

    pub fn plan_sha256(&self) -> &str {
        &self.plan_sha256
    }

    pub fn target_rows(&self) -> &[ManagedMod] {
        &self.plan.target
    }

    pub fn apply(&self) -> Result<()> {
        write_artifact(&self.backup, APPLY_STARTED, b"started")?;
        let result = if self.plan.repair {
            self.apply_repair()
        } else {
            self.apply_standard()
        };
        if result.is_ok() {
            write_artifact(&self.backup, APPLY_COMPLETE, b"complete")?;
        }
        result
    }

    fn apply_standard(&self) -> Result<()> {
        verify_managed_shape(&self.mods_root, &self.plan.previous)?;
        for original in &self.plan.replaced_external {
            let expected = original.sha256.as_deref().ok_or_else(|| {
                internal_err(
                    "mods_backup_plan_invalid",
                    "StarVault could not replace the external Mods file",
                    format!("external file `{}` has no recorded hash", original.path),
                )
            })?;
            let path = join_relative(&self.mods_root, &original.path)?;
            remove_managed_regular_if_hash(&self.mods_root, &path, expected)?;
            prune_empty_parents(path.parent(), &self.mods_root)?;
        }
        for unit in &self.plan.backup_units {
            move_live_unit_to_backup(&self.mods_root, &self.backup, unit, &self.plan.previous)?;
        }
        let target_by_key: BTreeMap<String, &ManagedMod> = self
            .plan
            .target
            .iter()
            .map(|managed| (key(&managed.path), managed))
            .collect();

        let mut previous_created: Vec<&ManagedMod> = self
            .plan
            .previous
            .iter()
            .filter(|managed| managed.disposition == ManagedModDisposition::Created)
            .filter(|managed| {
                target_by_key
                    .get(&key(&managed.path))
                    .is_none_or(|target| target.sha256 != managed.sha256)
            })
            .filter(|managed| {
                !self
                    .plan
                    .backup_units
                    .iter()
                    .any(|unit| atomic_unit_contains(&unit.path, &managed.path))
            })
            .collect();
        previous_created.sort_by_key(|managed| std::cmp::Reverse(path_depth(&managed.path)));
        for managed in previous_created {
            let path = join_relative(&self.mods_root, &managed.path)?;
            remove_managed_regular_if_hash(&self.mods_root, &path, &managed.sha256)?;
            prune_empty_parents(path.parent(), &self.mods_root)?;
        }

        for unit in &self.plan.atomic_units {
            move_atomic_unit(&self.staging, &self.mods_root, unit, &self.plan.target)?;
        }

        for managed in &self.plan.target {
            let previous = self
                .plan
                .previous
                .iter()
                .find(|candidate| key(&candidate.path) == key(&managed.path));
            let unchanged = previous.is_some_and(|previous| {
                previous.sha256 == managed.sha256 && previous.disposition == managed.disposition
            });
            let moved_atomically = self
                .plan
                .atomic_units
                .iter()
                .any(|unit| atomic_unit_contains(&unit.path, &managed.path));
            if managed.disposition == ManagedModDisposition::Borrowed
                || unchanged
                || moved_atomically
            {
                continue;
            }
            let source = join_relative(&self.staging, &managed.path)?;
            let destination = join_relative(&self.mods_root, &managed.path)?;
            prepare_owned_target(&destination, &self.mods_root, &self.plan.previous)?;
            copy_atomic(
                &self.staging,
                &source,
                &self.mods_root,
                &destination,
                &self.backup,
                &managed.path,
            )?;
        }
        verify_managed_shape(&self.mods_root, &self.plan.target)
    }

    fn apply_repair(&self) -> Result<()> {
        verify_repair_originals(&self.mods_root, &self.plan)?;
        let mut created: Vec<&ManagedMod> = self
            .plan
            .target
            .iter()
            .filter(|managed| managed.disposition == ManagedModDisposition::Created)
            .collect();
        created.sort_by_key(|managed| std::cmp::Reverse(path_depth(&managed.path)));
        for managed in &created {
            let path = join_relative(&self.mods_root, &managed.path)?;
            ensure_managed_ancestors(&self.mods_root, &path)?;
            remove_entry_if_exists(&path)?;
            prune_empty_parents(path.parent(), &self.mods_root)?;
        }
        created.sort_by_key(|managed| path_depth(&managed.path));
        for managed in created {
            let source = join_relative(&self.staging, &managed.path)?;
            let destination = join_relative(&self.mods_root, &managed.path)?;
            prepare_owned_target(&destination, &self.mods_root, &[])?;
            copy_atomic(
                &self.staging,
                &source,
                &self.mods_root,
                &destination,
                &self.backup,
                &managed.path,
            )?;
        }
        verify_managed_shape(&self.mods_root, &self.plan.target)
    }

    pub fn rollback(&self) -> Result<()> {
        rollback_from_paths(&self.mods_root, &self.backup, &self.staging)
    }

    pub fn finalize(&self) -> Result<()> {
        finalize_paths_bound(&self.backup, &self.staging, &self.plan_sha256)
    }
}

pub fn rollback_from_paths(mods_root: &Path, backup: &Path, staging: &Path) -> Result<()> {
    rollback_from_paths_preserving(mods_root, backup, staging)?;
    finalize_paths(backup, staging)
}

/// Restore the previous live Mods state while retaining the sidecar and all
/// backups. The workflow uses this form so a crash during another resource's
/// rollback can restart with the exact same verified plan.
pub(crate) fn rollback_from_paths_preserving(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
) -> Result<()> {
    verify_mods_root(mods_root)?;
    match backup.symlink_metadata() {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(user_path_err(
                "inspect_mods_backup",
                error.to_string(),
                backup,
                true,
            ));
        }
    }
    let plan = load_plan(backup)?;
    rollback_from_plan_preserving(mods_root, backup, staging, &plan)
}

/// Restore from the exact Mods plan bound into the atomic operation journal.
/// The plan is hashed and parsed from one verified file handle, then the
/// resulting in-memory value drives every mutation in this call.
pub(crate) fn rollback_from_paths_preserving_bound(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    expected_sha256: &str,
) -> Result<()> {
    verify_mods_root(mods_root)?;
    let plan = load_plan_bound(backup, expected_sha256)?;
    rollback_from_plan_preserving(mods_root, backup, staging, &plan)
}

fn rollback_from_plan_preserving(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    plan: &ModsPlan,
) -> Result<()> {
    if !artifact_file_exists(backup, APPLY_STARTED)? {
        if plan.repair {
            verify_repair_originals(mods_root, plan)?;
        } else {
            verify_managed(mods_root, &plan.previous)?;
            verify_external_originals(mods_root, &plan.replaced_external)?;
        }
        return Ok(());
    }
    verify_rollback_plan(mods_root, backup, staging, plan)?;
    if plan.repair {
        return rollback_repair_preserving(mods_root, backup, plan);
    }
    rollback_atomic_target_units(mods_root, backup, staging, plan)?;
    let previous_by_key: BTreeMap<String, &ManagedMod> = plan
        .previous
        .iter()
        .map(|managed| (key(&managed.path), managed))
        .collect();
    let mut target_created: Vec<&ManagedMod> = plan
        .target
        .iter()
        .filter(|managed| managed.disposition == ManagedModDisposition::Created)
        .filter(|managed| {
            previous_by_key
                .get(&key(&managed.path))
                .is_none_or(|previous| previous.sha256 != managed.sha256)
        })
        .filter(|managed| {
            !plan
                .atomic_units
                .iter()
                .any(|unit| atomic_unit_contains(&unit.path, &managed.path))
        })
        .collect();
    target_created.sort_by_key(|managed| std::cmp::Reverse(path_depth(&managed.path)));
    for managed in target_created {
        let path = join_relative(mods_root, &managed.path)?;
        match classify_rollback_target(mods_root, &path, managed, &plan.previous)? {
            RollbackTarget::AbsentOrPrevious => {}
            RollbackTarget::Target => {
                remove_managed_regular_if_hash(mods_root, &path, &managed.sha256)?;
                prune_empty_parents(path.parent(), mods_root)?;
            }
        }
    }

    for relative in &plan.backed_up {
        let previous = plan
            .previous
            .iter()
            .find(|managed| key(&managed.path) == key(relative))
            .ok_or_else(|| {
                internal_err(
                    "mods_backup_plan_invalid",
                    "StarVault could not recover the previous Mods files",
                    format!("backup path `{relative}` has no previous ledger row"),
                )
            })?;
        let backup_files = backup.join(BACKUP_FILES);
        let source = join_relative(&backup_files, relative)?;
        ensure_artifact_regular_file(backup, &source, "Mods backup")?;
        if hash_file(&source)? != previous.sha256 {
            return Err(package_err(
                "recovery_required",
                format!("backup of Mods file `{relative}` has changed"),
            ));
        }
        let destination = join_relative(mods_root, relative)?;
        prepare_recovery_target(
            &destination,
            mods_root,
            &plan.target,
            Some(&previous.sha256),
        )?;
        copy_atomic(
            &backup_files,
            &source,
            mods_root,
            &destination,
            backup,
            relative,
        )?;
    }
    for original in &plan.replaced_external {
        let expected_hash = original.sha256.as_deref().ok_or_else(|| {
            internal_err(
                "mods_backup_plan_invalid",
                "StarVault could not recover the external Mods files",
                format!("external file `{}` has no recorded hash", original.path),
            )
        })?;
        let backup_files = backup.join(BACKUP_FILES);
        let source = join_relative(&backup_files, &original.path)?;
        ensure_artifact_regular_file(backup, &source, "Mods backup")?;
        if hash_file(&source)? != expected_hash {
            return Err(package_err(
                "recovery_required",
                format!(
                    "backup of external Mods file `{}` has changed",
                    original.path
                ),
            ));
        }
        let destination = join_relative(mods_root, &original.path)?;
        prepare_recovery_target(&destination, mods_root, &plan.target, Some(expected_hash))?;
        copy_atomic(
            &backup_files,
            &source,
            mods_root,
            &destination,
            backup,
            &original.path,
        )?;
    }
    restore_atomic_backup_units(mods_root, backup, plan)?;
    verify_managed(mods_root, &plan.previous)?;
    verify_external_originals(mods_root, &plan.replaced_external)?;
    Ok(())
}

fn rollback_atomic_target_units(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    plan: &ModsPlan,
) -> Result<()> {
    for unit in &plan.atomic_units {
        if classify_atomic_target_rollback(mods_root, backup, staging, plan, unit)?
            == AtomicTargetRollbackState::TargetLive
        {
            rename_mods_with_retry(
                &join_relative(mods_root, &unit.path)?,
                &join_relative(staging, &unit.path)?,
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicTargetRollbackState {
    PreviousLive,
    TargetLive,
    TargetStaged,
}

fn classify_atomic_target_rollback(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    plan: &ModsPlan,
    unit: &AtomicModUnit,
) -> Result<AtomicTargetRollbackState> {
    let target_rows = rows_in_unit(&plan.target, unit);
    let replaces_previous = plan
        .backup_units
        .iter()
        .any(|candidate| key(&candidate.path) == key(&unit.path));
    let backup_present = replaces_previous
        && !entry_is_absent(&join_relative(&backup.join(BACKUP_FILES), &unit.path)?)?;
    let live_present = !entry_is_absent(&join_relative(mods_root, &unit.path)?)?;
    let staged_present = !entry_is_absent(&join_relative(staging, &unit.path)?)?;

    if replaces_previous && !backup_present {
        let previous_rows = rows_in_unit(&plan.previous, unit);
        if live_present
            && staged_present
            && atomic_unit_tree_matches(mods_root, unit, &previous_rows)?
            && atomic_unit_tree_matches(staging, unit, &target_rows)?
        {
            verify_managed(mods_root, &owned_rows(&previous_rows))?;
            return Ok(AtomicTargetRollbackState::PreviousLive);
        }
    } else if live_present && !staged_present {
        if atomic_unit_tree_matches(mods_root, unit, &target_rows)? {
            verify_managed(mods_root, &owned_rows(&target_rows))?;
            return Ok(AtomicTargetRollbackState::TargetLive);
        }
        return Err(package_err(
            "managed_file_changed",
            format!("managed Mods container `{}` changed", unit.path),
        ));
    } else if !live_present && staged_present {
        if atomic_unit_tree_matches(staging, unit, &target_rows)? {
            return Ok(AtomicTargetRollbackState::TargetStaged);
        }
        return Err(package_err(
            "unsafe_operation_artifact",
            format!("staged Mods container `{}` changed", unit.path),
        ));
    }

    Err(package_err(
        "recovery_required",
        format!(
            "Mods container `{}` has an ambiguous rollback state",
            unit.path
        ),
    ))
}

fn restore_atomic_backup_units(mods_root: &Path, backup: &Path, plan: &ModsPlan) -> Result<()> {
    let backup_files = backup.join(BACKUP_FILES);
    for unit in &plan.backup_units {
        if classify_atomic_backup_rollback(mods_root, backup, plan, unit)?
            == AtomicBackupRollbackState::BackedUp
        {
            rename_mods_with_retry(
                &join_relative(&backup_files, &unit.path)?,
                &join_relative(mods_root, &unit.path)?,
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicBackupRollbackState {
    PreviousLive,
    BackedUp,
}

fn classify_atomic_backup_rollback(
    mods_root: &Path,
    backup: &Path,
    plan: &ModsPlan,
    unit: &AtomicModUnit,
) -> Result<AtomicBackupRollbackState> {
    let backup_files = backup.join(BACKUP_FILES);
    let previous_rows = rows_in_unit(&plan.previous, unit);
    let source_present = !entry_is_absent(&join_relative(&backup_files, &unit.path)?)?;
    if source_present {
        if !atomic_unit_tree_matches(&backup_files, unit, &previous_rows)? {
            return Err(package_err(
                "recovery_required",
                format!("backup of Mods container `{}` changed", unit.path),
            ));
        }
        verify_managed(&backup_files, &owned_rows(&previous_rows))?;
        let live_present = !entry_is_absent(&join_relative(mods_root, &unit.path)?)?;
        let target_occupies_live = plan
            .atomic_units
            .iter()
            .any(|target| key(&target.path) == key(&unit.path));
        if live_present && !target_occupies_live {
            return Err(package_err(
                "managed_file_changed",
                format!("Mods restore target `{}` became occupied", unit.path),
            ));
        }
        return Ok(AtomicBackupRollbackState::BackedUp);
    }

    if atomic_unit_tree_matches(mods_root, unit, &previous_rows)? {
        verify_managed(mods_root, &owned_rows(&previous_rows))?;
        Ok(AtomicBackupRollbackState::PreviousLive)
    } else {
        Err(package_err(
            "recovery_required",
            format!("previous Mods container `{}` is missing", unit.path),
        ))
    }
}

fn verify_atomic_rollback_plan(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    plan: &ModsPlan,
) -> Result<()> {
    for unit in &plan.atomic_units {
        classify_atomic_target_rollback(mods_root, backup, staging, plan, unit)?;
    }

    for unit in &plan.backup_units {
        classify_atomic_backup_rollback(mods_root, backup, plan, unit)?;
    }
    Ok(())
}

fn rows_in_unit<'a>(managed: &'a [ManagedMod], unit: &AtomicModUnit) -> Vec<&'a ManagedMod> {
    managed
        .iter()
        .filter(|managed| atomic_unit_contains(&unit.path, &managed.path))
        .collect()
}

fn owned_rows(rows: &[&ManagedMod]) -> Vec<ManagedMod> {
    rows.iter().map(|managed| (*managed).clone()).collect()
}

/// Check that a Mods rollback can proceed without mutating either tree.
pub fn verify_rollback_from_paths(mods_root: &Path, backup: &Path, staging: &Path) -> Result<()> {
    verify_mods_root(mods_root)?;
    match backup.symlink_metadata() {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(user_path_err(
                "inspect_mods_backup",
                error.to_string(),
                backup,
                true,
            ));
        }
    }
    let plan = load_plan(backup)?;
    verify_rollback_from_plan(mods_root, backup, staging, &plan)
}

/// Verify rollback using the exact plan digest stored in the atomic operation
/// journal. The same bound loader is used again by the mutating call.
pub(crate) fn verify_rollback_from_paths_bound(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    expected_sha256: &str,
) -> Result<()> {
    verify_mods_root(mods_root)?;
    let plan = verify_bound_backup(backup, expected_sha256)?;
    verify_rollback_from_plan(mods_root, backup, staging, &plan)
}

fn verify_rollback_from_plan(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    plan: &ModsPlan,
) -> Result<()> {
    if !artifact_file_exists(backup, APPLY_STARTED)? {
        return if plan.repair {
            verify_repair_originals(mods_root, plan)
        } else {
            verify_managed(mods_root, &plan.previous)?;
            verify_external_originals(mods_root, &plan.replaced_external)
        };
    }
    verify_rollback_plan(mods_root, backup, staging, plan)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollbackTarget {
    AbsentOrPrevious,
    Target,
}

fn verify_rollback_plan(
    mods_root: &Path,
    backup: &Path,
    staging: &Path,
    plan: &ModsPlan,
) -> Result<()> {
    if plan.repair {
        for managed in plan
            .target
            .iter()
            .filter(|managed| managed.disposition == ManagedModDisposition::Created)
        {
            let path = join_relative(mods_root, &managed.path)?;
            let original = plan
                .repair_originals
                .iter()
                .find(|original| key(&original.path) == key(&managed.path));
            classify_repair_rollback_target(mods_root, &path, managed, original)?;
        }
        for original in &plan.repair_originals {
            if let Some(expected) = &original.sha256 {
                let source = join_relative(&backup.join(BACKUP_FILES), &original.path)?;
                ensure_artifact_regular_file(backup, &source, "Mods backup")?;
                if hash_file(&source)? != *expected {
                    return Err(package_err(
                        "recovery_required",
                        format!("repair backup of Mods file `{}` has changed", original.path),
                    ));
                }
            }
        }
        for managed in plan
            .previous
            .iter()
            .filter(|managed| managed.disposition == ManagedModDisposition::Borrowed)
        {
            verify_managed(mods_root, std::slice::from_ref(managed))?;
        }
        return Ok(());
    }

    verify_atomic_rollback_plan(mods_root, backup, staging, plan)?;

    let previous_by_key: BTreeMap<String, &ManagedMod> = plan
        .previous
        .iter()
        .map(|managed| (key(&managed.path), managed))
        .collect();
    for managed in plan
        .target
        .iter()
        .filter(|managed| managed.disposition == ManagedModDisposition::Created)
        .filter(|managed| {
            previous_by_key
                .get(&key(&managed.path))
                .is_none_or(|previous| previous.sha256 != managed.sha256)
        })
        .filter(|managed| {
            !plan
                .atomic_units
                .iter()
                .any(|unit| atomic_unit_contains(&unit.path, &managed.path))
        })
    {
        let path = join_relative(mods_root, &managed.path)?;
        classify_rollback_target(mods_root, &path, managed, &plan.previous)?;
    }
    for relative in &plan.backed_up {
        let previous = previous_by_key.get(&key(relative)).ok_or_else(|| {
            internal_err(
                "mods_backup_plan_invalid",
                "StarVault could not recover the previous Mods files",
                format!("backup path `{relative}` has no previous ledger row"),
            )
        })?;
        let source = join_relative(&backup.join(BACKUP_FILES), relative)?;
        ensure_artifact_regular_file(backup, &source, "Mods backup")?;
        if hash_file(&source)? != previous.sha256 {
            return Err(package_err(
                "recovery_required",
                format!("backup of Mods file `{relative}` has changed"),
            ));
        }
        verify_backed_up_destination(mods_root, relative, previous, &plan.target)?;
    }
    for original in &plan.replaced_external {
        let expected = original.sha256.as_deref().ok_or_else(|| {
            internal_err(
                "mods_backup_plan_invalid",
                "StarVault could not recover the external Mods files",
                format!("external file `{}` has no recorded hash", original.path),
            )
        })?;
        let source = join_relative(&backup.join(BACKUP_FILES), &original.path)?;
        ensure_artifact_regular_file(backup, &source, "Mods backup")?;
        if hash_file(&source)? != expected {
            return Err(package_err(
                "recovery_required",
                format!(
                    "backup of external Mods file `{}` has changed",
                    original.path
                ),
            ));
        }
    }
    let backed_up: BTreeSet<String> = plan.backed_up.iter().map(|path| key(path)).collect();
    for previous in plan
        .previous
        .iter()
        .filter(|managed| !backed_up.contains(&key(&managed.path)))
        .filter(|managed| {
            !plan
                .backup_units
                .iter()
                .any(|unit| atomic_unit_contains(&unit.path, &managed.path))
        })
    {
        verify_managed(mods_root, std::slice::from_ref(previous))?;
    }
    Ok(())
}

fn verify_external_originals(mods_root: &Path, originals: &[RepairOriginal]) -> Result<()> {
    for original in originals {
        let expected = original.sha256.as_deref().ok_or_else(|| {
            internal_err(
                "mods_backup_plan_invalid",
                "StarVault could not verify the external Mods files",
                format!("external file `{}` has no recorded hash", original.path),
            )
        })?;
        let path = join_relative(mods_root, &original.path)?;
        ensure_managed_ancestors(mods_root, &path)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            user_path_err("inspect_external_mod", error.to_string(), &path, true)
        })?;
        if !metadata.file_type().is_file() || is_link(&metadata) || hash_file(&path)? != expected {
            return Err(package_err(
                "managed_file_changed",
                format!(
                    "external Mods file `{}` changed during recovery",
                    original.path
                ),
            ));
        }
    }
    Ok(())
}

fn verify_backed_up_destination(
    mods_root: &Path,
    relative: &str,
    previous: &ManagedMod,
    target: &[ManagedMod],
) -> Result<()> {
    let relative_key = key(relative);
    let target_owns_shape = target.iter().any(|managed| {
        let target_key = key(&managed.path);
        target_key == relative_key
            || target_key.starts_with(&format!("{relative_key}/"))
            || relative_key.starts_with(&format!("{target_key}/"))
    });
    if target_owns_shape {
        // Every target-created row is classified separately above. This also
        // covers file/directory shape changes where the exact previous path
        // is temporarily a parent or child of the target path.
        return Ok(());
    }

    let destination = join_relative(mods_root, relative)?;
    ensure_managed_ancestors(mods_root, &destination)?;
    match std::fs::symlink_metadata(&destination) {
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(user_path_err(
            "inspect_managed_mod",
            error.to_string(),
            &destination,
            true,
        )),
        Ok(metadata)
            if metadata.file_type().is_file()
                && !is_link(&metadata)
                && hash_file(&destination)? == previous.sha256 =>
        {
            Ok(())
        }
        Ok(_) => Err(package_err(
            "managed_file_changed",
            format!("managed Mods path `{relative}` changed during recovery"),
        )),
    }
}

fn classify_rollback_target(
    mods_root: &Path,
    path: &Path,
    target: &ManagedMod,
    previous: &[ManagedMod],
) -> Result<RollbackTarget> {
    ensure_managed_ancestors(mods_root, path)?;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(RollbackTarget::AbsentOrPrevious);
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_managed_mod",
                error.to_string(),
                path,
                true,
            ));
        }
    };
    if metadata.file_type().is_file() && !is_link(&metadata) {
        let actual = hash_file(path)?;
        if actual == target.sha256 {
            return Ok(RollbackTarget::Target);
        }
        if previous.iter().any(|managed| {
            key(&managed.path) == key(&target.path)
                && managed.disposition == ManagedModDisposition::Created
                && managed.sha256 == actual
        }) {
            return Ok(RollbackTarget::AbsentOrPrevious);
        }
        return Err(package_err(
            "managed_file_changed",
            format!(
                "managed Mods file `{}` changed during recovery",
                target.path
            ),
        ));
    }
    if metadata.is_dir() && !is_link(&metadata) {
        let prefix = format!("{}/", key(&target.path));
        let owns_descendant = previous.iter().any(|managed| {
            managed.disposition == ManagedModDisposition::Created
                && key(&managed.path).starts_with(&prefix)
        });
        if !owns_descendant {
            return Err(package_err(
                "managed_file_changed",
                format!(
                    "managed Mods path `{}` changed during recovery",
                    target.path
                ),
            ));
        }
        for file in inventory_files(path)? {
            let relative = file
                .strip_prefix(mods_root)
                .map_err(|error| {
                    internal_err(
                        "mods_path_outside_root",
                        "StarVault could not recover the previous Mods files",
                        error.to_string(),
                    )
                })?
                .to_string_lossy()
                .replace('\\', "/");
            let Some(previous) = previous.iter().find(|managed| {
                key(&managed.path) == key(&relative)
                    && managed.disposition == ManagedModDisposition::Created
            }) else {
                return Err(package_err(
                    "managed_file_changed",
                    format!("unowned Mods file `{relative}` appeared during recovery"),
                ));
            };
            if hash_file(&file)? != previous.sha256 {
                return Err(package_err(
                    "managed_file_changed",
                    format!("managed Mods file `{relative}` changed during recovery"),
                ));
            }
        }
        return Ok(RollbackTarget::AbsentOrPrevious);
    }
    Err(package_err(
        "managed_file_changed",
        format!(
            "managed Mods path `{}` changed during recovery",
            target.path
        ),
    ))
}

fn classify_repair_rollback_target(
    mods_root: &Path,
    path: &Path,
    target: &ManagedMod,
    original: Option<&RepairOriginal>,
) -> Result<RollbackTarget> {
    ensure_managed_ancestors(mods_root, path)?;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RollbackTarget::AbsentOrPrevious);
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_managed_mod",
                error.to_string(),
                path,
                true,
            ));
        }
    };
    if metadata.file_type().is_file() && !is_link(&metadata) {
        let actual = hash_file(path)?;
        if actual == target.sha256 {
            return Ok(RollbackTarget::Target);
        }
        if original
            .and_then(|original| original.sha256.as_ref())
            .is_some_and(|expected| expected == &actual)
        {
            return Ok(RollbackTarget::AbsentOrPrevious);
        }
    }
    Err(package_err(
        "managed_file_changed",
        format!(
            "managed Mods file `{}` changed during repair recovery",
            target.path
        ),
    ))
}

fn rollback_repair_preserving(mods_root: &Path, backup: &Path, plan: &ModsPlan) -> Result<()> {
    let mut target_created: Vec<&ManagedMod> = plan
        .target
        .iter()
        .filter(|managed| managed.disposition == ManagedModDisposition::Created)
        .collect();
    target_created.sort_by_key(|managed| std::cmp::Reverse(path_depth(&managed.path)));
    for managed in target_created {
        let path = join_relative(mods_root, &managed.path)?;
        let original = plan
            .repair_originals
            .iter()
            .find(|original| key(&original.path) == key(&managed.path));
        match classify_repair_rollback_target(mods_root, &path, managed, original)? {
            RollbackTarget::AbsentOrPrevious => {}
            RollbackTarget::Target => {
                remove_managed_regular_if_hash(mods_root, &path, &managed.sha256)?;
                prune_empty_parents(path.parent(), mods_root)?;
            }
        }
    }

    for original in &plan.repair_originals {
        let Some(expected_hash) = &original.sha256 else {
            continue;
        };
        let source = join_relative(&backup.join(BACKUP_FILES), &original.path)?;
        ensure_artifact_regular_file(backup, &source, "Mods backup")?;
        if hash_file(&source)? != *expected_hash {
            return Err(package_err(
                "recovery_required",
                format!("repair backup of Mods file `{}` has changed", original.path),
            ));
        }
        let destination = join_relative(mods_root, &original.path)?;
        prepare_recovery_target(&destination, mods_root, &plan.target, Some(expected_hash))?;
        copy_atomic(
            &backup.join(BACKUP_FILES),
            &source,
            mods_root,
            &destination,
            backup,
            &original.path,
        )?;
    }
    verify_repair_originals(mods_root, plan)?;
    Ok(())
}

pub fn finalize_paths(backup: &Path, staging: &Path) -> Result<()> {
    let mut first_error = None;
    for path in [staging, backup] {
        if let Err(error) = remove_entry_if_exists(path) {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn finalize_paths_bound(backup: &Path, staging: &Path, expected_plan_sha256: &str) -> Result<()> {
    verify_committed_finalize_paths_bound(backup, staging, expected_plan_sha256)?;
    finalize_preverified_paths(backup, staging)
}

/// Remove artifacts after the workflow globally verified every cleanup tree.
/// The caller must hold the mutation lock and call the matching committed or
/// rollback finalize verifier immediately before this function.
pub(crate) fn finalize_preverified_paths(backup: &Path, staging: &Path) -> Result<()> {
    if operation_artifact_exists(staging)? {
        remove_entry(staging)?;
    }
    if operation_artifact_exists(backup)? {
        remove_entry(backup)?;
    }
    Ok(())
}

pub(crate) fn verify_committed_finalize_paths_bound(
    backup: &Path,
    staging: &Path,
    expected_plan_sha256: &str,
) -> Result<()> {
    verify_finalize_paths_bound(backup, staging, expected_plan_sha256, true)
}

pub(crate) fn verify_rollback_finalize_paths_bound(
    backup: &Path,
    staging: &Path,
    expected_plan_sha256: &str,
) -> Result<()> {
    verify_finalize_paths_bound(backup, staging, expected_plan_sha256, false)
}

fn verify_finalize_paths_bound(
    backup: &Path,
    staging: &Path,
    expected_plan_sha256: &str,
    committed: bool,
) -> Result<()> {
    let backup_exists = operation_artifact_exists(backup)?;
    let staging_exists = operation_artifact_exists(staging)?;
    if !backup_exists {
        return if staging_exists {
            Err(package_err(
                "unsafe_operation_artifact",
                "Mods staging remains after its journal-bound backup disappeared",
            ))
        } else {
            Ok(())
        };
    }
    let plan = verify_bound_backup(backup, expected_plan_sha256)?;
    if staging_exists {
        verify_staging_tree(staging, &plan.target)?;
    }
    verify_atomic_cleanup_state(backup, staging, &plan, committed)?;
    Ok(())
}

fn verify_atomic_cleanup_state(
    backup: &Path,
    staging: &Path,
    plan: &ModsPlan,
    committed: bool,
) -> Result<()> {
    let backup_files = backup.join(BACKUP_FILES);
    for unit in &plan.backup_units {
        let present = !entry_is_absent(&join_relative(&backup_files, &unit.path)?)?;
        if present != committed {
            return Err(package_err(
                "unsafe_operation_artifact",
                format!(
                    "Mods backup container `{}` is in the wrong cleanup state",
                    unit.path
                ),
            ));
        }
        if present {
            let rows = rows_in_unit(&plan.previous, unit);
            if !atomic_unit_tree_matches(&backup_files, unit, &rows)? {
                return Err(package_err(
                    "unsafe_operation_artifact",
                    format!("Mods backup container `{}` changed shape", unit.path),
                ));
            }
        }
    }
    for unit in &plan.atomic_units {
        let present = !entry_is_absent(&join_relative(staging, &unit.path)?)?;
        if present == committed {
            return Err(package_err(
                "unsafe_operation_artifact",
                format!(
                    "staged Mods container `{}` is in the wrong cleanup state",
                    unit.path
                ),
            ));
        }
        if present {
            let rows = rows_in_unit(&plan.target, unit);
            if !atomic_unit_tree_matches(staging, unit, &rows)? {
                return Err(package_err(
                    "unsafe_operation_artifact",
                    format!("staged Mods container `{}` changed shape", unit.path),
                ));
            }
        }
    }
    Ok(())
}

fn verify_bound_backup(backup: &Path, expected_plan_sha256: &str) -> Result<ModsPlan> {
    let plan = load_plan_bound(backup, expected_plan_sha256)?;
    let allowed_temporaries = present_target_temporaries(backup, &plan)?;
    let mut optional_directories = BTreeSet::new();
    let mut optional_files = BTreeMap::new();
    for unit in &plan.backup_units {
        for managed in plan
            .previous
            .iter()
            .filter(|managed| atomic_unit_contains(&unit.path, &managed.path))
        {
            insert_expected_artifact_file(
                &mut optional_directories,
                &mut optional_files,
                &format!("{BACKUP_FILES}/{}", managed.path.replace('\\', "/")),
                "",
            )?;
        }
    }
    optional_directories.remove(&key(BACKUP_FILES));
    let mut unhashed = allowed_temporaries.clone();
    unhashed.extend(optional_files.keys().cloned());
    let (actual_directories, mut actual_files) = inventory_operation_artifact(backup, &unhashed)?;
    for temporary in &allowed_temporaries {
        if actual_files.remove(temporary).is_none() {
            return Err(package_err(
                "unsafe_operation_artifact",
                "a journal-bound Mods temporary changed during cleanup verification",
            ));
        }
    }
    let mut expected_directories = BTreeSet::new();
    let mut expected_files = BTreeMap::new();
    expected_directories.insert(key(BACKUP_FILES));
    insert_expected_artifact_file(
        &mut expected_directories,
        &mut expected_files,
        PLAN_FILE,
        expected_plan_sha256,
    )?;
    for relative in &plan.backed_up {
        let previous = plan
            .previous
            .iter()
            .find(|managed| key(&managed.path) == key(relative))
            .ok_or_else(|| {
                package_err(
                    "corrupt_operation_journal",
                    format!("Mods backup path `{relative}` has no previous plan row"),
                )
            })?;
        insert_expected_artifact_file(
            &mut expected_directories,
            &mut expected_files,
            &format!("{BACKUP_FILES}/{}", relative.replace('\\', "/")),
            &previous.sha256,
        )?;
    }
    for original in &plan.repair_originals {
        if let Some(sha256) = &original.sha256 {
            insert_expected_artifact_file(
                &mut expected_directories,
                &mut expected_files,
                &format!("{BACKUP_FILES}/{}", original.path.replace('\\', "/")),
                sha256,
            )?;
        }
    }
    for original in &plan.replaced_external {
        let sha256 = original.sha256.as_deref().ok_or_else(|| {
            package_err(
                "corrupt_operation_journal",
                format!("external Mods backup `{}` has no hash", original.path),
            )
        })?;
        insert_expected_artifact_file(
            &mut expected_directories,
            &mut expected_files,
            &format!("{BACKUP_FILES}/{}", original.path.replace('\\', "/")),
            sha256,
        )?;
    }
    let started = actual_files.contains_key(&key(APPLY_STARTED));
    let complete = actual_files.contains_key(&key(APPLY_COMPLETE));
    if complete && !started {
        return Err(package_err(
            "unsafe_operation_artifact",
            "Mods backup has a completion marker without an apply marker",
        ));
    }
    if started {
        insert_expected_artifact_file(
            &mut expected_directories,
            &mut expected_files,
            APPLY_STARTED,
            &sha256_bytes(b"started"),
        )?;
    }
    if complete {
        insert_expected_artifact_file(
            &mut expected_directories,
            &mut expected_files,
            APPLY_COMPLETE,
            &sha256_bytes(b"complete"),
        )?;
    }
    verify_expected_artifact_tree_with_optional(
        backup,
        actual_directories,
        actual_files,
        expected_directories,
        expected_files,
        optional_directories,
        optional_files,
    )?;
    Ok(plan)
}

fn verify_staging_tree(staging: &Path, target: &[ManagedMod]) -> Result<()> {
    let unhashed = target
        .iter()
        .map(|managed| key(&managed.path))
        .collect::<BTreeSet<_>>();
    let (actual_directories, actual_files) = inventory_operation_artifact(staging, &unhashed)?;
    let mut expected_directories = BTreeSet::new();
    let mut expected_files = BTreeMap::new();
    for managed in target {
        insert_expected_artifact_file(
            &mut expected_directories,
            &mut expected_files,
            &managed.path,
            "",
        )?;
    }
    let directories_are_owned = actual_directories.is_subset(&expected_directories);
    let files_are_owned = actual_files
        .keys()
        .all(|path| expected_files.contains_key(path));
    if directories_are_owned && files_are_owned {
        return Ok(());
    }
    Err(user_path_err(
        "unsafe_operation_artifact",
        "Mods staging contains data not bound to the atomic operation journal",
        staging,
        false,
    ))
}

fn insert_expected_artifact_file(
    directories: &mut BTreeSet<String>,
    files: &mut BTreeMap<String, String>,
    relative: &str,
    sha256: &str,
) -> Result<()> {
    let normalized = relative.replace('\\', "/");
    let path = Path::new(&normalized);
    if normalized.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(package_err(
            "corrupt_operation_journal",
            format!("Mods cleanup evidence contains unsafe path `{relative}`"),
        ));
    }
    let file_key = key(&normalized);
    if files.insert(file_key, sha256.to_owned()).is_some() {
        return Err(package_err(
            "corrupt_operation_journal",
            format!("Mods cleanup evidence contains duplicate path `{relative}`"),
        ));
    }
    let segments = normalized.split('/').collect::<Vec<_>>();
    for end in 1..segments.len() {
        directories.insert(key(&segments[..end].join("/")));
    }
    Ok(())
}

fn inventory_operation_artifact(
    root: &Path,
    unhashed_regular_files: &BTreeSet<String>,
) -> Result<(BTreeSet<String>, BTreeMap<String, String>)> {
    ensure_owned_directory(root, "Mods operation artifact")?;
    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| {
                user_path_err("read_mods_artifact", error.to_string(), &directory, true)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                user_path_err("read_mods_artifact", error.to_string(), &directory, true)
            })?
        {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                user_path_err("inspect_mods_artifact", error.to_string(), &path, true)
            })?;
            if is_link(&metadata) {
                return Err(package_err(
                    "unsafe_operation_artifact",
                    "Mods cleanup artifact contains a link or junction",
                ));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|error| {
                    internal_err(
                        "mods_path_outside_root",
                        "StarVault could not verify a Mods cleanup artifact",
                        error.to_string(),
                    )
                })?
                .to_string_lossy()
                .replace('\\', "/");
            let relative_key = key(&relative);
            if metadata.is_dir() {
                if !directories.insert(relative_key) {
                    return Err(package_err(
                        "unsafe_operation_artifact",
                        "Mods cleanup artifact contains a case-aliased directory",
                    ));
                }
                stack.push(path);
            } else if metadata.file_type().is_file() {
                let sha256 = if unhashed_regular_files.contains(&relative_key) {
                    String::new()
                } else {
                    hash_file(&path)?
                };
                if files.insert(relative_key, sha256).is_some() {
                    return Err(package_err(
                        "unsafe_operation_artifact",
                        "Mods cleanup artifact contains a case-aliased file",
                    ));
                }
            } else {
                return Err(package_err(
                    "unsafe_operation_artifact",
                    "Mods cleanup artifact contains an unsupported object",
                ));
            }
        }
    }
    Ok((directories, files))
}

fn present_target_temporaries(backup: &Path, plan: &ModsPlan) -> Result<BTreeSet<String>> {
    ensure_owned_directory(backup, "Mods backup")?;
    let mut present = BTreeSet::new();
    for managed in plan
        .target
        .iter()
        .filter(|managed| managed.disposition == ManagedModDisposition::Created)
    {
        let temporary = temporary_copy_path(backup, &managed.path)?;
        let name = temporary
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                internal_err(
                    "mods_temporary_name_invalid",
                    "StarVault could not verify a Mods cleanup artifact",
                    temporary.display().to_string(),
                )
            })?;
        match std::fs::symlink_metadata(&temporary) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(user_path_err(
                    "inspect_mods_artifact",
                    error.to_string(),
                    &temporary,
                    true,
                ));
            }
            Ok(metadata) if metadata.file_type().is_file() && !is_link(&metadata) => {
                present.insert(key(name));
            }
            Ok(_) => {
                return Err(package_err(
                    "unsafe_operation_artifact",
                    "a journal-bound Mods temporary is linked or is not a regular file",
                ));
            }
        }
    }
    Ok(present)
}

fn verify_expected_artifact_tree_with_optional(
    root: &Path,
    actual_directories: BTreeSet<String>,
    actual_files: BTreeMap<String, String>,
    expected_directories: BTreeSet<String>,
    expected_files: BTreeMap<String, String>,
    optional_directories: BTreeSet<String>,
    optional_files: BTreeMap<String, String>,
) -> Result<()> {
    let mandatory_directories_present = expected_directories.is_subset(&actual_directories);
    let extra_directories_owned = actual_directories
        .difference(&expected_directories)
        .all(|path| optional_directories.contains(path));
    let mandatory_files_present = expected_files
        .iter()
        .all(|(path, hash)| actual_files.get(path) == Some(hash));
    let extra_files_owned = actual_files.iter().all(|(path, hash)| {
        expected_files.contains_key(path) || optional_files.get(path) == Some(hash)
    });
    if mandatory_directories_present
        && extra_directories_owned
        && mandatory_files_present
        && extra_files_owned
    {
        return Ok(());
    }
    Err(user_path_err(
        "unsafe_operation_artifact",
        "Mods cleanup artifact does not match the atomic operation journal",
        root,
        false,
    ))
}

fn operation_artifact_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_path_err(
            "inspect_mods_artifact",
            error.to_string(),
            path,
            true,
        )),
    }
}

pub fn verify_managed(mods_root: &Path, managed: &[ManagedMod]) -> Result<()> {
    verify_mods_root(mods_root)?;
    for managed in managed {
        let path = join_relative(mods_root, &managed.path)?;
        ensure_managed_ancestors(mods_root, &path)?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(changed_managed_error(
                    managed,
                    format!("managed Mods file `{}` is missing", managed.path),
                ));
            }
            Err(error) => {
                return Err(user_path_err(
                    "inspect_managed_mod",
                    error.to_string(),
                    &path,
                    true,
                ));
            }
        };
        if !metadata.file_type().is_file()
            || is_link(&metadata)
            || hash_file(&path)? != managed.sha256
        {
            return Err(changed_managed_error(
                managed,
                format!("managed Mods file `{}` has changed", managed.path),
            ));
        }
    }
    Ok(())
}

/// Validate only the journal-owned filesystem shape. Full content hashing is
/// reserved for startup health and checks performed before deleting a managed
/// file; freshly copied bytes already come from the immutable package store.
pub(crate) fn verify_managed_shape(mods_root: &Path, managed: &[ManagedMod]) -> Result<()> {
    verify_mods_root(mods_root)?;
    for managed in managed {
        let path = join_relative(mods_root, &managed.path)?;
        ensure_managed_ancestors(mods_root, &path)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            user_path_err("inspect_managed_mod", error.to_string(), &path, true)
        })?;
        if !metadata.file_type().is_file() || is_link(&metadata) {
            return Err(changed_managed_error(
                managed,
                format!("managed Mods file `{}` changed shape", managed.path),
            ));
        }
    }
    Ok(())
}

fn verify_mods_root(mods_root: &Path) -> Result<()> {
    match std::fs::symlink_metadata(mods_root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_path_err(
            "inspect_mods_root",
            error.to_string(),
            mods_root,
            true,
        )),
        Ok(metadata) if metadata.is_dir() && !is_link(&metadata) => Ok(()),
        Ok(_) => Err(package_err(
            "unsafe_mods_root",
            "the StarCraft II Mods path is linked or is not a directory",
        )),
    }
}

fn ensure_managed_ancestors(mods_root: &Path, path: &Path) -> Result<()> {
    verify_mods_root(mods_root)?;
    ensure_unlinked_ancestors(mods_root, path, "managed_file_changed", "managed Mods path")
}

fn ensure_artifact_regular_file(root: &Path, path: &Path, label: &str) -> Result<()> {
    ensure_owned_directory(root, label)?;
    ensure_unlinked_ancestors(
        root,
        path,
        "unsafe_operation_artifact",
        "operation artifact",
    )?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), path, true))?;
    if !metadata.file_type().is_file() || is_link(&metadata) {
        return Err(package_err(
            "unsafe_operation_artifact",
            format!(
                "{label} file `{}` is linked or is not regular",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn ensure_unlinked_ancestors(
    root: &Path,
    path: &Path,
    error_code: &'static str,
    label: &str,
) -> Result<()> {
    let relative = path.strip_prefix(root).map_err(|error| {
        internal_err(
            "mods_path_outside_root",
            "StarVault could not inspect a Mods path",
            error.to_string(),
        )
    })?;
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        let Component::Normal(component) = component else {
            return Err(internal_err(
                "invalid_managed_mod_path",
                "StarVault could not inspect a Mods path",
                format!("unsafe path `{}`", path.display()),
            ));
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(package_err(
                    error_code,
                    format!(
                        "{label} crosses an external link at `{}`",
                        current.display()
                    ),
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                break;
            }
            Err(error) => {
                return Err(user_path_err(
                    "inspect_mods_ancestor",
                    error.to_string(),
                    &current,
                    true,
                ));
            }
        }
    }
    Ok(())
}

fn changed_managed_error(managed: &ManagedMod, message: String) -> crate::Error {
    let code = if managed.disposition == ManagedModDisposition::Borrowed {
        "borrowed_file_changed"
    } else {
        "managed_file_changed"
    };
    package_err(code, message)
}

fn verify_repair_originals(mods_root: &Path, plan: &ModsPlan) -> Result<()> {
    for managed in plan
        .previous
        .iter()
        .filter(|managed| managed.disposition == ManagedModDisposition::Borrowed)
    {
        verify_managed(mods_root, std::slice::from_ref(managed))?;
    }
    for original in &plan.repair_originals {
        let path = join_relative(mods_root, &original.path)?;
        ensure_managed_ancestors(mods_root, &path)?;
        match (&original.sha256, std::fs::symlink_metadata(&path)) {
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            (None, Ok(_)) => {
                return Err(package_err(
                    "managed_file_changed",
                    format!("managed Mods path `{}` reappeared", original.path),
                ));
            }
            (None, Err(error)) => {
                return Err(user_path_err(
                    "inspect_managed_mod",
                    error.to_string(),
                    &path,
                    true,
                ));
            }
            (Some(expected), Ok(metadata))
                if metadata.file_type().is_file()
                    && !is_link(&metadata)
                    && hash_file(&path)? == *expected => {}
            (Some(_), Ok(_)) | (Some(_), Err(_)) => {
                return Err(package_err(
                    "managed_file_changed",
                    format!("managed Mods file `{}` changed again", original.path),
                ));
            }
        }
    }
    Ok(())
}

fn plan_target(
    mods_root: &Path,
    previous: &[ManagedMod],
    target: Option<&PackageManifest>,
    external_policy: ExternalModsPolicy,
) -> Result<(Vec<ManagedMod>, Vec<RepairOriginal>)> {
    let previous_by_key: BTreeMap<String, &ManagedMod> = previous
        .iter()
        .map(|managed| (key(&managed.path), managed))
        .collect();
    let desired: Vec<(String, String)> = target
        .into_iter()
        .flat_map(|manifest| &manifest.files)
        .filter_map(|file| {
            file.path
                .strip_prefix("mods/")
                .map(|path| (path.to_string(), file.sha256.clone()))
        })
        .collect();
    let desired_keys: BTreeSet<String> = desired.iter().map(|(path, _)| key(path)).collect();
    let mut rows = Vec::with_capacity(desired.len());
    let mut replaced_external = Vec::new();
    for (path, sha256) in desired {
        let (disposition, replaced) = match previous_by_key.get(&key(&path)) {
            Some(previous) if previous.sha256 == sha256 => (previous.disposition, None),
            Some(previous) if previous.disposition == ManagedModDisposition::Created => {
                (ManagedModDisposition::Created, None)
            }
            Some(previous) if external_policy == ExternalModsPolicy::Replace => (
                ManagedModDisposition::Created,
                Some(RepairOriginal {
                    path: path.clone(),
                    sha256: Some(previous.sha256.clone()),
                }),
            ),
            Some(_) => {
                return Err(package_err(
                    "external_mods_conflict",
                    format!("borrowed Mods file `{path}` cannot be replaced without permission"),
                ));
            }
            None => classify_unmanaged_target(
                mods_root,
                &path,
                &sha256,
                previous,
                &desired_keys,
                external_policy,
            )?,
        };
        if let Some(replaced) = replaced {
            replaced_external.push(replaced);
        }
        rows.push(ManagedMod {
            path,
            sha256,
            disposition,
        });
    }
    rows.sort_by_key(|managed| key(&managed.path));
    Ok((rows, replaced_external))
}

fn plan_atomic_units(
    mods_root: &Path,
    previous: &[ManagedMod],
    target: &[ManagedMod],
    replaced_external: &[RepairOriginal],
) -> Result<(Vec<AtomicModUnit>, Vec<AtomicModUnit>)> {
    let previous_groups = group_atomic_units(previous);
    let target_groups = group_atomic_units(target);
    let keys = previous_groups
        .keys()
        .chain(target_groups.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut backup_units = Vec::new();
    let mut target_units = Vec::new();

    for unit_key in keys {
        let previous_group = previous_groups.get(&unit_key);
        let target_group = target_groups.get(&unit_key);
        let target_candidate = target_group
            .and_then(|(path, rows)| atomic_unit_kind(path, rows).map(|kind| (path, rows, kind)))
            .filter(|(path, rows, _)| {
                rows.iter()
                    .all(|managed| managed.disposition == ManagedModDisposition::Created)
                    && !replaced_external
                        .iter()
                        .any(|original| atomic_unit_contains(path, &original.path))
            });
        let backup_candidate = if let Some((path, rows)) = previous_group {
            if rows
                .iter()
                .all(|managed| managed.disposition == ManagedModDisposition::Created)
            {
                if let Some(kind) = atomic_unit_kind(path, rows) {
                    let unit = AtomicModUnit {
                        path: path.clone(),
                        kind,
                    };
                    atomic_unit_tree_matches(mods_root, &unit, rows)?.then_some((path, rows, kind))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let can_backup =
            backup_candidate.is_some() && (target_group.is_none() || target_candidate.is_some());
        if let Some((path, _, kind)) = backup_candidate.filter(|_| can_backup) {
            backup_units.push(AtomicModUnit {
                path: path.clone(),
                kind,
            });
        }

        let live_will_be_absent = if previous_group.is_some() {
            can_backup
        } else {
            let path = target_group
                .map(|(path, _)| path)
                .expect("a grouped unit key has a target or previous group");
            entry_is_absent(&join_relative(mods_root, path)?)?
        };
        if let Some((path, _, kind)) = target_candidate.filter(|_| live_will_be_absent) {
            target_units.push(AtomicModUnit {
                path: path.clone(),
                kind,
            });
        }
    }
    Ok((backup_units, target_units))
}

fn group_atomic_units(managed: &[ManagedMod]) -> BTreeMap<String, (String, Vec<&ManagedMod>)> {
    let mut grouped = BTreeMap::new();
    for managed in managed {
        let Some(unit) = managed.path.split('/').next() else {
            continue;
        };
        if unit.is_empty() || !unit.to_ascii_lowercase().ends_with(".sc2mod") {
            continue;
        }
        grouped
            .entry(key(unit))
            .or_insert_with(|| (unit.to_owned(), Vec::new()))
            .1
            .push(managed);
    }
    grouped
}

fn atomic_unit_kind(path: &str, rows: &[&ManagedMod]) -> Option<AtomicModUnitKind> {
    if rows.len() == 1 && key(&rows[0].path) == key(path) {
        Some(AtomicModUnitKind::File)
    } else if rows
        .iter()
        .all(|managed| atomic_unit_contains(path, &managed.path) && key(&managed.path) != key(path))
    {
        Some(AtomicModUnitKind::Directory)
    } else {
        None
    }
}

fn entry_is_absent(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(true)
        }
        Err(error) => Err(user_path_err(
            "inspect_mods_target",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn atomic_unit_contains(unit: &str, path: &str) -> bool {
    let unit = key(unit);
    let path = key(path);
    path == unit || path.starts_with(&format!("{unit}/"))
}

fn move_live_unit_to_backup(
    mods_root: &Path,
    backup: &Path,
    unit: &AtomicModUnit,
    previous: &[ManagedMod],
) -> Result<()> {
    let rows = previous
        .iter()
        .filter(|managed| atomic_unit_contains(&unit.path, &managed.path))
        .collect::<Vec<_>>();
    if !atomic_unit_tree_matches(mods_root, unit, &rows)? {
        return Err(package_err(
            "managed_file_changed",
            format!("managed Mods container `{}` changed shape", unit.path),
        ));
    }
    let source = join_relative(mods_root, &unit.path)?;
    let backup_files = backup.join(BACKUP_FILES);
    ensure_owned_directory(backup, "Mods backup")?;
    ensure_owned_directory(&backup_files, "Mods backup files")?;
    let destination = join_relative(&backup_files, &unit.path)?;
    if !entry_is_absent(&destination)? {
        return Err(package_err(
            "unsafe_operation_artifact",
            format!("Mods backup container `{}` is already occupied", unit.path),
        ));
    }
    rename_mods_with_retry(&source, &destination)
}

fn move_atomic_unit(
    staging: &Path,
    mods_root: &Path,
    unit: &AtomicModUnit,
    target: &[ManagedMod],
) -> Result<()> {
    verify_atomic_staging_unit(staging, unit, target)?;
    verify_mods_root(mods_root)?;
    std::fs::create_dir_all(mods_root)
        .map_err(|error| user_path_err("create_mods_root", error.to_string(), mods_root, true))?;
    verify_mods_root(mods_root)?;

    let source = join_relative(staging, &unit.path)?;
    let destination = join_relative(mods_root, &unit.path)?;
    ensure_managed_ancestors(mods_root, &destination)?;
    match std::fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(user_path_err(
                "inspect_mods_target",
                error.to_string(),
                &destination,
                true,
            ));
        }
        Ok(_) => {
            return Err(package_err(
                "managed_file_changed",
                format!(
                    "Mods container `{}` became occupied before deployment",
                    unit.path
                ),
            ));
        }
    }

    rename_mods_with_retry(&source, &destination)
}

fn verify_atomic_staging_unit(
    staging: &Path,
    unit: &AtomicModUnit,
    target: &[ManagedMod],
) -> Result<()> {
    let rows = target
        .iter()
        .filter(|managed| atomic_unit_contains(&unit.path, &managed.path))
        .collect::<Vec<_>>();
    if atomic_unit_tree_matches(staging, unit, &rows)? {
        Ok(())
    } else {
        Err(package_err(
            "unsafe_operation_artifact",
            format!("Mods staging unit `{}` changed shape", unit.path),
        ))
    }
}

fn atomic_unit_tree_matches(
    root: &Path,
    unit: &AtomicModUnit,
    rows: &[impl std::borrow::Borrow<ManagedMod>],
) -> Result<bool> {
    let source = join_relative(root, &unit.path)?;
    ensure_unlinked_ancestors(root, &source, "unsafe_operation_artifact", "Mods container")?;
    let metadata = match std::fs::symlink_metadata(&source) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_mods_artifact",
                error.to_string(),
                &source,
                true,
            ));
        }
    };
    if is_link(&metadata) {
        return Err(package_err(
            "unsafe_operation_artifact",
            format!("Mods container `{}` is linked", unit.path),
        ));
    }

    match unit.kind {
        AtomicModUnitKind::File => Ok(metadata.file_type().is_file()
            && rows.len() == 1
            && key(&rows[0].borrow().path) == key(&unit.path)),
        AtomicModUnitKind::Directory if metadata.is_dir() => {
            let prefix = format!("{}/", unit.path);
            let relative_rows = rows
                .iter()
                .filter_map(|managed| managed.borrow().path.strip_prefix(&prefix))
                .collect::<Vec<_>>();
            if relative_rows.len() != rows.len() || relative_rows.is_empty() {
                return Ok(false);
            }
            let unhashed = relative_rows
                .iter()
                .map(|path| key(path))
                .collect::<BTreeSet<_>>();
            let (actual_directories, actual_files) =
                inventory_operation_artifact(&source, &unhashed)?;
            let mut expected_directories = BTreeSet::new();
            let mut expected_files = BTreeMap::new();
            for relative in relative_rows {
                insert_expected_artifact_file(
                    &mut expected_directories,
                    &mut expected_files,
                    relative,
                    "",
                )?;
            }
            Ok(actual_directories == expected_directories && actual_files == expected_files)
        }
        AtomicModUnitKind::Directory => Ok(false),
    }
}

fn rename_mods_with_retry(source: &Path, destination: &Path) -> Result<()> {
    for attempt in 0..RETRY_ATTEMPTS {
        match std::fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error) if retryable_mods_io(&error) && attempt + 1 < RETRY_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(
                    RETRY_BASE_MS * (attempt as u64 + 1),
                ));
            }
            Err(error) => {
                return Err(user_path_err(
                    "commit_mods_container",
                    error.to_string(),
                    destination,
                    retryable_mods_io(&error),
                ));
            }
        }
    }
    unreachable!("the Mods rename retry loop always returns")
}

fn retryable_mods_io(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    ) || matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

fn classify_unmanaged_target(
    mods_root: &Path,
    relative: &str,
    sha256: &str,
    previous: &[ManagedMod],
    desired_keys: &BTreeSet<String>,
    external_policy: ExternalModsPolicy,
) -> Result<(ManagedModDisposition, Option<RepairOriginal>)> {
    let target = join_relative(mods_root, relative)?;
    inspect_ancestors(mods_root, &target, previous)?;
    match std::fs::symlink_metadata(&target) {
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok((ManagedModDisposition::Created, None))
        }
        Err(error) => Err(user_path_err(
            "inspect_mods_target",
            error.to_string(),
            &target,
            true,
        )),
        Ok(metadata) if metadata.file_type().is_file() && !is_link(&metadata) => {
            let actual = hash_file(&target)?;
            if actual == sha256 {
                Ok((ManagedModDisposition::Borrowed, None))
            } else if external_policy == ExternalModsPolicy::Replace {
                Ok((
                    ManagedModDisposition::Created,
                    Some(RepairOriginal {
                        path: relative.to_string(),
                        sha256: Some(actual),
                    }),
                ))
            } else {
                Err(package_err(
                    "external_mods_conflict",
                    format!("an external Mods file already occupies `{relative}`"),
                ))
            }
        }
        Ok(metadata) if metadata.is_dir() && !is_link(&metadata) => {
            let files = inventory_files(&target)?;
            let all_replaceable = !files.is_empty()
                && files.iter().all(|path| {
                    let Ok(relative_path) = path.strip_prefix(mods_root) else {
                        return false;
                    };
                    let relative_path = relative_path.to_string_lossy().replace('\\', "/");
                    previous.iter().any(|managed| {
                        key(&managed.path) == key(&relative_path)
                            && managed.disposition == ManagedModDisposition::Created
                            && !desired_keys.contains(&key(&managed.path))
                    })
                });
            if all_replaceable {
                Ok((ManagedModDisposition::Created, None))
            } else {
                Err(package_err(
                    "mods_conflict",
                    format!("an external Mods directory already occupies `{relative}`"),
                ))
            }
        }
        Ok(_) => Err(package_err(
            "mods_conflict",
            format!("an unsupported filesystem entry occupies `{relative}`"),
        )),
    }
}

fn inspect_ancestors(mods_root: &Path, target: &Path, previous: &[ManagedMod]) -> Result<()> {
    let mut current = target.parent();
    while let Some(path) = current {
        if path == mods_root {
            return Ok(());
        }
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            if is_link(&metadata) {
                return Err(package_err(
                    "mods_conflict",
                    format!("Mods path `{}` crosses an external link", path.display()),
                ));
            }
            if !metadata.is_dir() {
                let relative = path
                    .strip_prefix(mods_root)
                    .map_err(|error| {
                        internal_err(
                            "mods_path_outside_root",
                            "StarVault could not inspect the Mods directory",
                            error.to_string(),
                        )
                    })?
                    .to_string_lossy()
                    .replace('\\', "/");
                let owned = previous.iter().any(|managed| {
                    key(&managed.path) == key(&relative)
                        && managed.disposition == ManagedModDisposition::Created
                });
                if !owned {
                    return Err(package_err(
                        "mods_conflict",
                        format!("an external file blocks Mods path `{relative}`"),
                    ));
                }
            }
        }
        current = path.parent();
    }
    Err(internal_err(
        "mods_path_outside_root",
        "StarVault could not inspect the Mods directory",
        target.display().to_string(),
    ))
}

fn prepare_owned_target(target: &Path, root: &Path, previous: &[ManagedMod]) -> Result<()> {
    clear_owned_ancestors(target, root, previous)?;
    if let Ok(metadata) = std::fs::symlink_metadata(target) {
        if metadata.is_dir() {
            remove_entry(target)?;
        } else if metadata.file_type().is_file() {
            return Err(package_err(
                "mods_conflict",
                format!("Mods target `{}` remained occupied", target.display()),
            ));
        } else {
            return Err(package_err(
                "mods_conflict",
                format!("Mods target `{}` is an unsupported link", target.display()),
            ));
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn prepare_recovery_target(
    target: &Path,
    root: &Path,
    target_rows: &[ManagedMod],
    previous_hash: Option<&str>,
) -> Result<()> {
    clear_owned_ancestors(target, root, target_rows)?;
    if let Ok(metadata) = std::fs::symlink_metadata(target) {
        let relative = target
            .strip_prefix(root)
            .map_err(|error| {
                internal_err(
                    "mods_path_outside_root",
                    "StarVault could not recover the previous Mods files",
                    error.to_string(),
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        let relative_key = key(&relative);
        let safe = if metadata.file_type().is_file() && !is_link(&metadata) {
            let actual = hash_file(target)?;
            previous_hash.is_some_and(|expected| expected == actual)
                || target_rows.iter().any(|managed| {
                    managed.disposition == ManagedModDisposition::Created
                        && key(&managed.path) == relative_key
                        && managed.sha256 == actual
                })
        } else if metadata.is_dir() && !is_link(&metadata) {
            let prefix = format!("{relative_key}/");
            let owns_directory = target_rows.iter().any(|managed| {
                managed.disposition == ManagedModDisposition::Created
                    && key(&managed.path).starts_with(&prefix)
            });
            owns_directory
                && inventory_files(target)?.into_iter().try_fold(
                    true,
                    |safe, file| -> Result<bool> {
                        if !safe {
                            return Ok(false);
                        }
                        let file_relative = file
                            .strip_prefix(root)
                            .map_err(|error| {
                                internal_err(
                                    "mods_path_outside_root",
                                    "StarVault could not recover the previous Mods files",
                                    error.to_string(),
                                )
                            })?
                            .to_string_lossy()
                            .replace('\\', "/");
                        let actual = hash_file(&file)?;
                        Ok(target_rows.iter().any(|managed| {
                            managed.disposition == ManagedModDisposition::Created
                                && key(&managed.path) == key(&file_relative)
                                && managed.sha256 == actual
                        }))
                    },
                )?
        } else {
            false
        };
        if !safe {
            return Err(package_err(
                "managed_file_changed",
                format!("managed Mods path `{relative}` changed during recovery"),
            ));
        }
        remove_entry(target)?;
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn clear_owned_ancestors(target: &Path, root: &Path, owned: &[ManagedMod]) -> Result<()> {
    let mut chain = Vec::new();
    let mut current = target.parent();
    while let Some(path) = current {
        if path == root {
            break;
        }
        chain.push(path.to_path_buf());
        current = path.parent();
    }
    for path in chain.into_iter().rev() {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if is_link(&metadata) {
            return Err(package_err(
                "managed_file_changed",
                format!(
                    "managed Mods path `{}` crosses an external link",
                    path.display()
                ),
            ));
        }
        if metadata.is_dir() && !is_link(&metadata) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| {
                internal_err(
                    "mods_path_outside_root",
                    "StarVault could not prepare the Mods directory",
                    error.to_string(),
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        let Some(managed) = owned
            .iter()
            .find(|managed| key(&managed.path) == key(&relative))
        else {
            return Err(package_err(
                "mods_conflict",
                format!("external entry blocks Mods path `{relative}`"),
            ));
        };
        remove_managed_regular_if_hash(root, &path, &managed.sha256)?;
        std::fs::create_dir_all(&path)?;
    }
    Ok(())
}

fn remove_managed_regular_if_hash(root: &Path, path: &Path, sha256: &str) -> Result<()> {
    ensure_managed_ancestors(root, path)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_managed_mod", error.to_string(), path, true))?;
    if !metadata.file_type().is_file() || is_link(&metadata) || hash_file(path)? != sha256 {
        return Err(package_err(
            "managed_file_changed",
            format!("managed Mods file `{}` has changed", path.display()),
        ));
    }
    ensure_managed_ancestors(root, path)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_managed_mod", error.to_string(), path, true))?;
    if !metadata.file_type().is_file() || is_link(&metadata) || hash_file(path)? != sha256 {
        return Err(package_err(
            "managed_file_changed",
            format!(
                "managed Mods file `{}` changed before removal",
                path.display()
            ),
        ));
    }
    std::fs::remove_file(path)
        .map_err(|error| user_path_err("remove_managed_mod", error.to_string(), path, true))
}

fn copy_managed_file(root: &Path, source: &Path, backup: &Path, destination: &Path) -> Result<()> {
    ensure_managed_ancestors(root, source)?;
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| user_path_err("inspect_managed_mod", error.to_string(), source, true))?;
    if !metadata.file_type().is_file() || is_link(&metadata) {
        return Err(package_err(
            "managed_file_changed",
            format!(
                "managed Mods path `{}` is not a regular file",
                source.display()
            ),
        ));
    }
    ensure_owned_directory(backup, "Mods backup")?;
    ensure_unlinked_ancestors(
        backup,
        destination,
        "unsafe_operation_artifact",
        "Mods backup path",
    )?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ensure_owned_directory(backup, "Mods backup")?;
    ensure_unlinked_ancestors(
        backup,
        destination,
        "unsafe_operation_artifact",
        "Mods backup path",
    )?;
    match std::fs::symlink_metadata(destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(user_path_err(
                "inspect_mods_artifact",
                error.to_string(),
                destination,
                true,
            ));
        }
        Ok(_) => {
            return Err(package_err(
                "unsafe_operation_artifact",
                format!(
                    "Mods backup path `{}` was unexpectedly occupied",
                    destination.display()
                ),
            ));
        }
    }
    ensure_managed_ancestors(root, source)?;
    let mut input = std::fs::File::open(source)
        .map_err(|error| user_path_err("open_managed_mod", error.to_string(), source, true))?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| {
            user_path_err("backup_managed_mod", error.to_string(), destination, true)
        })?;
    copy_contents(&mut input, &mut output, source, destination)?;
    output.sync_all().map_err(|error| {
        user_path_err(
            "sync_managed_mod_backup",
            error.to_string(),
            destination,
            true,
        )
    })
}

fn copy_atomic(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    backup: &Path,
    relative: &str,
) -> Result<()> {
    let temporary = temporary_copy_path(backup, relative)?;
    ensure_owned_directory(backup, "Mods backup")?;
    ensure_artifact_regular_file(source_root, source, "Mods staging or backup")?;
    remove_flat_temporary_if_exists(backup, &temporary)?;
    let result = (|| -> Result<()> {
        ensure_owned_directory(backup, "Mods backup")?;
        ensure_artifact_regular_file(source_root, source, "Mods staging or backup")?;
        let mut input = std::fs::File::open(source)
            .map_err(|error| user_path_err("open_staged_mod", error.to_string(), source, true))?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                user_path_err("create_managed_mod", error.to_string(), &temporary, true)
            })?;
        copy_contents(&mut input, &mut output, source, &temporary)?;
        output.sync_all().map_err(|error| {
            user_path_err("sync_managed_mod", error.to_string(), &temporary, true)
        })?;
        drop(output);
        ensure_owned_directory(backup, "Mods backup")?;
        ensure_artifact_regular_file(backup, &temporary, "Mods temporary copy")?;
        ensure_managed_ancestors(destination_root, destination)?;
        match std::fs::symlink_metadata(destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(user_path_err(
                    "inspect_managed_mod",
                    error.to_string(),
                    destination,
                    true,
                ));
            }
            Ok(_) => {
                return Err(package_err(
                    "managed_file_changed",
                    format!(
                        "managed Mods path `{}` became occupied",
                        destination.display()
                    ),
                ));
            }
        }
        std::fs::rename(&temporary, destination).map_err(|error| {
            user_path_err("commit_managed_mod", error.to_string(), destination, true)
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_flat_temporary_if_exists(backup, &temporary);
    }
    result
}

fn temporary_copy_path(backup: &Path, relative: &str) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};

    let _ = join_relative(Path::new("managed-mod"), relative)?;
    let digest = Sha256::digest(key(relative).as_bytes());
    Ok(backup.join(format!("{TEMPORARY_PREFIX}{}.partial", hex::encode(digest))))
}

fn remove_flat_temporary_if_exists(backup: &Path, temporary: &Path) -> Result<()> {
    ensure_owned_directory(backup, "Mods backup")?;
    if temporary.parent() != Some(backup) {
        return Err(internal_err(
            "mods_temporary_outside_backup",
            "StarVault could not prepare a Mods file",
            temporary.display().to_string(),
        ));
    }
    match std::fs::symlink_metadata(temporary) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_path_err(
            "inspect_mods_artifact",
            error.to_string(),
            temporary,
            true,
        )),
        Ok(metadata) if metadata.file_type().is_file() && !is_link(&metadata) => {
            ensure_owned_directory(backup, "Mods backup")?;
            let metadata = std::fs::symlink_metadata(temporary).map_err(|error| {
                user_path_err("inspect_mods_artifact", error.to_string(), temporary, true)
            })?;
            if !metadata.file_type().is_file() || is_link(&metadata) {
                return Err(package_err(
                    "unsafe_operation_artifact",
                    "the Mods temporary copy changed before cleanup",
                ));
            }
            std::fs::remove_file(temporary).map_err(|error| {
                user_path_err("remove_mods_temporary", error.to_string(), temporary, true)
            })
        }
        Ok(_) => Err(package_err(
            "unsafe_operation_artifact",
            "the Mods temporary copy is linked or is not a regular file",
        )),
    }
}

fn copy_contents(
    input: &mut std::fs::File,
    output: &mut std::fs::File,
    source: &Path,
    destination: &Path,
) -> Result<()> {
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| user_path_err("read_mods_file", error.to_string(), source, true))?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read]).map_err(|error| {
            user_path_err("write_mods_file", error.to_string(), destination, true)
        })?;
    }
    Ok(())
}

fn persist_plan(backup: &Path, plan: &ModsPlan) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(plan).map_err(|error| {
        internal_err(
            "serialize_mods_plan",
            "StarVault could not prepare the Mods deployment",
            error.to_string(),
        )
    })?;
    write_artifact(backup, PLAN_FILE, &bytes)
}

fn load_plan(backup: &Path) -> Result<ModsPlan> {
    let bytes = read_plan_bytes(backup)?;
    parse_plan(&bytes)
}

fn load_plan_bound(backup: &Path, expected_sha256: &str) -> Result<ModsPlan> {
    validate_plan_digest(expected_sha256)?;
    let bytes = read_plan_bytes(backup)?;
    let actual = {
        use sha2::{Digest, Sha256};

        hex::encode(Sha256::digest(&bytes))
    };
    if actual != expected_sha256 {
        return Err(package_err(
            "corrupt_operation_journal",
            "the Mods recovery plan does not match the pending operation",
        ));
    }
    parse_plan(&bytes)
}

fn validate_plan_digest(expected_sha256: &str) -> Result<()> {
    if expected_sha256.len() == 64
        && expected_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(package_err(
        "corrupt_operation_journal",
        "the pending operation contains an invalid Mods plan digest",
    ))
}

fn parse_plan(bytes: &[u8]) -> Result<ModsPlan> {
    serde_json::from_slice(bytes).map_err(|error| {
        package_err(
            "corrupt_operation_journal",
            format!("Mods recovery plan is unreadable: {error}"),
        )
    })
}

fn read_plan_bytes(backup: &Path) -> Result<Vec<u8>> {
    ensure_owned_directory(backup, "Mods backup")?;
    let path = backup.join(PLAN_FILE);
    ensure_unlinked_ancestors(
        backup,
        &path,
        "unsafe_operation_artifact",
        "operation artifact",
    )?;
    let expected = std::fs::symlink_metadata(&path)
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), &path, true))?;
    validate_plan_metadata(&expected, &path)?;
    let identity = capture_plan_identity(&path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .open(&path)
        .map_err(|error| user_path_err("read_mods_plan", error.to_string(), &path, false))?;
    let opened = file
        .metadata()
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), &path, true))?;
    validate_plan_metadata(&opened, &path)?;
    if !opened_plan_is_current(&path, Some(&expected), &identity, &file, &opened)? {
        return Err(package_err(
            "unsafe_operation_artifact",
            "the Mods recovery plan changed while it was being opened",
        ));
    }
    let mut bytes = Vec::with_capacity(opened.len().min(MAX_PLAN_BYTES).try_into().unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| user_path_err("read_mods_plan", error.to_string(), &path, false))?;
    if bytes.len() as u64 > MAX_PLAN_BYTES {
        return Err(package_err(
            "unsafe_operation_artifact",
            "the Mods recovery plan exceeds the 64 MiB safety limit",
        ));
    }
    ensure_owned_directory(backup, "Mods backup")?;
    if !opened_plan_is_current(&path, None, &identity, &file, &opened)? {
        return Err(package_err(
            "unsafe_operation_artifact",
            "the Mods recovery plan changed while it was being read",
        ));
    }
    Ok(bytes)
}

fn validate_plan_metadata(metadata: &Metadata, path: &Path) -> Result<()> {
    if !metadata.file_type().is_file() || is_link(metadata) {
        return Err(package_err(
            "unsafe_operation_artifact",
            format!(
                "Mods backup file `{}` is linked or is not regular",
                path.display()
            ),
        ));
    }
    if metadata.len() > MAX_PLAN_BYTES {
        return Err(package_err(
            "unsafe_operation_artifact",
            "the Mods recovery plan exceeds the 64 MiB safety limit",
        ));
    }
    Ok(())
}

fn write_artifact(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    ensure_owned_directory(root, "Mods backup")?;
    let path = root.join(name);
    ensure_unlinked_ancestors(
        root,
        &path,
        "unsafe_operation_artifact",
        "operation artifact",
    )?;
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(user_path_err(
                "inspect_mods_artifact",
                error.to_string(),
                &path,
                true,
            ));
        }
        Ok(metadata) if metadata.file_type().is_file() && !is_link(&metadata) => {}
        Ok(_) => {
            return Err(package_err(
                "unsafe_operation_artifact",
                format!("Mods operation artifact `{name}` is linked or is not regular"),
            ));
        }
    }
    ensure_owned_directory(root, "Mods backup")?;
    crate::atomic_file::write(&path, bytes)?;
    ensure_artifact_regular_file(root, &path, "Mods backup")
}

fn artifact_file_exists(root: &Path, name: &str) -> Result<bool> {
    ensure_owned_directory(root, "Mods backup")?;
    let path = root.join(name);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_path_err(
            "inspect_mods_artifact",
            error.to_string(),
            &path,
            true,
        )),
        Ok(metadata) if metadata.file_type().is_file() && !is_link(&metadata) => Ok(true),
        Ok(_) => Err(package_err(
            "unsafe_operation_artifact",
            format!("Mods operation artifact `{name}` is linked or is not regular"),
        )),
    }
}

fn ensure_owned_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), path, true))?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(package_err(
            "unsafe_operation_artifact",
            format!("{label} is not an owned directory"),
        ));
    }
    Ok(())
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

#[cfg(unix)]
fn opened_plan_is_current(
    path: &Path,
    initial: Option<&Metadata>,
    _identity: &PlanOpenIdentity,
    _file: &File,
    opened: &Metadata,
) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let current = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), path, true))?;
    let same =
        |left: &Metadata, right: &Metadata| left.dev() == right.dev() && left.ino() == right.ino();
    Ok(same(&current, opened) && initial.is_none_or(|initial| same(initial, opened)))
}

#[cfg(windows)]
fn opened_plan_is_current(
    path: &Path,
    _initial: Option<&Metadata>,
    identity: &PlanOpenIdentity,
    file: &File,
    _opened: &Metadata,
) -> Result<bool> {
    let current_metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), path, true))?;
    validate_plan_metadata(&current_metadata, path)?;
    let current = open_plan_identity_file(path)?;
    validate_plan_identity_handle(path, &current)?;
    let current_metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), path, true))?;
    validate_plan_metadata(&current_metadata, path)?;
    Ok(
        windows_file_identity(identity, path)? == windows_file_identity(file, path)?
            && windows_file_identity(file, path)? == windows_file_identity(&current, path)?,
    )
}

#[cfg(not(any(unix, windows)))]
fn opened_plan_is_current(
    _path: &Path,
    _initial: Option<&Metadata>,
    _identity: &PlanOpenIdentity,
    _file: &File,
    _opened: &Metadata,
) -> Result<bool> {
    Ok(false)
}

#[cfg(windows)]
fn capture_plan_identity(path: &Path) -> Result<PlanOpenIdentity> {
    let identity = open_plan_identity_file(path)?;
    validate_plan_identity_handle(path, &identity)?;
    let current = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), path, true))?;
    validate_plan_metadata(&current, path)?;
    Ok(identity)
}

#[cfg(not(windows))]
fn capture_plan_identity(_path: &Path) -> Result<PlanOpenIdentity> {
    Ok(PlanOpenIdentity)
}

#[cfg(windows)]
fn open_plan_identity_file(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| user_path_err("read_mods_plan", error.to_string(), path, false))
}

#[cfg(windows)]
fn validate_plan_identity_handle(path: &Path, file: &File) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| user_path_err("inspect_mods_artifact", error.to_string(), path, true))?;
    validate_plan_metadata(&metadata, path)
}

#[cfg(windows)]
fn windows_file_identity(file: &File, path: &Path) -> Result<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct WindowsFileTime {
        low_date_time: u32,
        high_date_time: u32,
    }

    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct WindowsFileInformation {
        file_attributes: u32,
        creation_time: WindowsFileTime,
        last_access_time: WindowsFileTime,
        last_write_time: WindowsFileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "GetFileInformationByHandle"]
        fn get_file_information_by_handle(
            file: *mut std::ffi::c_void,
            information: *mut WindowsFileInformation,
        ) -> i32;
    }

    let mut information = WindowsFileInformation::default();
    // SAFETY: `file` owns a valid handle and `information` points to a fully
    // allocated structure with the layout required by Win32.
    let result = unsafe {
        get_file_information_by_handle(file.as_raw_handle(), std::ptr::addr_of_mut!(information))
    };
    if result == 0 {
        return Err(user_path_err(
            "inspect_mods_artifact",
            std::io::Error::last_os_error().to_string(),
            path,
            true,
        ));
    }
    Ok((
        information.volume_serial_number,
        (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low),
    ))
}

fn join_relative(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(internal_err(
            "invalid_managed_mod_path",
            "StarVault could not prepare the Mods deployment",
            format!("unsafe managed Mods path `{relative}`"),
        ));
    }
    Ok(root.join(path))
}

fn sibling_path(path: &Path, kind: &str, operation_id: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{kind}-{operation_id}"))
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.is_empty()
        || operation_id.len() > 96
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(internal_err(
            "invalid_operation_id",
            "StarVault could not prepare the Mods deployment",
            "operation id is not a safe path component",
        ));
    }
    Ok(())
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

fn inventory_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&directory).map_err(|error| {
            user_path_err("inspect_mods_entry", error.to_string(), &directory, true)
        })?;
        if !metadata.is_dir() || is_link(&metadata) {
            return Err(package_err(
                "mods_conflict",
                format!(
                    "external link or non-directory found at `{}`",
                    directory.display()
                ),
            ));
        }
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            user_path_err("read_mods_directory", error.to_string(), &directory, true)
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                user_path_err("read_mods_directory", error.to_string(), &directory, true)
            })?;
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
                user_path_err("inspect_mods_entry", error.to_string(), entry.path(), true)
            })?;
            if is_link(&metadata) {
                return Err(package_err(
                    "mods_conflict",
                    format!("external link found at `{}`", entry.path().display()),
                ));
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.file_type().is_file() {
                files.push(entry.path());
            } else {
                return Err(package_err(
                    "mods_conflict",
                    format!("unsupported entry found at `{}`", entry.path().display()),
                ));
            }
        }
    }
    Ok(files)
}

fn prune_empty_parents(mut current: Option<&Path>, root: &Path) -> Result<()> {
    while let Some(directory) = current {
        if directory == root {
            break;
        }
        ensure_managed_ancestors(root, directory)?;
        match std::fs::symlink_metadata(directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = directory.parent();
                continue;
            }
            Err(error) => {
                return Err(user_path_err(
                    "inspect_mods_directory",
                    error.to_string(),
                    directory,
                    true,
                ));
            }
            Ok(metadata) if metadata.is_dir() && !is_link(&metadata) => {}
            Ok(_) => {
                return Err(package_err(
                    "managed_file_changed",
                    format!(
                        "managed Mods directory `{}` is linked or changed",
                        directory.display()
                    ),
                ));
            }
        }
        match std::fs::remove_dir(directory) {
            Ok(()) => current = directory.parent(),
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = directory.parent();
            }
            Err(error) => {
                return Err(user_path_err(
                    "prune_mods_directory",
                    error.to_string(),
                    directory,
                    true,
                ));
            }
        }
    }
    Ok(())
}

fn remove_entry_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => remove_entry(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_path_err(
            "inspect_mods_artifact",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn remove_entry(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_mods_entry", error.to_string(), path, true))?;
    if is_link(&metadata) {
        return Err(package_err(
            "mods_conflict",
            format!("refusing to remove link `{}`", path.display()),
        ));
    }
    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .map_err(|error| user_path_err("remove_mods_entry", error.to_string(), path, true))
}

fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path)
        .map_err(|error| user_path_err("open_mods_file", error.to_string(), path, true))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| user_path_err("read_mods_file", error.to_string(), path, true))?;
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

fn key(path: &str) -> String {
    path.replace('\\', "/").to_ascii_lowercase()
}

fn path_depth(path: &str) -> usize {
    path.split(['/', '\\']).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_relative_rejects_escape() {
        assert!(join_relative(Path::new("mods"), "../outside").is_err());
        assert!(join_relative(Path::new("mods"), "safe/file").is_ok());
    }

    #[test]
    fn oversized_recovery_plan_is_rejected_before_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let backup = directory.path().join("Mods.backup-op");
        std::fs::create_dir(&backup).unwrap();
        let plan = std::fs::File::create(backup.join(PLAN_FILE)).unwrap();
        plan.set_len(MAX_PLAN_BYTES + 1).unwrap();

        let error = load_plan(&backup).unwrap_err();
        assert_eq!(error.code(), "unsafe_operation_artifact");
    }
}
