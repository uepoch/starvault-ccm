//! Reversible deployment of the single loose campaign override root.
//!
//! StarCraft II resolves custom campaign maps below `Maps/Campaign`. Official
//! campaign content lives outside this loose override tree, so StarVault swaps
//! the whole directory as one object for every faction. While active, the live
//! path points at an immutable per-revision deployment and the previous loose
//! tree is preserved beside it as `Campaign.starvault-plain`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::config::StrategyChoice;
use crate::error::{internal_err, package_err, user_path_err, Result};
use crate::filesystem::{
    is_link_or_reparse as is_link, is_safe_operation_id, operation_sibling as sibling_path,
};
use crate::layout::{SlotId, WindowsLayout, SLOT_OWNED_SIBLINGS};
use crate::operation::{SlotOperationJournal, SlotOperationPaths, SlotStateBinding, SlotStateKind};
use crate::store::{PackageManifest, Store};

const RETRY_ATTEMPTS: usize = 8;
const RETRY_BASE_MS: u64 = 25;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TreeEntry {
    Directory,
    File { size: u64, modified_nanos: u128 },
    Link { target: PathBuf },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StateReceipt {
    Absent,
    Directory {
        entries: BTreeMap<String, TreeEntry>,
    },
    Junction {
        target: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct PreparedSlotTransition {
    change: SlotChange,
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

    pub fn prepare(
        &self,
        previous: Option<&PackageManifest>,
        target: Option<&PackageManifest>,
        operation_id: &str,
    ) -> Result<PreparedSlotTransition> {
        validate_operation_id(operation_id)?;
        if target.is_none() {
            self.verify_owned_for_restore(previous.ok_or_else(|| {
                package_err("slot_drift", "restore has no active campaign deployment")
            })?)?;
        } else {
            self.verify_current(previous)?;
        }

        let live = self.layout.campaign_dir();
        let plain = self.layout.plain_campaign_dir();
        if previous.is_none() && !path_exists(&live)? {
            std::fs::create_dir_all(&live).map_err(|error| {
                user_path_err("create_campaign_root", error.to_string(), &live, true)
            })?;
        }
        let faction = operation_faction(previous, target);
        let staging = sibling_path(&live, "staging", operation_id);
        let backup = sibling_path(&live, "backup", operation_id);
        ensure_absent(&staging)?;
        ensure_absent(&backup)?;

        let previous_state = capture_state(faction, &live)?;
        let target_state = if let Some(manifest) = target {
            self.stage_target(manifest, &staging)?;
            capture_state(faction, &staging)?
        } else {
            ensure_real_directory(&plain, "preserved plain campaign directory")?;
            capture_state(faction, &plain)?
        };

        Ok(PreparedSlotTransition {
            change: SlotChange {
                paths: SlotOperationPaths {
                    faction,
                    live,
                    staging,
                    backup,
                },
                previous: previous.cloned(),
                expected: target.cloned(),
                previous_state,
                target_state,
            },
        })
    }

    fn stage_target(&self, manifest: &PackageManifest, staging: &Path) -> Result<()> {
        if self.strategy_override == Some(StrategyChoice::Copy) {
            self.store.materialize_campaign(manifest, staging)?;
            return verify_campaign_tree(staging, manifest);
        }

        let deployed = self.ensure_deployment(manifest)?;
        match make_junction(staging, &deployed) {
            Ok(()) => Ok(()),
            Err(error) if self.strategy_override.is_none() => {
                tracing::info!(
                    error = %error,
                    "campaign-root junction unavailable; using copy strategy"
                );
                remove_entry_if_exists(staging)?;
                self.store.materialize_campaign(manifest, staging)?;
                verify_campaign_tree(staging, manifest)
            }
            Err(error) => Err(user_path_err(
                "junction_creation_failed",
                error.to_string(),
                staging,
                true,
            )),
        }
    }

    fn ensure_deployment(&self, manifest: &PackageManifest) -> Result<PathBuf> {
        let deployed = self
            .store
            .deploy_dir(manifest.faction, &manifest.revision)?;
        if path_exists(&deployed)? {
            ensure_real_directory(&deployed, "campaign deployment")?;
            if !self.store.deployment_was_verified(&deployed) {
                match verify_campaign_tree(&deployed, manifest) {
                    Ok(()) => self.store.mark_deployment_verified(&deployed),
                    Err(error) => {
                        if live_link_targets(&self.layout.campaign_dir(), &deployed)? {
                            return Err(error);
                        }
                        remove_entry(&deployed)?;
                        self.store.forget_deployment(&deployed);
                    }
                }
            }
        }
        if !path_exists(&deployed)? {
            self.store.materialize_campaign(manifest, &deployed)?;
            verify_campaign_tree(&deployed, manifest)?;
            self.store.mark_deployment_verified(&deployed);
        }
        Ok(deployed)
    }

    pub fn verify_current(&self, manifest: Option<&PackageManifest>) -> Result<()> {
        let live = self.layout.campaign_dir();
        let plain = self.layout.plain_campaign_dir();
        match manifest {
            None => {
                ensure_absent(&plain)?;
                match std::fs::symlink_metadata(&live) {
                    Ok(metadata) if !is_link(&metadata) && metadata.is_dir() => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Ok(_) => Err(package_err(
                        "unowned_campaign_slot_link",
                        "Maps/Campaign is not a real directory while StarVault is in vanilla state",
                    )),
                    Err(error) => Err(user_path_err(
                        "inspect_campaign_root",
                        error.to_string(),
                        &live,
                        true,
                    )),
                }
            }
            Some(manifest) => {
                ensure_real_directory(&plain, "preserved plain campaign directory")?;
                let metadata = std::fs::symlink_metadata(&live).map_err(|error| {
                    user_path_err("inspect_campaign_root", error.to_string(), &live, true)
                })?;
                if is_link(&metadata) {
                    let deployed = self
                        .store
                        .deploy_dir(manifest.faction, &manifest.revision)?;
                    verify_link_target(&live, &deployed)?;
                    match std::fs::symlink_metadata(&deployed) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return Err(package_err(
                                "slot_drift",
                                "the owned campaign deployment is missing",
                            ));
                        }
                        Err(error) => {
                            return Err(user_path_err(
                                "inspect_campaign_root",
                                error.to_string(),
                                &deployed,
                                true,
                            ));
                        }
                        Ok(_) => ensure_real_directory(&deployed, "campaign deployment")?,
                    }
                    if !self.store.deployment_was_verified(&deployed) {
                        verify_campaign_tree(&deployed, manifest)?;
                        self.store.mark_deployment_verified(&deployed);
                    }
                    Ok(())
                } else if metadata.is_dir() {
                    // Explicit copy mode uses a real directory; the normal
                    // path is one root junction.
                    verify_campaign_tree(&live, manifest)
                } else {
                    Err(package_err(
                        "slot_drift",
                        "Maps/Campaign is neither an owned junction nor a campaign directory",
                    ))
                }
            }
        }
    }

    fn verify_owned_for_restore(&self, manifest: &PackageManifest) -> Result<()> {
        let live = self.layout.campaign_dir();
        ensure_real_directory(
            &self.layout.plain_campaign_dir(),
            "preserved plain campaign directory",
        )?;
        let metadata = std::fs::symlink_metadata(&live).map_err(|error| {
            user_path_err("inspect_campaign_root", error.to_string(), &live, true)
        })?;
        if is_link(&metadata) {
            verify_link_target(
                &live,
                &self
                    .store
                    .deploy_dir(manifest.faction, &manifest.revision)?,
            )
        } else if metadata.is_dir() {
            if inventory_tree(&live, true)?
                .values()
                .any(|entry| matches!(entry, TreeEntry::Link { .. }))
            {
                Err(package_err(
                    "slot_drift",
                    "Maps/Campaign contains an external link",
                ))
            } else {
                Ok(())
            }
        } else {
            Err(package_err(
                "slot_drift",
                "Maps/Campaign is neither an owned junction nor a campaign directory",
            ))
        }
    }

    pub fn verify_target(&self, transition: &PreparedSlotTransition) -> Result<()> {
        verify_state_at(
            &transition.change.paths.live,
            &transition.change.target_state,
        )?;
        self.verify_current(transition.change.expected.as_ref())
    }
}

impl PreparedSlotTransition {
    pub fn journal_paths(&self) -> SlotOperationJournal {
        SlotOperationJournal::new(
            self.change.paths.clone(),
            Some(self.change.previous_state.clone()),
            Some(self.change.target_state.clone()),
        )
    }

    pub fn apply(&self) -> Result<()> {
        self.apply_with_local_rollback(true)
    }

    pub(crate) fn apply_journaled(&self) -> Result<()> {
        self.apply_with_local_rollback(false)
    }

    fn apply_with_local_rollback(&self, rollback_on_error: bool) -> Result<()> {
        let result = apply_change(&self.change);
        if result.is_err() && rollback_on_error {
            rollback_paths_checked(
                &self.journal_paths(),
                self.change.previous.as_ref(),
                self.change.expected.as_ref(),
            )?;
        }
        result
    }

    pub fn rollback(&self) -> Result<()> {
        rollback_paths_checked(
            &self.journal_paths(),
            self.change.previous.as_ref(),
            self.change.expected.as_ref(),
        )
    }

    pub fn finalize(&self) -> Result<()> {
        commit_paths(
            &self.journal_paths(),
            self.change.previous.is_some(),
            self.change.expected.is_some(),
        )
    }
}

fn apply_change(change: &SlotChange) -> Result<()> {
    let paths = &change.paths;
    let plain = plain_path(&paths.live);
    verify_state_at(&paths.live, &change.previous_state)?;
    if change.previous.is_some() {
        ensure_real_directory(&plain, "preserved plain campaign directory")?;
    } else {
        ensure_absent(&plain)?;
    }
    if change.expected.is_some() {
        verify_state_at(&paths.staging, &change.target_state)?;
    } else {
        verify_state_at(&plain, &change.target_state)?;
    }

    rename_with_retry(&paths.live, &paths.backup)?;
    let applied = if change.expected.is_some() {
        rename_with_retry(&paths.staging, &paths.live)
    } else {
        rename_with_retry(&plain, &paths.live)
    };
    applied?;
    verify_state_at(&paths.live, &change.target_state)
}

/// Complete the campaign-root part of a ledger-committed operation. This is
/// idempotent so startup recovery can resume after any individual rename or
/// cleanup step.
pub(crate) fn commit_paths(
    journal: &SlotOperationJournal,
    previous_present: bool,
    target_present: bool,
) -> Result<()> {
    validate_journal_bindings(journal, true)?;
    let paths = &journal.paths;
    let previous = previous_binding_for(journal, paths)?;
    let target = target_binding_for(journal, paths)?;
    let plain = plain_path(&paths.live);
    verify_state_at(&paths.live, target)?;

    if target_present {
        if previous_present {
            ensure_real_directory(&plain, "preserved plain campaign directory")?;
            remove_bound_if_exists(&paths.backup, previous)?;
        } else if path_exists(&paths.backup)? {
            verify_state_at(&paths.backup, previous)?;
            ensure_absent(&plain)?;
            rename_with_retry(&paths.backup, &plain)?;
        } else {
            verify_state_at(&plain, previous)?;
        }
    } else {
        if !previous_present {
            return Err(package_err(
                "unsafe_operation_journal",
                "restore operation has no previous campaign",
            ));
        }
        ensure_absent(&plain)?;
        remove_bound_if_exists(&paths.backup, previous)?;
    }
    remove_bound_if_exists(&paths.staging, target)
}

pub fn verify_rollback_paths_checked(
    journal: &SlotOperationJournal,
    previous: Option<&PackageManifest>,
    target: Option<&PackageManifest>,
) -> Result<()> {
    validate_journal_bindings(journal, true)?;
    let paths = &journal.paths;
    let previous_state = previous_binding_for(journal, paths)?;
    let target_state = target_binding_for(journal, paths)?;
    let plain = plain_path(&paths.live);
    let backup_exists = path_exists(&paths.backup)?;

    if backup_exists {
        verify_state_at(&paths.backup, previous_state)?;
        if target.is_some() {
            verify_optional_state(&paths.live, target_state)?;
            verify_optional_state(&paths.staging, target_state)?;
        } else if path_exists(&paths.live)? {
            verify_state_at(&paths.live, target_state)?;
            ensure_absent(&plain)?;
        } else {
            verify_state_at(&plain, target_state)?;
        }
    } else {
        verify_state_at(&paths.live, previous_state)?;
        verify_optional_state(&paths.staging, target_state)?;
        if previous.is_some() {
            ensure_real_directory(&plain, "preserved plain campaign directory")?;
        } else {
            ensure_absent(&plain)?;
        }
    }
    Ok(())
}

pub fn rollback_paths_checked(
    journal: &SlotOperationJournal,
    previous: Option<&PackageManifest>,
    target: Option<&PackageManifest>,
) -> Result<()> {
    verify_rollback_paths_checked(journal, previous, target)?;
    let paths = &journal.paths;
    let previous_state = previous_binding_for(journal, paths)?;
    let target_state = target_binding_for(journal, paths)?;
    let plain = plain_path(&paths.live);

    if path_exists(&paths.backup)? {
        if target.is_some() {
            remove_bound_if_exists(&paths.live, target_state)?;
            remove_bound_if_exists(&paths.staging, target_state)?;
        } else if path_exists(&paths.live)? {
            verify_state_at(&paths.live, target_state)?;
            ensure_absent(&plain)?;
            rename_with_retry(&paths.live, &plain)?;
        } else {
            verify_state_at(&plain, target_state)?;
        }
        verify_state_at(&paths.backup, previous_state)?;
        rename_with_retry(&paths.backup, &paths.live)?;
    } else {
        remove_bound_if_exists(&paths.staging, target_state)?;
    }
    verify_state_at(&paths.live, previous_state)
}

/// Preparation cleanup. A preparing journal has no state bindings and slot
/// apply has not started, so only deterministic staging paths may exist.
pub fn finalize_paths(journal: &SlotOperationJournal) -> Result<()> {
    let mut first_error = None;
    for artifact in [&journal.paths.staging, &journal.paths.backup] {
        if let Err(error) = remove_entry_if_exists(artifact) {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(crate) fn verify_finalize_bound_paths(journal: &SlotOperationJournal) -> Result<()> {
    validate_journal_bindings(journal, true)?;
    let paths = &journal.paths;
    if path_exists(&paths.staging)? {
        verify_state_at(&paths.staging, target_binding_for(journal, paths)?)?;
    }
    if path_exists(&paths.backup)? {
        verify_state_at(&paths.backup, previous_binding_for(journal, paths)?)?;
    }
    Ok(())
}

pub(crate) fn finalize_preverified_paths(journal: &SlotOperationJournal) -> Result<()> {
    let paths = &journal.paths;
    remove_entry_if_exists(&paths.staging)?;
    remove_entry_if_exists(&paths.backup)
}

pub(crate) fn validate_journal_bindings(
    journal: &SlotOperationJournal,
    require_complete: bool,
) -> Result<()> {
    if !require_complete {
        return if journal.previous_state().is_none() && journal.target_state().is_none() {
            Ok(())
        } else {
            Err(package_err(
                "unsafe_operation_journal",
                "preparing operation unexpectedly contains campaign-root state identities",
            ))
        };
    }
    if journal.previous_state().is_none() || journal.target_state().is_none() {
        return Err(package_err(
            "unsafe_operation_journal",
            "operation journal has incomplete campaign-root state identities",
        ));
    }
    let paths = &journal.paths;
    for binding in [
        previous_binding_for(journal, paths)?,
        target_binding_for(journal, paths)?,
    ] {
        if binding.kind == SlotStateKind::SharedDirectory {
            return Err(package_err(
                "unsafe_operation_journal",
                "legacy shared-slot state is unsupported",
            ));
        }
        validate_digest(&binding.sha256)?;
    }
    Ok(())
}

fn previous_binding_for<'a>(
    journal: &'a SlotOperationJournal,
    paths: &SlotOperationPaths,
) -> Result<&'a SlotStateBinding> {
    binding_for(journal.previous_state(), paths, "previous")
}

fn target_binding_for<'a>(
    journal: &'a SlotOperationJournal,
    paths: &SlotOperationPaths,
) -> Result<&'a SlotStateBinding> {
    binding_for(journal.target_state(), paths, "target")
}

fn binding_for<'a>(
    binding: Option<&'a SlotStateBinding>,
    paths: &SlotOperationPaths,
    label: &str,
) -> Result<&'a SlotStateBinding> {
    let binding = binding.ok_or_else(|| {
        package_err(
            "unsafe_operation_journal",
            format!("operation journal is missing the {label} campaign-root state"),
        )
    })?;
    if binding.faction != paths.faction {
        return Err(package_err(
            "unsafe_operation_journal",
            format!("operation journal has an invalid {label} campaign-root state"),
        ));
    }
    Ok(binding)
}

fn operation_faction(
    previous: Option<&PackageManifest>,
    target: Option<&PackageManifest>,
) -> SlotId {
    target
        .map(|manifest| manifest.faction)
        .or_else(|| previous.map(|manifest| manifest.faction))
        .unwrap_or(SlotId::Wol)
}

fn capture_state(faction: SlotId, path: &Path) -> Result<SlotStateBinding> {
    let receipt = state_receipt(path)?;
    let kind = match receipt {
        StateReceipt::Absent => SlotStateKind::Absent,
        StateReceipt::Directory { .. } => SlotStateKind::Directory,
        StateReceipt::Junction { .. } => SlotStateKind::Junction,
    };
    Ok(SlotStateBinding {
        faction,
        kind,
        sha256: receipt_digest(&receipt)?,
    })
}

fn verify_state_at(path: &Path, binding: &SlotStateBinding) -> Result<()> {
    let receipt = state_receipt(path)?;
    let kind = match receipt {
        StateReceipt::Absent => SlotStateKind::Absent,
        StateReceipt::Directory { .. } => SlotStateKind::Directory,
        StateReceipt::Junction { .. } => SlotStateKind::Junction,
    };
    let digest = receipt_digest(&receipt)?;
    if kind != binding.kind || digest != binding.sha256 {
        return Err(user_path_err(
            "slot_drift",
            "campaign-root object no longer matches the operation journal",
            path,
            false,
        ));
    }
    Ok(())
}

fn verify_optional_state(path: &Path, binding: &SlotStateBinding) -> Result<()> {
    if path_exists(path)? {
        verify_state_at(path, binding)
    } else {
        Ok(())
    }
}

fn state_receipt(path: &Path) -> Result<StateReceipt> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StateReceipt::Absent);
        }
        Err(error) => {
            return Err(user_path_err(
                "inspect_campaign_root",
                error.to_string(),
                path,
                true,
            ));
        }
    };
    if is_link(&metadata) {
        return Ok(StateReceipt::Junction {
            target: canonical_link_target(path)?,
        });
    }
    if metadata.is_dir() {
        return Ok(StateReceipt::Directory {
            entries: inventory_tree(path, true)?,
        });
    }
    Err(user_path_err(
        "unsafe_campaign_root",
        "campaign root must be a directory or directory junction",
        path,
        false,
    ))
}

fn receipt_digest(receipt: &StateReceipt) -> Result<String> {
    let bytes = serde_json::to_vec(receipt).map_err(|error| {
        internal_err(
            "serialize_campaign_state",
            "StarVault could not record the campaign-root state",
            error.to_string(),
        )
    })?;
    Ok(sha256_bytes(&bytes))
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(package_err(
            "unsafe_operation_journal",
            "campaign-root state contains an invalid digest",
        ))
    }
}

fn verify_campaign_tree(root: &Path, manifest: &PackageManifest) -> Result<()> {
    ensure_real_directory(root, "campaign deployment")?;
    let inventory = match inventory_tree(root, false) {
        Err(error) if error.code() == "unsafe_campaign_tree" => {
            return Err(package_err(
                "slot_drift",
                "campaign deployment contains an unexpected filesystem object",
            ));
        }
        result => result?,
    };
    let mut actual = BTreeMap::new();
    for (path, entry) in inventory {
        match entry {
            TreeEntry::File { size, .. } => {
                actual.insert(path, size);
            }
            TreeEntry::Link { .. } => {
                return Err(package_err(
                    "slot_drift",
                    "campaign deployment contains an unexpected link",
                ));
            }
            TreeEntry::Directory => {}
        }
    }

    let mut expected = BTreeMap::new();
    let prefix = campaign_prefix(manifest.faction);
    for file in &manifest.files {
        let Some(relative) = file.path.strip_prefix("slot/") else {
            continue;
        };
        if manifest.faction == SlotId::Wol {
            let first = relative.split('/').next().unwrap_or_default();
            if SLOT_OWNED_SIBLINGS
                .iter()
                .any(|reserved| first.eq_ignore_ascii_case(reserved))
            {
                return Err(package_err(
                    "reserved_campaign_path",
                    format!("Wings of Liberty package uses reserved path `{relative}`"),
                ));
            }
        }
        let path = if prefix.is_empty() {
            relative.to_string()
        } else {
            format!("{prefix}/{relative}")
        };
        expected.insert(path, file.size);
    }
    if actual == expected {
        Ok(())
    } else {
        Err(package_err(
            "slot_drift",
            "campaign deployment does not match the package manifest",
        ))
    }
}

fn campaign_prefix(faction: SlotId) -> &'static str {
    match faction {
        SlotId::Wol => "",
        SlotId::HotS => "swarm",
        SlotId::LotV => "void",
        SlotId::Nco => "nova",
    }
}

fn inventory_tree(root: &Path, include_links: bool) -> Result<BTreeMap<String, TreeEntry>> {
    let mut inventory = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = read_dir(&directory)?;
        for entry in entries {
            let path = entry.path();
            let relative = canonical_relative(root, &path)?;
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                user_path_err("inspect_campaign_tree", error.to_string(), &path, true)
            })?;
            if is_link(&metadata) {
                if !include_links {
                    return Err(user_path_err(
                        "unsafe_campaign_tree",
                        "campaign deployment contains a link or reparse point",
                        &path,
                        false,
                    ));
                }
                inventory.insert(
                    relative,
                    TreeEntry::Link {
                        target: canonical_link_target(&path)?,
                    },
                );
            } else if metadata.is_dir() {
                inventory.insert(relative, TreeEntry::Directory);
                stack.push(path);
            } else if metadata.is_file() {
                inventory.insert(
                    relative,
                    TreeEntry::File {
                        size: metadata.len(),
                        modified_nanos: metadata
                            .modified()
                            .ok()
                            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                            .map_or(0, |duration| duration.as_nanos()),
                    },
                );
            } else {
                return Err(user_path_err(
                    "unsafe_campaign_tree",
                    "campaign tree contains an unsupported filesystem object",
                    &path,
                    false,
                ));
            }
        }
    }
    Ok(inventory)
}

fn canonical_relative(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|error| {
        internal_err(
            "campaign_path_outside_root",
            "StarVault could not inspect the campaign tree",
            error.to_string(),
        )
    })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_string_lossy().into_owned()),
            _ => {
                return Err(user_path_err(
                    "unsafe_campaign_tree",
                    "campaign tree contains a non-canonical path",
                    path,
                    false,
                ));
            }
        }
    }
    Ok(parts.join("/"))
}

fn live_link_targets(live: &Path, target: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(live) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(user_path_err(
                "inspect_campaign_root",
                error.to_string(),
                live,
                true,
            ));
        }
    };
    if !is_link(&metadata) {
        return Ok(false);
    }
    Ok(canonical_link_target(live)? == canonical_target_allow_missing(target)?)
}

fn verify_link_target(link: &Path, expected: &Path) -> Result<()> {
    if canonical_link_target(link)? == canonical_target_allow_missing(expected)? {
        Ok(())
    } else {
        Err(user_path_err(
            "unowned_campaign_slot_link",
            "Maps/Campaign points outside StarVault's deployment store",
            link,
            false,
        ))
    }
}

fn canonical_link_target(link: &Path) -> Result<PathBuf> {
    let target = std::fs::read_link(link)
        .map_err(|error| user_path_err("inspect_campaign_link", error.to_string(), link, true))?;
    let absolute = if target.is_absolute() {
        target
    } else {
        link.parent().unwrap_or_else(|| Path::new("")).join(target)
    };
    canonical_target_allow_missing(&absolute)
}

fn canonical_target_allow_missing(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(_) => {
                let name = ancestor.file_name().ok_or_else(|| {
                    user_path_err(
                        "inspect_campaign_link",
                        "campaign link target has no resolvable ancestor",
                        path,
                        false,
                    )
                })?;
                missing.push(name.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    user_path_err(
                        "inspect_campaign_link",
                        "campaign link target has no resolvable ancestor",
                        path,
                        false,
                    )
                })?;
            }
        }
    }
}

fn plain_path(live: &Path) -> PathBuf {
    live.with_file_name("Campaign.starvault-plain")
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if is_safe_operation_id(operation_id) {
        Ok(())
    } else {
        Err(package_err(
            "invalid_operation_id",
            "operation id is not safe for campaign artifact paths",
        ))
    }
}

fn ensure_absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_path_err(
            "inspect_campaign_artifact",
            error.to_string(),
            path,
            true,
        )),
        Ok(_) => Err(user_path_err(
            "campaign_artifact_collision",
            "campaign operation path already exists",
            path,
            false,
        )),
    }
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_campaign_root", error.to_string(), path, true))?;
    if !is_link(&metadata) && metadata.is_dir() {
        Ok(())
    } else {
        Err(user_path_err(
            "unsafe_campaign_root",
            format!("{label} must be a real directory"),
            path,
            false,
        ))
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_path_err(
            "inspect_campaign_artifact",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn read_dir(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| user_path_err("read_campaign_tree", error.to_string(), path, true))?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| user_path_err("read_campaign_tree", error.to_string(), path, true))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn remove_bound_if_exists(path: &Path, binding: &SlotStateBinding) -> Result<()> {
    if path_exists(path)? {
        verify_state_at(path, binding)?;
        remove_entry(path)?;
    }
    Ok(())
}

fn remove_entry_if_exists(path: &Path) -> Result<()> {
    if path_exists(path)? {
        remove_entry(path)
    } else {
        Ok(())
    }
}

fn remove_entry(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        user_path_err("inspect_campaign_artifact", error.to_string(), path, true)
    })?;
    if is_link(&metadata) {
        retry_filesystem(path, || remove_link(path))
    } else if metadata.is_dir() {
        for entry in read_dir(path)? {
            remove_entry(&entry.path())?;
        }
        retry_filesystem(path, || std::fs::remove_dir(path))
    } else {
        retry_filesystem(path, || std::fs::remove_file(path))
    }
}

fn rename_with_retry(source: &Path, destination: &Path) -> Result<()> {
    retry_filesystem(source, || std::fs::rename(source, destination))
}

fn retry_filesystem(path: &Path, mut operation: impl FnMut() -> std::io::Result<()>) -> Result<()> {
    let mut last = None;
    for attempt in 0..RETRY_ATTEMPTS {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) if retryable_io(&error) && attempt + 1 < RETRY_ATTEMPTS => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(
                    RETRY_BASE_MS * (attempt as u64 + 1),
                ));
            }
            Err(error) => {
                return Err(user_path_err(
                    "campaign_filesystem_operation_failed",
                    error.to_string(),
                    path,
                    retryable_io(&error),
                ));
            }
        }
    }
    let error = last.expect("retry loop records its last error");
    Err(user_path_err(
        "campaign_filesystem_operation_failed",
        error.to_string(),
        path,
        true,
    ))
}

fn retryable_io(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
    ) || matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(bytes))
}

#[cfg(windows)]
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    junction::create(target, link)
}

#[cfg(unix)]
fn make_junction(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(any(windows, unix)))]
fn make_junction(_link: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "directory links are unsupported on this platform",
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
    fn state_binding_preserves_links_without_traversing_them() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("Campaign");
        let external = temporary.path().join("external");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("payload"), b"external").unwrap();
        make_junction(&root.join("linked"), &external).unwrap();

        let binding = capture_state(SlotId::Wol, &root).unwrap();
        verify_state_at(&root, &binding).unwrap();
        assert_eq!(
            std::fs::read(external.join("payload")).unwrap(),
            b"external"
        );
    }

    #[test]
    fn full_campaign_tree_places_the_selected_faction_only() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(temporary.path().join("store")).unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir_all(source.join("map.SC2Map")).unwrap();
        std::fs::write(source.join("map.SC2Map/payload"), b"map").unwrap();
        let plan = crate::package::normalize::plan_from_extracted(&source).unwrap();
        let id = crate::identity::PackageId::parse("root-view").unwrap();
        store.ingest(&id, SlotId::LotV, &plan).unwrap();
        let manifest = store.load_manifest(&id).unwrap();
        let deployment = temporary.path().join("deployment");
        store.materialize_campaign(&manifest, &deployment).unwrap();

        verify_campaign_tree(&deployment, &manifest).unwrap();
        assert!(deployment.join("void/map.SC2Map/payload").is_file());
        assert!(std::fs::read_dir(deployment.join("swarm"))
            .unwrap()
            .next()
            .is_none());
    }
}
