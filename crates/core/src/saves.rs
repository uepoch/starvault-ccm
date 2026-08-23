//! Transactional campaign save isolation.
//!
//! Root campaign-progress files are faction-scoped. `Campaign`, `Unsaved`,
//! and non-vanilla `Banks` entries form one global set owned by the active
//! package, or by vanilla when no package is active. Transitions are prepared
//! without touching live data, then applied with a verified backup available
//! for rollback. The application workflow owns the durable journal.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{internal_err, user_err, user_path_err, Result};
use crate::identity::{PackageId, ProfileId};
use crate::layout::SlotId;
use crate::operation::SaveRecoveryProof;

const SWEPT_DIRS: [&str; 2] = ["Campaign", "Unsaved"];
const RETRY_ATTEMPTS: u32 = 6;
const SET_LAYOUT_VERSION: &str = "v2";
const APPLY_STARTED: &str = ".apply-started";
const APPLY_COMPLETE: &str = ".apply-complete";
const RECEIPT_VERSION: u32 = 2;
const RECEIPTS_DIR: &str = "receipts";
const RECOVERY_PROOF_VERSION: u32 = 1;

static NEXT_BACKUP: AtomicU64 = AtomicU64::new(1);

/// The global owner of campaign saves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveOwner {
    Plain,
    Package(PackageId),
}

impl SaveOwner {
    fn directory(&self, root: &Path) -> PathBuf {
        match self {
            Self::Plain => root.join("plain"),
            Self::Package(id) => root.join("packages").join(id.as_str()),
        }
    }
}

/// One complete save-owner transition. A missing faction means vanilla has no
/// active campaign on that side of the transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveTransition {
    pub previous_owner: SaveOwner,
    pub previous_faction: Option<SlotId>,
    pub target_owner: SaveOwner,
    pub target_faction: Option<SlotId>,
}

impl SaveTransition {
    pub fn is_noop(&self) -> bool {
        self.previous_owner == self.target_owner && self.previous_faction == self.target_faction
    }

    fn validate(&self) -> Result<()> {
        if matches!(self.previous_owner, SaveOwner::Package(_)) && self.previous_faction.is_none() {
            return Err(user_err(
                "invalid_save_transition",
                "a package save owner requires a previous faction",
            ));
        }
        if matches!(self.target_owner, SaveOwner::Package(_)) && self.target_faction.is_none() {
            return Err(user_err(
                "invalid_save_transition",
                "a package save owner requires a target faction",
            ));
        }
        Ok(())
    }

    fn affected_factions(&self) -> Vec<SlotId> {
        let mut factions = Vec::new();
        if let Some(faction) = self.previous_faction {
            push_unique_faction(&mut factions, faction);
        }
        if let Some(faction) = self.target_faction {
            push_unique_faction(&mut factions, faction);
        }
        factions
    }

    fn root_updates(&self) -> Vec<(SaveOwner, SlotId)> {
        let mut updates = Vec::new();
        if let Some(faction) = self.previous_faction {
            push_unique_root(&mut updates, self.previous_owner.clone(), faction);
        } else if self.previous_owner == SaveOwner::Plain {
            if let Some(faction) = self.target_faction {
                push_unique_root(&mut updates, SaveOwner::Plain, faction);
            }
        }
        if let (Some(previous), Some(target)) = (self.previous_faction, self.target_faction) {
            if previous != target && matches!(self.target_owner, SaveOwner::Package(_)) {
                push_unique_root(&mut updates, SaveOwner::Plain, target);
            }
        }
        updates
    }

    fn desired_roots(&self) -> Vec<(SaveOwner, SlotId)> {
        let mut desired = Vec::new();
        match (&self.target_owner, self.target_faction) {
            (SaveOwner::Package(_), Some(faction)) => {
                push_unique_root(&mut desired, self.target_owner.clone(), faction);
            }
            (SaveOwner::Plain, Some(faction)) => {
                push_unique_root(&mut desired, SaveOwner::Plain, faction);
            }
            (SaveOwner::Plain, None) => {
                if let Some(faction) = self.previous_faction {
                    push_unique_root(&mut desired, SaveOwner::Plain, faction);
                }
            }
            (SaveOwner::Package(_), None) => {}
        }
        if let (Some(previous), Some(target)) = (self.previous_faction, self.target_faction) {
            if previous != target {
                push_unique_root(&mut desired, SaveOwner::Plain, previous);
            }
        }
        desired
    }
}

fn push_unique_faction(factions: &mut Vec<SlotId>, faction: SlotId) {
    if !factions.contains(&faction) {
        factions.push(faction);
    }
}

fn push_unique_root(roots: &mut Vec<(SaveOwner, SlotId)>, owner: SaveOwner, faction: SlotId) {
    if !roots
        .iter()
        .any(|candidate| candidate == &(owner.clone(), faction))
    {
        roots.push((owner, faction));
    }
}

/// One freshly discovered local SC2 save profile. The label is for local UI
/// display only. Filesystem paths never cross IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavesProfile {
    pub id: ProfileId,
    pub display_label: String,
    #[serde(skip)]
    saves: PathBuf,
    #[serde(skip)]
    banks: PathBuf,
}

impl SavesProfile {
    pub fn saves_dir(&self) -> &Path {
        &self.saves
    }

    pub fn banks_dir(&self) -> &Path {
        &self.banks
    }
}

/// Enumerate current save profiles. Missing SC2 account data means an empty
/// result; unreadable existing data is an error rather than a hidden profile.
pub fn discover(documents: &Path) -> Result<Vec<SavesProfile>> {
    let accounts_root = documents.join("StarCraft II").join("Accounts");
    if !ensure_profile_directory_chain(documents, &accounts_root)? {
        return Ok(Vec::new());
    }
    let accounts = match std::fs::read_dir(&accounts_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("discover_save_profiles", &accounts_root, error)),
    };
    let mut profiles = Vec::new();
    for account in accounts {
        let account =
            account.map_err(|error| io_error("discover_save_profiles", &accounts_root, error))?;
        ensure_profile_directory(&account.path(), "StarCraft II account directory")?;
        let account_name = account.file_name().to_string_lossy().into_owned();
        let children = std::fs::read_dir(account.path())
            .map_err(|error| io_error("discover_save_profiles", &account.path(), error))?;
        for profile in children {
            let profile = profile
                .map_err(|error| io_error("discover_save_profiles", &account.path(), error))?;
            let profile_name = profile.file_name().to_string_lossy().into_owned();
            if !profile_name.contains("-S2-") {
                continue;
            }
            ensure_profile_directory(&profile.path(), "StarCraft II profile directory")?;
            let saves = profile.path().join("Saves");
            match std::fs::symlink_metadata(&saves) {
                Ok(metadata) => ensure_profile_directory_metadata(
                    &saves,
                    &metadata,
                    "StarCraft II Saves directory",
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("inspect_save_profile", &saves, error)),
            }
            let banks = profile.path().join("Banks");
            match std::fs::symlink_metadata(&banks) {
                Ok(metadata) => ensure_profile_directory_metadata(
                    &banks,
                    &metadata,
                    "StarCraft II Banks directory",
                )?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("inspect_save_profile", &banks, error)),
            }
            let label = format!("{account_name}/{profile_name}");
            profiles.push(SavesProfile {
                id: ProfileId::discovered(label.as_bytes()),
                display_label: label,
                banks,
                saves,
            });
        }
    }
    profiles.sort_by(|left, right| left.display_label.cmp(&right.display_label));
    Ok(profiles)
}

fn ensure_profile_directory_chain(root: &Path, path: &Path) -> Result<bool> {
    let root_metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_error("inspect_save_profile", root, error)),
    };
    ensure_profile_directory_metadata(root, &root_metadata, "Documents directory")?;
    let relative = path.strip_prefix(root).map_err(|error| {
        internal_err(
            "save_profile_path_escape",
            "StarVault could not inspect save profiles",
            error.to_string(),
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(unsafe_profile_path(path, "save profile path"));
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure_profile_directory_metadata(&current, &metadata, "save profile path")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("inspect_save_profile", &current, error)),
        }
    }
    Ok(true)
}

fn ensure_profile_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect_save_profile", path, error))?;
    ensure_profile_directory_metadata(path, &metadata, label)
}

fn ensure_optional_profile_directory(path: &Path, label: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure_profile_directory_metadata(path, &metadata, label)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect_save_profile", path, error)),
    }
}

fn ensure_profile_directory_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    label: &str,
) -> Result<()> {
    if metadata.is_dir() && !is_link_or_reparse(metadata) {
        Ok(())
    } else {
        Err(unsafe_profile_path(path, label))
    }
}

fn unsafe_profile_path(path: &Path, label: &str) -> crate::Error {
    user_path_err(
        "unsafe_save_profile",
        format!("{label} must be a real directory, not a link or reparse point"),
        path,
        false,
    )
}

fn ensure_recovery_directory<I: SaveIo>(
    io: &I,
    documents: &Path,
    path: &Path,
    create: bool,
) -> Result<bool> {
    ensure_profile_directory(documents, "Documents directory")?;
    let relative = path.strip_prefix(documents).map_err(|error| {
        internal_err(
            "recovery_backup_path_escape",
            "StarVault could not create a recovery backup",
            error.to_string(),
        )
    })?;
    let mut current = documents.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(unsafe_recovery_path(path));
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => {}
            Ok(_) => return Err(unsafe_recovery_path(&current)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(false);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match retry_io(io, || io.create_dir(&current)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(io_error("create_recovery_directory", &current, error));
                    }
                }
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|error| io_error("inspect_recovery_directory", &current, error))?;
                if !metadata.is_dir() || is_link_or_reparse(&metadata) {
                    return Err(unsafe_recovery_path(&current));
                }
            }
            Err(error) => return Err(io_error("inspect_recovery_directory", &current, error)),
        }
    }
    Ok(true)
}

fn remove_recovery_directory_if_exists<I: SaveIo>(
    io: &I,
    documents: &Path,
    path: &Path,
) -> Result<()> {
    if !ensure_recovery_directory(io, documents, path, false)? {
        return Ok(());
    }
    ensure_recovery_directory(io, documents, path, false)?;
    remove_if_exists(io, path)
}

fn unsafe_recovery_path(path: &Path) -> crate::Error {
    user_path_err(
        "unsafe_recovery_backup_path",
        "recovery backup directories must not be links or reparse points",
        path,
        false,
    )
}

/// Resolve an opaque profile token only against a fresh discovery pass.
pub fn resolve_profile(documents: &Path, profile_id: &ProfileId) -> Result<SavesProfile> {
    discover(documents)?
        .into_iter()
        .find(|profile| &profile.id == profile_id)
        .ok_or_else(|| {
            user_err(
                "save_profile_not_found",
                "the selected StarCraft II save profile no longer exists",
            )
        })
}

/// Convenience for callers that only need the freshly resolved Saves path.
pub fn saves_dir(documents: &Path, profile_id: &ProfileId) -> Result<PathBuf> {
    Ok(resolve_profile(documents, profile_id)?.saves)
}

/// True for conventional OneDrive folder names.
pub fn is_onedrive(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.eq_ignore_ascii_case("onedrive")
            || name.to_ascii_lowercase().starts_with("onedrive - ")
    })
}

/// OneDrive detection when the shell also supplies its known consumer and
/// commercial roots. This catches renamed organization folders.
pub fn is_onedrive_with_roots(path: &Path, roots: &[PathBuf]) -> bool {
    is_onedrive(path)
        || roots
            .iter()
            .any(|root| path_starts_with_case_insensitive(path, root))
}

fn path_starts_with_case_insensitive(path: &Path, root: &Path) -> bool {
    let path: Vec<String> = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let root: Vec<String> = root
        .components()
        .map(|part| part.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    path.len() >= root.len() && path[..root.len()] == root
}

/// Save-file prefixes owned by each faction.
pub fn save_prefixes(slot: SlotId) -> &'static [&'static str] {
    match slot {
        SlotId::Wol => &["LibertyCampaign"],
        SlotId::HotS => &["SwarmCampaign", "SwarmPublish"],
        SlotId::LotV => &[
            "VoidCampaign",
            "VoidPublish",
            "VoidPrologue",
            "VoidEpilogue",
        ],
        SlotId::Nco => &["NovaCampaign"],
    }
}

/// Fixed Blizzard bank files stay live across every custom campaign.
const VANILLA_BANKS: [&str; 11] = [
    "warchive",
    "warmy",
    "wcampaign",
    "wstory",
    "zarchive",
    "zcampaignstats",
    "parchive",
    "pprologue",
    "prologuearchive",
    "epiloguearchive",
    "sc2epilogue",
];

/// Paths exported to the workflow journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveOperationPaths {
    pub saves_staging: PathBuf,
    pub saves_backup: PathBuf,
    pub banks_staging: PathBuf,
    pub banks_backup: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SaveCommitReceipt {
    version: u32,
    operation_id: String,
    transition: SaveTransition,
    saves_fingerprint: String,
    banks_fingerprint: String,
    recovery_proof_sha256: String,
}

#[derive(Debug, Serialize)]
struct RecoveryProofIdentity<'a> {
    version: u32,
    operation_id: &'a str,
    transition: &'a SaveTransition,
}

#[derive(Debug, Serialize)]
struct LogicalSetFingerprint {
    key: String,
    sha256: String,
}

/// Core save manager. Save-set state lives under `<store>/saves/v2`; operation
/// artifacts live under `<store>/save-operations/<operation-id>`.
#[derive(Debug, Clone)]
pub struct SavesManager {
    live: PathBuf,
    banks: PathBuf,
    store_root: PathBuf,
    sets_root: PathBuf,
    operations_root: PathBuf,
}

impl SavesManager {
    pub fn new(live_saves_dir: PathBuf, store_root: &Path) -> Self {
        let banks = live_saves_dir
            .parent()
            .map(|parent| parent.join("Banks"))
            .unwrap_or_else(|| live_saves_dir.join("Banks"));
        Self {
            live: live_saves_dir,
            banks,
            store_root: store_root.to_path_buf(),
            sets_root: store_root.join("saves").join(SET_LAYOUT_VERSION),
            operations_root: store_root.join("save-operations"),
        }
    }

    pub fn prepare(
        &self,
        transition: SaveTransition,
        operation_id: &str,
    ) -> Result<PreparedSaveTransition> {
        self.prepare_with(transition, operation_id, &SystemSaveIo)
    }

    #[doc(hidden)]
    pub fn prepare_with<I: SaveIo>(
        &self,
        transition: SaveTransition,
        operation_id: &str,
        io: &I,
    ) -> Result<PreparedSaveTransition> {
        transition.validate()?;
        validate_operation_id(operation_id)?;
        self.ensure_store_layout(io)?;
        self.ensure_live_profile_roots()?;
        self.ensure_transition_set_directories(io, &transition)?;
        let mut artifact = self.artifact(transition, operation_id);
        if artifact.receipt_exists()? {
            return Err(user_path_err(
                "save_operation_already_committed",
                "this save operation has already committed",
                artifact.receipt_path(),
                false,
            ));
        }
        if ensure_internal_directory(
            io,
            &self.store_root,
            &artifact.operation_root,
            false,
            "save operation",
        )? {
            return Err(user_path_err(
                "save_operation_exists",
                "save operation artifacts already exist",
                &artifact.operation_root,
                false,
            ));
        }
        for path in [
            artifact.saves_stage_live(),
            artifact.banks_stage_live(),
            artifact.saves_backup_live(),
            artifact.banks_backup_live(),
        ] {
            ensure_internal_directory(
                io,
                &self.store_root,
                &path,
                true,
                "save operation artifact",
            )?;
        }

        let prepared = (|| -> Result<SaveRecoveryProof> {
            if !artifact.transition.is_noop() {
                artifact.stage_backup(io)?;
                artifact.stage_set_updates(io)?;
                artifact.stage_desired_live(io)?;
                artifact.verify_backup_matches_live(io)?;
            }
            artifact.build_recovery_proof(io)
        })();
        artifact.recovery_proof = Some(match prepared {
            Ok(proof) => proof,
            Err(error) => {
                return match artifact.remove_operation_root(io) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(cleanup_failure(
                        "save_prepare_cleanup_failed",
                        "StarVault could not clean up an incomplete save preparation",
                        &error,
                        &cleanup,
                    )),
                };
            }
        });
        Ok(artifact)
    }

    /// Return deterministic artifact paths before preparation has produced a
    /// recovery proof.
    pub fn planned_paths(
        &self,
        transition: SaveTransition,
        operation_id: &str,
    ) -> Result<SaveOperationPaths> {
        transition.validate()?;
        validate_operation_id(operation_id)?;
        Ok(self.artifact(transition, operation_id).paths)
    }

    /// Reconstruct a proof-bound transition during startup recovery.
    pub fn prepared(
        &self,
        transition: SaveTransition,
        operation_id: &str,
        recovery_proof: SaveRecoveryProof,
    ) -> Result<PreparedSaveTransition> {
        transition.validate()?;
        validate_operation_id(operation_id)?;
        let mut artifact = self.artifact(transition, operation_id);
        artifact.validate_recovery_proof_identity(&recovery_proof)?;
        artifact.recovery_proof = Some(recovery_proof);
        Ok(artifact)
    }

    /// Reconstruct paths for cleanup of a `Preparing` journal, which cannot
    /// yet contain a recovery proof and must not have changed the live profile.
    pub(crate) fn preparing(
        &self,
        transition: SaveTransition,
        operation_id: &str,
    ) -> Result<PreparedSaveTransition> {
        transition.validate()?;
        validate_operation_id(operation_id)?;
        Ok(self.artifact(transition, operation_id))
    }

    fn ensure_store_layout<I: SaveIo>(&self, io: &I) -> Result<()> {
        ensure_store_root(io, &self.store_root)?;
        ensure_internal_directory(
            io,
            &self.store_root,
            &self.operations_root,
            true,
            "save operations root",
        )?;
        ensure_internal_directory(
            io,
            &self.store_root,
            &self.operations_root.join(RECEIPTS_DIR),
            true,
            "save receipts root",
        )?;
        ensure_internal_directory(
            io,
            &self.store_root,
            &self.sets_root,
            true,
            "save sets root",
        )?;
        Ok(())
    }

    fn ensure_live_profile_roots(&self) -> Result<()> {
        ensure_profile_directory(&self.live, "selected Saves directory")?;
        ensure_optional_profile_directory(&self.banks, "selected Banks directory")?;
        Ok(())
    }

    fn ensure_transition_set_directories<I: SaveIo>(
        &self,
        io: &I,
        transition: &SaveTransition,
    ) -> Result<()> {
        let mut paths = Vec::new();
        for (owner, faction) in transition
            .root_updates()
            .into_iter()
            .chain(transition.desired_roots())
        {
            paths.push(
                owner
                    .directory(&self.sets_root)
                    .join("roots")
                    .join(faction.as_str()),
            );
        }
        for owner in [&transition.previous_owner, &transition.target_owner] {
            let owner = owner.directory(&self.sets_root);
            paths.push(owner.join("global"));
            paths.push(owner.join("global-banks"));
        }
        paths.sort();
        paths.dedup();
        for path in paths {
            ensure_internal_directory(io, &self.store_root, &path, true, "save owner set")?;
        }
        Ok(())
    }

    fn artifact(&self, transition: SaveTransition, operation_id: &str) -> PreparedSaveTransition {
        let operation_root = self.operations_root.join(operation_id);
        PreparedSaveTransition {
            manager: self.clone(),
            transition,
            operation_id: operation_id.to_string(),
            paths: SaveOperationPaths {
                saves_staging: operation_root.join("saves-staging"),
                saves_backup: operation_root.join("saves-backup"),
                banks_staging: operation_root.join("banks-staging"),
                banks_backup: operation_root.join("banks-backup"),
            },
            operation_root,
            recovery_proof: None,
        }
    }
}

/// A prepared save transition. Methods are idempotent so startup recovery can
/// repeat the phase selected by the durable workflow journal.
#[derive(Debug, Clone)]
pub struct PreparedSaveTransition {
    manager: SavesManager,
    transition: SaveTransition,
    operation_id: String,
    operation_root: PathBuf,
    paths: SaveOperationPaths,
    recovery_proof: Option<SaveRecoveryProof>,
}

impl PreparedSaveTransition {
    pub fn paths(&self) -> &SaveOperationPaths {
        &self.paths
    }

    pub fn transition(&self) -> &SaveTransition {
        &self.transition
    }

    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn recovery_proof(&self) -> Result<&SaveRecoveryProof> {
        self.recovery_proof.as_ref().ok_or_else(|| {
            internal_err(
                "missing_save_recovery_proof",
                "StarVault could not verify the prepared save transition",
                "prepared save transition is not bound to the operation journal",
            )
        })
    }

    fn build_recovery_proof<I: SaveIo>(&self, io: &I) -> Result<SaveRecoveryProof> {
        let transition_sha256 = recovery_identity_sha256(&self.operation_id, &self.transition)?;
        let set_updates_sha256 = if self.transition.is_noop() {
            fingerprint_logical_sets(&[])?
        } else {
            fingerprint_logical_sets(&self.staged_set_fingerprints(io)?)?
        };
        let (previous_saves, previous_banks, target_saves, target_banks) =
            if self.transition.is_noop() {
                let (saves, banks) = self.live_snapshots(io)?;
                (saves.clone(), banks.clone(), saves, banks)
            } else {
                (
                    snapshot_tree(io, &self.saves_backup_live())?,
                    snapshot_tree(io, &self.banks_backup_live())?,
                    snapshot_tree(io, &self.saves_stage_live())?,
                    snapshot_tree(io, &self.banks_stage_live())?,
                )
            };
        Ok(SaveRecoveryProof {
            version: RECOVERY_PROOF_VERSION,
            operation_id: self.operation_id.clone(),
            transition_sha256,
            previous_saves_sha256: fingerprint_snapshot(&previous_saves)?,
            previous_banks_sha256: fingerprint_snapshot(&previous_banks)?,
            target_saves_sha256: fingerprint_snapshot(&target_saves)?,
            target_banks_sha256: fingerprint_snapshot(&target_banks)?,
            set_updates_sha256,
        })
    }

    fn validate_recovery_proof_identity(&self, proof: &SaveRecoveryProof) -> Result<()> {
        if proof.version != RECOVERY_PROOF_VERSION
            || proof.operation_id != self.operation_id
            || proof.transition_sha256
                != recovery_identity_sha256(&self.operation_id, &self.transition)?
        {
            return Err(user_err(
                "invalid_save_recovery_proof",
                "save recovery proof does not match the pending operation",
            ));
        }
        for digest in [
            &proof.transition_sha256,
            &proof.previous_saves_sha256,
            &proof.previous_banks_sha256,
            &proof.target_saves_sha256,
            &proof.target_banks_sha256,
            &proof.set_updates_sha256,
        ] {
            if !is_lowercase_sha256(digest) {
                return Err(user_err(
                    "invalid_save_recovery_proof",
                    "save recovery proof contains an invalid fingerprint",
                ));
            }
        }
        Ok(())
    }

    fn proof_sha256(&self) -> Result<String> {
        fingerprint_serialized(self.recovery_proof()?, "serialize_save_recovery_proof")
    }

    fn verify_static_artifacts<I: SaveIo>(&self, io: &I) -> Result<()> {
        let proof = self.recovery_proof()?;
        self.validate_recovery_proof_identity(proof)?;
        self.verify_operation_structure_readonly()?;
        if self.transition.is_noop() {
            return Ok(());
        }
        verify_snapshot_fingerprint(
            snapshot_tree(io, &self.saves_backup_live())?,
            &proof.previous_saves_sha256,
            &self.paths.saves_backup,
            "prepared Saves backup",
        )?;
        verify_snapshot_fingerprint(
            snapshot_tree(io, &self.banks_backup_live())?,
            &proof.previous_banks_sha256,
            &self.paths.banks_backup,
            "prepared Banks backup",
        )?;
        verify_snapshot_fingerprint(
            snapshot_tree(io, &self.saves_stage_live())?,
            &proof.target_saves_sha256,
            &self.paths.saves_staging,
            "prepared Saves target",
        )?;
        verify_snapshot_fingerprint(
            snapshot_tree(io, &self.banks_stage_live())?,
            &proof.target_banks_sha256,
            &self.paths.banks_staging,
            "prepared Banks target",
        )?;
        let set_updates = fingerprint_logical_sets(&self.staged_set_fingerprints(io)?)?;
        if set_updates != proof.set_updates_sha256 {
            return Err(user_path_err(
                "save_recovery_proof_mismatch",
                "prepared save archives no longer match the operation journal",
                &self.paths.saves_staging,
                false,
            ));
        }
        Ok(())
    }

    fn verify_previous_live<I: SaveIo>(&self, io: &I) -> Result<()> {
        let proof = self.recovery_proof()?;
        let (saves, banks) = self.live_snapshots(io)?;
        if fingerprint_snapshot(&saves)? == proof.previous_saves_sha256
            && fingerprint_snapshot(&banks)? == proof.previous_banks_sha256
        {
            Ok(())
        } else {
            Err(user_path_err(
                "save_verification_failed",
                "save data changed while StarVault was preparing the transition",
                &self.manager.live,
                true,
            ))
        }
    }

    fn verify_target_live<I: SaveIo>(&self, io: &I) -> Result<()> {
        let proof = self.recovery_proof()?;
        let (saves, banks) = self.live_snapshots(io)?;
        verify_snapshot_fingerprint(
            saves,
            &proof.target_saves_sha256,
            &self.manager.live,
            "live Saves target state",
        )?;
        verify_snapshot_fingerprint(
            banks,
            &proof.target_banks_sha256,
            &self.manager.banks,
            "live Banks target state",
        )
    }

    /// Read-only rollback preflight. The workflow calls this before restoring
    /// Mods or campaign slots so bad save evidence cannot leave a mixed state.
    pub fn verify_rollback_ready(&self) -> Result<()> {
        self.verify_rollback_ready_with(&SystemSaveIo)
    }

    #[doc(hidden)]
    pub fn verify_rollback_ready_with<I: SaveIo>(&self, io: &I) -> Result<()> {
        let proof = self.recovery_proof()?;
        self.validate_recovery_proof_identity(proof)?;
        if self.receipt_exists_readonly()? {
            return Err(user_err(
                "save_operation_already_committed",
                "a committed save transition cannot be rolled back",
            ));
        }
        let operation_exists = verify_internal_directory_readonly(
            &self.manager.store_root,
            &self.operation_root,
            "save operation",
        )?;
        if !operation_exists {
            return self.verify_previous_live(io);
        }
        let started = self.marker_exists_readonly(APPLY_STARTED)?;
        let complete = self.marker_exists_readonly(APPLY_COMPLETE)?;
        if complete && !started {
            return Err(user_err(
                "invalid_save_operation_markers",
                "save operation completion marker exists without a start marker",
            ));
        }
        if !started {
            return self.verify_previous_live(io);
        }
        self.verify_static_artifacts(io)?;
        self.verify_live_rollback_state(io, complete)
    }

    fn verify_live_rollback_state<I: SaveIo>(&self, io: &I, complete: bool) -> Result<()> {
        if complete {
            return self.verify_target_live(io);
        }
        let (actual_saves, actual_banks) = self.live_snapshots(io)?;
        verify_snapshot_entries_known(
            actual_saves,
            snapshot_tree(io, &self.saves_backup_live())?,
            snapshot_tree(io, &self.saves_stage_live())?,
            &self.manager.live,
        )?;
        verify_snapshot_entries_known(
            actual_banks,
            snapshot_tree(io, &self.banks_backup_live())?,
            snapshot_tree(io, &self.banks_stage_live())?,
            &self.manager.banks,
        )
    }

    pub fn apply(&self) -> Result<()> {
        self.apply_with_policy(&SystemSaveIo, true)
    }

    /// Apply under the application workflow's cross-resource journal. A
    /// failure preserves the started marker and backup so the workflow can
    /// re-check that SC2 is stopped before rolling anything back.
    pub(crate) fn apply_journaled(&self) -> Result<()> {
        self.apply_with_policy(&SystemSaveIo, false)
    }

    #[doc(hidden)]
    pub fn apply_with<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.apply_with_policy(io, true)
    }

    fn apply_with_policy<I: SaveIo>(&self, io: &I, rollback_on_error: bool) -> Result<()> {
        if self.receipt_exists()? {
            return self.verify_committed_with(io);
        }
        self.verify_static_artifacts(io)?;
        if self.transition.is_noop() {
            self.verify_previous_live(io)?;
            self.write_marker(io, APPLY_STARTED, b"started")?;
            self.write_marker(io, APPLY_COMPLETE, b"complete")?;
            return Ok(());
        }
        if self.marker_exists(io, APPLY_COMPLETE)? {
            self.verify_target_live(io)?;
            return self.verify_desired_live(io);
        }
        if self.marker_exists(io, APPLY_STARTED)? {
            if rollback_on_error {
                self.rollback_with(io)?;
            }
            return Err(user_err(
                "save_operation_interrupted",
                if rollback_on_error {
                    "an interrupted save transition was rolled back; retry the operation"
                } else {
                    "an interrupted save transition requires workflow recovery"
                },
            ));
        }
        self.verify_previous_live(io)?;
        self.verify_backup_matches_live(io)?;
        self.write_marker(io, APPLY_STARTED, b"started")?;
        let applied = (|| -> Result<()> {
            self.ensure_operation_structure(io)?;
            self.remove_affected_live(io)?;
            self.manager.ensure_live_profile_roots()?;
            copy_children_verified(io, &self.saves_stage_live(), &self.manager.live)?;
            copy_children_verified(io, &self.banks_stage_live(), &self.manager.banks)?;
            self.verify_desired_live(io)?;
            self.write_marker(io, APPLY_COMPLETE, b"complete")?;
            Ok(())
        })();
        if let Err(error) = applied {
            if rollback_on_error {
                if let Err(rollback) = self.rollback_with(io) {
                    return Err(internal_err(
                        "save_rollback_failed",
                        "StarVault could not restore the previous save state",
                        format!("apply failed: {error}; rollback failed: {rollback}"),
                    ));
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        self.rollback_with(&SystemSaveIo)
    }

    /// Remove artifacts created while preparing an operation that was durably
    /// journaled but never allowed to begin its live save swap.
    pub(crate) fn discard_prepared(&self) -> Result<()> {
        self.manager.ensure_store_layout(&SystemSaveIo)?;
        for marker in [APPLY_STARTED, APPLY_COMPLETE] {
            if self.marker_exists(&SystemSaveIo, marker)? {
                return Err(user_path_err(
                    "save_operation_started",
                    "save preparation cannot be discarded after the live swap started",
                    self.operation_root.join(marker),
                    false,
                ));
            }
        }
        if self.receipt_exists()? {
            return Err(user_path_err(
                "save_operation_already_committed",
                "committed save artifacts cannot be discarded as preparation",
                self.receipt_path(),
                false,
            ));
        }
        self.remove_operation_root(&SystemSaveIo)
    }

    #[doc(hidden)]
    pub fn rollback_with<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.verify_rollback_ready_with(io)?;
        if self.transition.is_noop() {
            return self.remove_operation_root(io);
        }
        if !verify_internal_directory_readonly(
            &self.manager.store_root,
            &self.operation_root,
            "save operation",
        )? {
            return Ok(());
        }
        if !self.marker_exists(io, APPLY_STARTED)? {
            return self.remove_operation_root(io);
        }
        self.ensure_operation_structure(io)?;
        self.remove_affected_live(io)?;
        self.manager.ensure_live_profile_roots()?;
        copy_children_verified(io, &self.saves_backup_live(), &self.manager.live)?;
        copy_children_verified(io, &self.banks_backup_live(), &self.manager.banks)?;
        self.verify_backup_matches_live(io)?;
        self.remove_marker(io, APPLY_COMPLETE)?;
        self.remove_marker(io, APPLY_STARTED)?;
        self.remove_operation_root(io)
    }

    pub fn finalize(&self) -> Result<()> {
        self.finalize_with(&SystemSaveIo)
    }

    /// Read-only committed-cleanup preflight. The application workflow calls
    /// this before finalizing any other resource, so corrupt save evidence
    /// cannot cause partial cross-resource cleanup.
    pub(crate) fn verify_finalize_ready(&self) -> Result<()> {
        self.verify_finalize_ready_with(&SystemSaveIo)
    }

    fn verify_finalize_ready_with<I: SaveIo>(&self, io: &I) -> Result<()> {
        if self.receipt_exists()? {
            return self.verify_committed_with(io);
        }
        self.verify_static_artifacts(io)?;
        if !self.marker_exists(io, APPLY_COMPLETE)? {
            return Err(user_err(
                "save_transition_not_applied",
                "save transition cannot be finalized before it is applied",
            ));
        }
        if !self.marker_exists(io, APPLY_STARTED)? {
            return Err(user_err(
                "invalid_save_operation_markers",
                "save operation completion marker exists without a start marker",
            ));
        }
        self.verify_target_live(io)?;
        if !self.transition.is_noop() {
            self.verify_desired_live(io)?;
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn finalize_with<I: SaveIo>(&self, io: &I) -> Result<()> {
        if self.receipt_exists()? {
            self.verify_committed_with(io)?;
            return self.cleanup_operation_artifacts(io);
        }
        self.verify_static_artifacts(io)?;
        if !self.marker_exists(io, APPLY_COMPLETE)? {
            return Err(user_err(
                "save_transition_not_applied",
                "save transition cannot be finalized before it is applied",
            ));
        }
        if !self.marker_exists(io, APPLY_STARTED)? {
            return Err(user_err(
                "invalid_save_operation_markers",
                "save operation completion marker exists without a start marker",
            ));
        }
        self.verify_target_live(io)?;
        if self.transition.is_noop() {
            self.write_commit_receipt(io)?;
            return self.cleanup_operation_artifacts(io);
        }
        self.verify_desired_live(io)?;
        for (owner, faction) in self.transition.root_updates() {
            replace_directory(
                io,
                &self.manager.store_root,
                &self.staged_root_update(&owner, faction),
                &self.set_root(&owner, faction),
                &self.operation_id,
            )?;
        }
        replace_directory(
            io,
            &self.manager.store_root,
            &self.staged_global_saves(&self.transition.previous_owner),
            &self.set_global_saves(&self.transition.previous_owner),
            &self.operation_id,
        )?;
        replace_directory(
            io,
            &self.manager.store_root,
            &self.staged_global_banks(&self.transition.previous_owner),
            &self.set_global_banks(&self.transition.previous_owner),
            &self.operation_id,
        )?;
        self.verify_target_from_persistent_sets(io)?;
        self.write_commit_receipt(io)?;
        self.cleanup_operation_artifacts(io)
    }

    /// Verify a previously finalized transition after restart. A receipt is
    /// necessary but not sufficient: live Saves and Banks must still match
    /// both the receipt fingerprint and the persistent target save sets.
    pub fn verify_committed(&self) -> Result<()> {
        self.verify_committed_with(&SystemSaveIo)
    }

    #[doc(hidden)]
    pub fn verify_committed_with<I: SaveIo>(&self, io: &I) -> Result<()> {
        let proof = self.recovery_proof()?;
        self.validate_recovery_proof_identity(proof)?;
        let receipt = self.read_commit_receipt()?;
        if receipt.version != RECEIPT_VERSION
            || receipt.operation_id != self.operation_id
            || receipt.transition != self.transition
            || receipt.recovery_proof_sha256 != self.proof_sha256()?
        {
            return Err(internal_err(
                "invalid_save_commit_receipt",
                "StarVault could not verify the committed save transition",
                "receipt version, operation id, or transition did not match",
            ));
        }
        let (saves, banks) = self.live_snapshots(io)?;
        if fingerprint_snapshot(&saves)? != receipt.saves_fingerprint
            || fingerprint_snapshot(&banks)? != receipt.banks_fingerprint
        {
            return Err(user_path_err(
                "committed_saves_drifted",
                "save data changed before StarVault finished recovering the operation",
                &self.manager.live,
                false,
            ));
        }
        self.verify_target_live(io)?;
        if !self.transition.is_noop() {
            self.verify_target_from_persistent_sets(io)?;
            self.verify_committed_set_updates(io)?;
        }
        Ok(())
    }

    /// Remove the small proof record only after the workflow has durably
    /// cleared its journal. A crash before this call merely leaves an orphan
    /// receipt that can be removed on a later clean startup.
    pub fn clear_receipt(&self) -> Result<()> {
        self.clear_receipt_with(&SystemSaveIo)
    }

    #[doc(hidden)]
    pub fn clear_receipt_with<I: SaveIo>(&self, io: &I) -> Result<()> {
        remove_internal_file_if_exists(
            io,
            &self.manager.store_root,
            &self.receipt_path(),
            "save commit receipt",
        )
    }

    fn cleanup_operation_artifacts<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.manager.ensure_store_layout(io)?;
        for path in [
            &self.paths.saves_staging,
            &self.paths.saves_backup,
            &self.paths.banks_staging,
            &self.paths.banks_backup,
        ] {
            self.remove_operation_directory(io, path)?;
        }
        self.remove_marker(io, APPLY_COMPLETE)?;
        self.remove_marker(io, APPLY_STARTED)?;
        self.remove_operation_root(io)
    }

    fn write_commit_receipt<I: SaveIo>(&self, io: &I) -> Result<()> {
        let (saves, banks) = self.live_snapshots(io)?;
        let receipt = SaveCommitReceipt {
            version: RECEIPT_VERSION,
            operation_id: self.operation_id.clone(),
            transition: self.transition.clone(),
            saves_fingerprint: fingerprint_snapshot(&saves)?,
            banks_fingerprint: fingerprint_snapshot(&banks)?,
            recovery_proof_sha256: self.proof_sha256()?,
        };
        let bytes = serde_json::to_vec_pretty(&receipt).map_err(|error| {
            internal_err(
                "serialize_save_commit_receipt",
                "StarVault could not record the committed save transition",
                error.to_string(),
            )
        })?;
        write_internal_file(
            io,
            &self.manager.store_root,
            &self.receipt_path(),
            &bytes,
            "save commit receipt",
        )
    }

    fn read_commit_receipt(&self) -> Result<SaveCommitReceipt> {
        let path = self.receipt_path();
        if !internal_file_exists(
            &SystemSaveIo,
            &self.manager.store_root,
            &path,
            "save commit receipt",
        )? {
            return Err(user_path_err(
                "invalid_save_commit_receipt",
                "save commit receipt is missing",
                &path,
                false,
            ));
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| io_error("read_save_commit_receipt", &path, error))?;
        serde_json::from_slice(&bytes).map_err(|error| {
            internal_err(
                "invalid_save_commit_receipt",
                "StarVault could not verify the committed save transition",
                error.to_string(),
            )
        })
    }

    fn receipt_exists(&self) -> Result<bool> {
        let path = self.receipt_path();
        internal_file_exists(
            &SystemSaveIo,
            &self.manager.store_root,
            &path,
            "save commit receipt",
        )
    }

    fn receipt_path(&self) -> PathBuf {
        self.manager
            .operations_root
            .join(RECEIPTS_DIR)
            .join(format!("{}.json", self.operation_id))
    }

    fn live_snapshots<I: SaveIo>(
        &self,
        io: &I,
    ) -> Result<(Vec<SnapshotEntry>, Vec<SnapshotEntry>)> {
        self.manager.ensure_live_profile_roots()?;
        let saves = snapshot_selection(
            io,
            &self.manager.live,
            selected_save_entries(
                &self.manager.live,
                &self.transition.affected_factions(),
                true,
            )?,
        )?;
        let banks = snapshot_selection(
            io,
            &self.manager.banks,
            selected_bank_entries(&self.manager.banks)?,
        )?;
        Ok((saves, banks))
    }

    fn verify_target_from_persistent_sets<I: SaveIo>(&self, io: &I) -> Result<()> {
        let (live_saves, live_banks) = self.live_snapshots(io)?;
        let mut expected_saves = Vec::new();
        for (owner, faction) in self.transition.desired_roots() {
            let path = self.set_root(&owner, faction);
            ensure_internal_directory(
                io,
                &self.manager.store_root,
                &path,
                false,
                "save owner set",
            )?;
            expected_saves.extend(snapshot_tree(io, &path)?);
        }
        let global_saves = self.set_global_saves(&self.transition.target_owner);
        let global_banks = self.set_global_banks(&self.transition.target_owner);
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            &global_saves,
            false,
            "save owner set",
        )?;
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            &global_banks,
            false,
            "save owner set",
        )?;
        expected_saves.extend(snapshot_tree(io, &global_saves)?);
        expected_saves.sort();
        let expected_banks = snapshot_tree(io, &global_banks)?;
        compare_snapshots(live_saves, expected_saves, &self.manager.live)?;
        compare_snapshots(live_banks, expected_banks, &self.manager.banks)
    }

    fn stage_backup<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.ensure_operation_structure(io)?;
        self.manager.ensure_live_profile_roots()?;
        copy_selected_saves(
            io,
            &self.manager.live,
            &self.saves_backup_live(),
            &self.transition.affected_factions(),
            true,
        )?;
        copy_selected_banks(io, &self.manager.banks, &self.banks_backup_live())
    }

    fn stage_set_updates<I: SaveIo>(&self, io: &I) -> Result<()> {
        for (owner, faction) in self.transition.root_updates() {
            let destination = self.staged_root_update(&owner, faction);
            ensure_internal_directory(
                io,
                &self.manager.store_root,
                &destination,
                true,
                "staged save set",
            )?;
            let factions = vec![faction];
            copy_selected_saves(io, &self.manager.live, &destination, &factions, false)?;
        }
        let saves = self.staged_global_saves(&self.transition.previous_owner);
        let banks = self.staged_global_banks(&self.transition.previous_owner);
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            &saves,
            true,
            "staged save set",
        )?;
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            &banks,
            true,
            "staged save set",
        )?;
        copy_selected_saves(io, &self.manager.live, &saves, &[], true)?;
        copy_selected_banks(io, &self.manager.banks, &banks)
    }

    fn stage_desired_live<I: SaveIo>(&self, io: &I) -> Result<()> {
        for (owner, faction) in self.transition.desired_roots() {
            let source = self.set_root(&owner, faction);
            ensure_internal_directory(
                io,
                &self.manager.store_root,
                &source,
                false,
                "save owner set",
            )?;
            copy_children_verified(io, &source, &self.saves_stage_live())?;
        }
        let global_saves = self.set_global_saves(&self.transition.target_owner);
        let global_banks = self.set_global_banks(&self.transition.target_owner);
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            &global_saves,
            false,
            "save owner set",
        )?;
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            &global_banks,
            false,
            "save owner set",
        )?;
        copy_children_verified(io, &global_saves, &self.saves_stage_live())?;
        copy_children_verified(io, &global_banks, &self.banks_stage_live())
    }

    fn remove_affected_live<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.manager.ensure_live_profile_roots()?;
        for path in selected_save_entries(
            &self.manager.live,
            &self.transition.affected_factions(),
            true,
        )? {
            remove_if_exists(io, &path)?;
        }
        for path in selected_bank_entries(&self.manager.banks)? {
            remove_if_exists(io, &path)?;
        }
        Ok(())
    }

    fn verify_backup_matches_live<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.ensure_operation_structure(io)?;
        self.manager.ensure_live_profile_roots()?;
        compare_selected_to_directory(
            io,
            &self.manager.live,
            &self.transition.affected_factions(),
            true,
            &self.saves_backup_live(),
        )?;
        compare_bank_selection_to_directory(io, &self.manager.banks, &self.banks_backup_live())
    }

    fn verify_desired_live<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.ensure_operation_structure(io)?;
        self.manager.ensure_live_profile_roots()?;
        compare_selected_to_directory(
            io,
            &self.manager.live,
            &self.transition.affected_factions(),
            true,
            &self.saves_stage_live(),
        )?;
        compare_bank_selection_to_directory(io, &self.manager.banks, &self.banks_stage_live())
    }

    fn saves_stage_live(&self) -> PathBuf {
        self.paths.saves_staging.join("live")
    }

    fn banks_stage_live(&self) -> PathBuf {
        self.paths.banks_staging.join("live")
    }

    fn saves_backup_live(&self) -> PathBuf {
        self.paths.saves_backup.join("live")
    }

    fn banks_backup_live(&self) -> PathBuf {
        self.paths.banks_backup.join("live")
    }

    fn staged_root_update(&self, owner: &SaveOwner, faction: SlotId) -> PathBuf {
        owner
            .directory(&self.paths.saves_staging.join("set-updates"))
            .join("roots")
            .join(faction.as_str())
    }

    fn staged_global_saves(&self, owner: &SaveOwner) -> PathBuf {
        owner
            .directory(&self.paths.saves_staging.join("set-updates"))
            .join("global")
    }

    fn staged_global_banks(&self, owner: &SaveOwner) -> PathBuf {
        owner
            .directory(&self.paths.banks_staging.join("set-updates"))
            .join("global")
    }

    fn staged_set_fingerprints<I: SaveIo>(&self, io: &I) -> Result<Vec<LogicalSetFingerprint>> {
        let mut sets = Vec::new();
        for (owner, faction) in self.transition.root_updates() {
            let path = self.staged_root_update(&owner, faction);
            require_internal_directory_readonly(
                &self.manager.store_root,
                &path,
                "staged save archive",
            )?;
            sets.push(LogicalSetFingerprint {
                key: format!("roots/{}/{faction}", owner_key(&owner)),
                sha256: fingerprint_snapshot(&snapshot_tree(io, &path)?)?,
            });
        }
        let saves = self.staged_global_saves(&self.transition.previous_owner);
        require_internal_directory_readonly(
            &self.manager.store_root,
            &saves,
            "staged global Saves archive",
        )?;
        sets.push(LogicalSetFingerprint {
            key: format!(
                "global-saves/{}",
                owner_key(&self.transition.previous_owner)
            ),
            sha256: fingerprint_snapshot(&snapshot_tree(io, &saves)?)?,
        });
        let banks = self.staged_global_banks(&self.transition.previous_owner);
        require_internal_directory_readonly(
            &self.manager.store_root,
            &banks,
            "staged global Banks archive",
        )?;
        sets.push(LogicalSetFingerprint {
            key: format!(
                "global-banks/{}",
                owner_key(&self.transition.previous_owner)
            ),
            sha256: fingerprint_snapshot(&snapshot_tree(io, &banks)?)?,
        });
        sets.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(sets)
    }

    fn committed_set_fingerprints<I: SaveIo>(&self, io: &I) -> Result<Vec<LogicalSetFingerprint>> {
        let mut sets = Vec::new();
        for (owner, faction) in self.transition.root_updates() {
            let path = self.set_root(&owner, faction);
            require_internal_directory_readonly(
                &self.manager.store_root,
                &path,
                "committed save archive",
            )?;
            sets.push(LogicalSetFingerprint {
                key: format!("roots/{}/{faction}", owner_key(&owner)),
                sha256: fingerprint_snapshot(&snapshot_tree(io, &path)?)?,
            });
        }
        let saves = self.set_global_saves(&self.transition.previous_owner);
        require_internal_directory_readonly(
            &self.manager.store_root,
            &saves,
            "committed global Saves archive",
        )?;
        sets.push(LogicalSetFingerprint {
            key: format!(
                "global-saves/{}",
                owner_key(&self.transition.previous_owner)
            ),
            sha256: fingerprint_snapshot(&snapshot_tree(io, &saves)?)?,
        });
        let banks = self.set_global_banks(&self.transition.previous_owner);
        require_internal_directory_readonly(
            &self.manager.store_root,
            &banks,
            "committed global Banks archive",
        )?;
        sets.push(LogicalSetFingerprint {
            key: format!(
                "global-banks/{}",
                owner_key(&self.transition.previous_owner)
            ),
            sha256: fingerprint_snapshot(&snapshot_tree(io, &banks)?)?,
        });
        sets.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(sets)
    }

    fn verify_committed_set_updates<I: SaveIo>(&self, io: &I) -> Result<()> {
        let actual = fingerprint_logical_sets(&self.committed_set_fingerprints(io)?)?;
        if actual == self.recovery_proof()?.set_updates_sha256 {
            Ok(())
        } else {
            Err(user_path_err(
                "committed_save_archives_drifted",
                "archived save data changed before StarVault finished recovery",
                &self.manager.sets_root,
                false,
            ))
        }
    }

    fn set_root(&self, owner: &SaveOwner, faction: SlotId) -> PathBuf {
        owner
            .directory(&self.manager.sets_root)
            .join("roots")
            .join(faction.as_str())
    }

    fn set_global_saves(&self, owner: &SaveOwner) -> PathBuf {
        owner.directory(&self.manager.sets_root).join("global")
    }

    fn set_global_banks(&self, owner: &SaveOwner) -> PathBuf {
        owner
            .directory(&self.manager.sets_root)
            .join("global-banks")
    }

    fn ensure_operation_structure<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.manager.ensure_store_layout(io)?;
        for path in [
            &self.operation_root,
            &self.paths.saves_staging,
            &self.paths.saves_backup,
            &self.paths.banks_staging,
            &self.paths.banks_backup,
            &self.saves_stage_live(),
            &self.saves_backup_live(),
            &self.banks_stage_live(),
            &self.banks_backup_live(),
        ] {
            if !ensure_internal_directory(
                io,
                &self.manager.store_root,
                path,
                false,
                "save operation artifact",
            )? {
                return Err(user_path_err(
                    "missing_save_operation_artifact",
                    "save operation artifacts are incomplete",
                    path,
                    false,
                ));
            }
        }
        Ok(())
    }

    fn verify_operation_structure_readonly(&self) -> Result<()> {
        for path in [
            &self.operation_root,
            &self.paths.saves_staging,
            &self.paths.saves_backup,
            &self.paths.banks_staging,
            &self.paths.banks_backup,
            &self.saves_stage_live(),
            &self.saves_backup_live(),
            &self.banks_stage_live(),
            &self.banks_backup_live(),
        ] {
            require_internal_directory_readonly(
                &self.manager.store_root,
                path,
                "save operation artifact",
            )?;
        }
        Ok(())
    }

    fn marker_exists_readonly(&self, name: &str) -> Result<bool> {
        internal_file_exists_readonly(
            &self.manager.store_root,
            &self.operation_root.join(name),
            "save operation marker",
        )
    }

    fn receipt_exists_readonly(&self) -> Result<bool> {
        internal_file_exists_readonly(
            &self.manager.store_root,
            &self.receipt_path(),
            "save commit receipt",
        )
    }

    fn marker_exists<I: SaveIo>(&self, io: &I, name: &str) -> Result<bool> {
        internal_file_exists(
            io,
            &self.manager.store_root,
            &self.operation_root.join(name),
            "save operation marker",
        )
    }

    fn write_marker<I: SaveIo>(&self, io: &I, name: &str, bytes: &[u8]) -> Result<()> {
        write_internal_file(
            io,
            &self.manager.store_root,
            &self.operation_root.join(name),
            bytes,
            "save operation marker",
        )
    }

    fn remove_marker<I: SaveIo>(&self, io: &I, name: &str) -> Result<()> {
        remove_internal_file_if_exists(
            io,
            &self.manager.store_root,
            &self.operation_root.join(name),
            "save operation marker",
        )
    }

    fn remove_operation_directory<I: SaveIo>(&self, io: &I, path: &Path) -> Result<()> {
        if !ensure_internal_directory(
            io,
            &self.manager.store_root,
            path,
            false,
            "save operation artifact",
        )? {
            return Ok(());
        }
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            path,
            false,
            "save operation artifact",
        )?;
        remove_if_exists(io, path)
    }

    fn remove_operation_root<I: SaveIo>(&self, io: &I) -> Result<()> {
        self.manager.ensure_store_layout(io)?;
        if !ensure_internal_directory(
            io,
            &self.manager.store_root,
            &self.operation_root,
            false,
            "save operation",
        )? {
            return Ok(());
        }
        ensure_internal_directory(
            io,
            &self.manager.store_root,
            &self.operation_root,
            false,
            "save operation",
        )?;
        remove_if_exists(io, &self.operation_root)
    }
}

/// A retained, verified full-profile recovery backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryBackup {
    pub path: PathBuf,
    pub profile_id: ProfileId,
    pub created_at: u64,
}

/// Back up both complete Saves and Banks trees before enabling isolation or
/// changing the selected profile. Backups live outside app data indefinitely.
pub fn create_recovery_backup(
    documents: &Path,
    profile_id: &ProfileId,
    unix_timestamp: u64,
) -> Result<RecoveryBackup> {
    create_recovery_backup_with(documents, profile_id, unix_timestamp, &SystemSaveIo)
}

#[doc(hidden)]
pub fn create_recovery_backup_with<I: SaveIo>(
    documents: &Path,
    profile_id: &ProfileId,
    unix_timestamp: u64,
    io: &I,
) -> Result<RecoveryBackup> {
    let profile = resolve_profile(documents, profile_id)?;
    let recovery_root = documents.join("StarVault CCM Recovery");
    ensure_recovery_directory(io, documents, &recovery_root, true)?;
    let sequence = NEXT_BACKUP.fetch_add(1, Ordering::Relaxed);
    let unique = format!("{unix_timestamp}-{}-{sequence}", std::process::id());
    let staging = recovery_root.join(format!(".{unique}.staging"));
    let destination = recovery_root.join(unique);
    if ensure_recovery_directory(io, documents, &staging, false)?
        || ensure_recovery_directory(io, documents, &destination, false)?
    {
        return Err(user_path_err(
            "recovery_backup_collision",
            "a recovery backup path is already occupied",
            &recovery_root,
            false,
        ));
    }
    ensure_recovery_directory(io, documents, &staging, true)?;
    let result = (|| -> Result<()> {
        ensure_recovery_directory(io, documents, &staging, false)?;
        ensure_profile_directory(profile.saves_dir(), "selected Saves directory")?;
        copy_path_verified(io, profile.saves_dir(), &staging.join("Saves"))?;
        if ensure_optional_profile_directory(profile.banks_dir(), "selected Banks directory")? {
            copy_path_verified(io, profile.banks_dir(), &staging.join("Banks"))?;
        } else {
            create_dir_retry(io, &staging.join("Banks"))?;
        }
        ensure_recovery_directory(io, documents, &staging, false)?;
        ensure_recovery_directory(io, documents, &recovery_root, false)?;
        if ensure_recovery_directory(io, documents, &destination, false)? {
            return Err(user_path_err(
                "recovery_backup_collision",
                "a recovery backup path became occupied",
                &destination,
                false,
            ));
        }
        move_path(io, &staging, &destination)?;
        ensure_recovery_directory(io, documents, &destination, false)?;
        ensure_profile_directory(profile.saves_dir(), "selected Saves directory")?;
        verify_same(io, profile.saves_dir(), &destination.join("Saves"))?;
        if ensure_optional_profile_directory(profile.banks_dir(), "selected Banks directory")? {
            verify_same(io, profile.banks_dir(), &destination.join("Banks"))?;
        } else if std::fs::read_dir(destination.join("Banks"))?
            .next()
            .is_some()
        {
            return Err(internal_err(
                "recovery_backup_verification_failed",
                "StarVault could not verify the recovery backup",
                "missing source Banks directory did not produce an empty backup",
            ));
        }
        Ok(())
    })();
    if let Err(error) = result {
        let staging_cleanup = remove_recovery_directory_if_exists(io, documents, &staging);
        let destination_cleanup = remove_recovery_directory_if_exists(io, documents, &destination);
        return match (staging_cleanup, destination_cleanup) {
            (Ok(()), Ok(())) => Err(error),
            (left, right) => {
                let cleanup = left
                    .err()
                    .into_iter()
                    .chain(right.err())
                    .map(|failure| failure.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(internal_err(
                    "recovery_backup_cleanup_failed",
                    "StarVault could not clean up an incomplete recovery backup",
                    format!("backup failed: {error}; cleanup failed: {cleanup}"),
                ))
            }
        };
    }
    Ok(RecoveryBackup {
        path: destination,
        profile_id: profile.id,
        created_at: unix_timestamp,
    })
}

/// Minimal injection seam for deterministic cross-device and sharing-violation
/// tests. Production uses [`SystemSaveIo`].
#[doc(hidden)]
pub trait SaveIo {
    fn rename(&self, source: &Path, destination: &Path) -> std::io::Result<()>;
    fn copy_file(&self, source: &Path, destination: &Path) -> std::io::Result<u64>;
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
    fn remove_dir(&self, path: &Path) -> std::io::Result<()>;
    fn wait(&self, duration: Duration);

    fn create_dir(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }
}

#[derive(Debug, Clone, Copy, Default)]
#[doc(hidden)]
pub struct SystemSaveIo;

impl SaveIo for SystemSaveIo {
    fn rename(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        std::fs::rename(source, destination)
    }

    fn copy_file(&self, source: &Path, destination: &Path) -> std::io::Result<u64> {
        std::fs::copy(source, destination)
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_dir(path)
    }

    fn wait(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn ensure_store_root<I: SaveIo>(io: &I, store_root: &Path) -> Result<()> {
    match std::fs::symlink_metadata(store_root) {
        Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => Ok(()),
        Ok(_) => Err(unsafe_internal_path(store_root, "save store root")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_dir_all_retry(io, store_root)?;
            let metadata = std::fs::symlink_metadata(store_root)
                .map_err(|error| io_error("inspect_save_store", store_root, error))?;
            if metadata.is_dir() && !is_link_or_reparse(&metadata) {
                Ok(())
            } else {
                Err(unsafe_internal_path(store_root, "save store root"))
            }
        }
        Err(error) => Err(io_error("inspect_save_store", store_root, error)),
    }
}

fn verify_store_root_readonly(store_root: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(store_root) {
        Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => Ok(true),
        Ok(_) => Err(unsafe_internal_path(store_root, "save store root")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect_save_store", store_root, error)),
    }
}

fn verify_internal_directory_readonly(store_root: &Path, path: &Path, label: &str) -> Result<bool> {
    if !verify_store_root_readonly(store_root)? {
        return Ok(false);
    }
    let relative = path.strip_prefix(store_root).map_err(|error| {
        internal_err(
            "save_path_escape",
            "StarVault could not inspect its save data",
            error.to_string(),
        )
    })?;
    let mut current = store_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(unsafe_internal_path(path, label));
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => {}
            Ok(_) => return Err(unsafe_internal_path(&current, label)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("inspect_save_store", &current, error)),
        }
    }
    Ok(true)
}

fn require_internal_directory_readonly(store_root: &Path, path: &Path, label: &str) -> Result<()> {
    if verify_internal_directory_readonly(store_root, path, label)? {
        Ok(())
    } else {
        Err(user_path_err(
            "missing_save_operation_artifact",
            format!("{label} is missing"),
            path,
            false,
        ))
    }
}

fn internal_file_exists_readonly(store_root: &Path, path: &Path, label: &str) -> Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_internal_path(path, label))?;
    if !verify_internal_directory_readonly(store_root, parent, label)? {
        return Ok(false);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !is_link_or_reparse(&metadata) => {
            Ok(true)
        }
        Ok(_) => Err(unsafe_internal_path(path, label)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect_save_store_file", path, error)),
    }
}

fn ensure_internal_directory<I: SaveIo>(
    io: &I,
    store_root: &Path,
    path: &Path,
    create: bool,
    label: &str,
) -> Result<bool> {
    ensure_store_root(io, store_root)?;
    let relative = path.strip_prefix(store_root).map_err(|error| {
        internal_err(
            "save_path_escape",
            "StarVault could not inspect its save data",
            error.to_string(),
        )
    })?;
    let mut current = store_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(unsafe_internal_path(path, label));
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !is_link_or_reparse(&metadata) => {}
            Ok(_) => return Err(unsafe_internal_path(&current, label)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => {
                return Ok(false);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match retry_io(io, || io.create_dir(&current)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(io_error("create_save_store_directory", &current, error));
                    }
                }
                let metadata = std::fs::symlink_metadata(&current)
                    .map_err(|error| io_error("inspect_save_store", &current, error))?;
                if !metadata.is_dir() || is_link_or_reparse(&metadata) {
                    return Err(unsafe_internal_path(&current, label));
                }
            }
            Err(error) => return Err(io_error("inspect_save_store", &current, error)),
        }
    }
    Ok(true)
}

fn internal_file_exists<I: SaveIo>(
    io: &I,
    store_root: &Path,
    path: &Path,
    label: &str,
) -> Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_internal_path(path, label))?;
    if !ensure_internal_directory(io, store_root, parent, false, label)? {
        return Ok(false);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !is_link_or_reparse(&metadata) => {
            Ok(true)
        }
        Ok(_) => Err(unsafe_internal_path(path, label)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect_save_store_file", path, error)),
    }
}

fn write_internal_file<I: SaveIo>(
    io: &I,
    store_root: &Path,
    path: &Path,
    bytes: &[u8],
    label: &str,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_internal_path(path, label))?;
    ensure_internal_directory(io, store_root, parent, true, label)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !is_link_or_reparse(&metadata) => {}
        Ok(_) => return Err(unsafe_internal_path(path, label)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("inspect_save_store_file", path, error)),
    }
    ensure_internal_directory(io, store_root, parent, false, label)?;
    crate::atomic_file::write(path, bytes)?;
    if internal_file_exists(io, store_root, path, label)? {
        Ok(())
    } else {
        Err(internal_err(
            "save_artifact_missing_after_write",
            "StarVault could not write its save operation data",
            path.display().to_string(),
        ))
    }
}

fn remove_internal_file_if_exists<I: SaveIo>(
    io: &I,
    store_root: &Path,
    path: &Path,
    label: &str,
) -> Result<()> {
    if !internal_file_exists(io, store_root, path, label)? {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_internal_path(path, label))?;
    ensure_internal_directory(io, store_root, parent, false, label)?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect_save_store_file", path, error))?;
    if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
        return Err(unsafe_internal_path(path, label));
    }
    retry_io(io, || io.remove_file(path))
        .map_err(|error| io_error("remove_save_store_file", path, error))
}

fn remove_internal_directory_if_exists<I: SaveIo>(
    io: &I,
    store_root: &Path,
    path: &Path,
    label: &str,
) -> Result<()> {
    if !ensure_internal_directory(io, store_root, path, false, label)? {
        return Ok(());
    }
    ensure_internal_directory(io, store_root, path, false, label)?;
    remove_if_exists(io, path)
}

fn unsafe_internal_path(path: &Path, label: &str) -> crate::Error {
    user_path_err(
        "unsafe_store_path",
        format!("{label} must not be a link, reparse point, or non-directory ancestor"),
        path,
        false,
    )
}

#[cfg(windows)]
fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn validate_operation_id(operation_id: &str) -> Result<()> {
    if operation_id.is_empty()
        || operation_id.len() > 128
        || !operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(user_err(
            "invalid_operation_id",
            "operation id must contain 1 to 128 ASCII letters, digits, or dashes",
        ));
    }
    Ok(())
}

fn selected_save_entries(
    root: &Path,
    factions: &[SlotId],
    include_global: bool,
) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("read_live_saves", root, error)),
    };
    let prefixes: Vec<String> = factions
        .iter()
        .flat_map(|faction| save_prefixes(*faction))
        .map(|prefix| prefix.to_ascii_lowercase())
        .collect();
    let mut selected = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read_live_saves", root, error))?;
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let is_root_save =
            name.ends_with(".sc2save") && prefixes.iter().any(|prefix| name.starts_with(prefix));
        let is_global = include_global
            && SWEPT_DIRS
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate));
        if is_root_save || is_global {
            selected.push(entry.path());
        }
    }
    selected.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(selected)
}

fn selected_bank_entries(root: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("read_live_banks", root, error)),
    };
    let mut selected = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read_live_banks", root, error))?;
        let lower = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let vanilla = VANILLA_BANKS
            .iter()
            .any(|name| lower == format!("{name}.sc2bank"));
        if !vanilla {
            selected.push(entry.path());
        }
    }
    selected.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(selected)
}

fn copy_selected_saves<I: SaveIo>(
    io: &I,
    source_root: &Path,
    destination_root: &Path,
    factions: &[SlotId],
    include_global: bool,
) -> Result<()> {
    create_dir_all_retry(io, destination_root)?;
    for source in selected_save_entries(source_root, factions, include_global)? {
        let destination = destination_root.join(required_name(&source)?);
        copy_path_verified(io, &source, &destination)?;
    }
    Ok(())
}

fn copy_selected_banks<I: SaveIo>(
    io: &I,
    source_root: &Path,
    destination_root: &Path,
) -> Result<()> {
    create_dir_all_retry(io, destination_root)?;
    for source in selected_bank_entries(source_root)? {
        let destination = destination_root.join(required_name(&source)?);
        copy_path_verified(io, &source, &destination)?;
    }
    Ok(())
}

fn compare_selected_to_directory<I: SaveIo>(
    io: &I,
    live_root: &Path,
    factions: &[SlotId],
    include_global: bool,
    expected_root: &Path,
) -> Result<()> {
    let actual = snapshot_selection(
        io,
        live_root,
        selected_save_entries(live_root, factions, include_global)?,
    )?;
    let expected = snapshot_tree(io, expected_root)?;
    compare_snapshots(actual, expected, live_root)
}

fn compare_bank_selection_to_directory<I: SaveIo>(
    io: &I,
    live_root: &Path,
    expected_root: &Path,
) -> Result<()> {
    let actual = snapshot_selection(io, live_root, selected_bank_entries(live_root)?)?;
    let expected = snapshot_tree(io, expected_root)?;
    compare_snapshots(actual, expected, live_root)
}

fn copy_children_verified<I: SaveIo>(io: &I, source: &Path, destination: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("read_save_set", source, error)),
    };
    create_dir_all_retry(io, destination)?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read_save_set", source, error))?;
        copy_path_verified(io, &entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn replace_directory<I: SaveIo>(
    io: &I,
    store_root: &Path,
    source: &Path,
    destination: &Path,
    operation_id: &str,
) -> Result<()> {
    if !ensure_internal_directory(io, store_root, source, false, "staged save set")? {
        return Err(user_path_err(
            "missing_save_operation_artifact",
            "staged save set is missing",
            source,
            false,
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        user_path_err(
            "invalid_save_set_path",
            "save-set path has no parent directory",
            destination,
            false,
        )
    })?;
    ensure_internal_directory(io, store_root, parent, true, "save set parent")?;
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let temporary = parent.join(format!(".{name}.staging-{operation_id}"));
    let backup = parent.join(format!(".{name}.backup-{operation_id}"));
    let destination_exists =
        ensure_internal_directory(io, store_root, destination, false, "persistent save set")?;
    if destination_exists
        && snapshots_equal(snapshot_tree(io, source)?, snapshot_tree(io, destination)?)
    {
        remove_internal_directory_if_exists(io, store_root, &temporary, "temporary save set")?;
        return remove_internal_directory_if_exists(io, store_root, &backup, "backup save set");
    }
    remove_internal_directory_if_exists(io, store_root, &temporary, "temporary save set")?;
    copy_path_verified(io, source, &temporary)?;
    ensure_internal_directory(io, store_root, &temporary, false, "temporary save set")?;
    if destination_exists {
        remove_internal_directory_if_exists(io, store_root, &backup, "backup save set")?;
        ensure_internal_directory(io, store_root, destination, false, "persistent save set")?;
        move_path(io, destination, &backup)?;
    }
    ensure_internal_directory(io, store_root, &temporary, false, "temporary save set")?;
    ensure_internal_directory(io, store_root, parent, false, "save set parent")?;
    if let Err(error) = move_path(io, &temporary, destination) {
        let backup_exists =
            ensure_internal_directory(io, store_root, &backup, false, "backup save set")?;
        let destination_exists =
            ensure_internal_directory(io, store_root, destination, false, "persistent save set")?;
        if backup_exists && !destination_exists {
            if let Err(rollback) = move_path(io, &backup, destination) {
                return Err(cleanup_failure(
                    "save_set_rollback_failed",
                    "StarVault could not restore the previous save set",
                    &error,
                    &rollback,
                ));
            }
        }
        return Err(error);
    }
    if let Err(error) = verify_same(io, source, destination) {
        if ensure_internal_directory(io, store_root, &backup, false, "backup save set")? {
            remove_internal_directory_if_exists(
                io,
                store_root,
                destination,
                "persistent save set",
            )?;
            if let Err(rollback) = move_path(io, &backup, destination) {
                return Err(cleanup_failure(
                    "save_set_rollback_failed",
                    "StarVault could not restore the previous save set",
                    &error,
                    &rollback,
                ));
            }
        }
        return Err(error);
    }
    remove_internal_directory_if_exists(io, store_root, &backup, "backup save set")
}

fn move_path<I: SaveIo>(io: &I, source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        create_dir_all_retry(io, parent)?;
    }
    match retry_io(io, || io.rename(source, destination)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_path_verified(io, source, destination)?;
            if let Err(remove_error) = remove_if_exists(io, source) {
                return match remove_if_exists(io, destination) {
                    Ok(()) => Err(remove_error),
                    Err(cleanup) => Err(cleanup_failure(
                        "cross_device_move_cleanup_failed",
                        "StarVault could not clean up an incomplete cross-volume save move",
                        &remove_error,
                        &cleanup,
                    )),
                };
            }
            Ok(())
        }
        Err(error) => Err(io_error("move_save_entry", source, error)),
    }
}

fn copy_path_verified<I: SaveIo>(io: &I, source: &Path, destination: &Path) -> Result<()> {
    remove_if_exists(io, destination)?;
    let copied = copy_path(io, source, destination);
    if let Err(error) = copied {
        return match remove_if_exists(io, destination) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup_failure(
                "save_copy_cleanup_failed",
                "StarVault could not clean up an incomplete save copy",
                &error,
                &cleanup,
            )),
        };
    }
    if let Err(error) = verify_same(io, source, destination) {
        return match remove_if_exists(io, destination) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup_failure(
                "save_copy_cleanup_failed",
                "StarVault could not clean up an unverified save copy",
                &error,
                &cleanup,
            )),
        };
    }
    Ok(())
}

fn copy_path<I: SaveIo>(io: &I, source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| io_error("inspect_save_entry", source, error))?;
    if let Some(parent) = destination.parent() {
        create_dir_all_retry(io, parent)?;
    }
    let file_type = metadata.file_type();
    if is_link_or_reparse(&metadata) {
        return copy_link(source, destination, &file_type);
    }
    if file_type.is_dir() {
        create_dir_retry(io, destination)?;
        let mut entries = std::fs::read_dir(source)
            .map_err(|error| io_error("read_save_entry", source, error))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| io_error("read_save_entry", source, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            copy_path(io, &entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if file_type.is_file() {
        retry_io(io, || io.copy_file(source, destination))
            .map(|_| ())
            .map_err(|error| io_error("copy_save_entry", source, error))?;
        return Ok(());
    }
    Err(user_path_err(
        "unsupported_save_entry",
        "save data contains an unsupported filesystem entry",
        source,
        false,
    ))
}

#[cfg(unix)]
fn copy_link(source: &Path, destination: &Path, _file_type: &std::fs::FileType) -> Result<()> {
    use std::os::unix::fs::symlink;

    let target =
        std::fs::read_link(source).map_err(|error| io_error("read_save_link", source, error))?;
    symlink(target, destination).map_err(|error| io_error("copy_save_link", source, error))
}

#[cfg(windows)]
fn copy_link(source: &Path, destination: &Path, file_type: &std::fs::FileType) -> Result<()> {
    use std::os::windows::fs::{symlink_file, FileTypeExt};

    let target =
        std::fs::read_link(source).map_err(|error| io_error("read_save_link", source, error))?;
    let result = if file_type.is_symlink_dir() {
        junction::create(target, destination)
    } else if file_type.is_symlink_file() {
        symlink_file(target, destination)
    } else {
        Err(std::io::Error::other("unknown Windows reparse-point kind"))
    };
    result.map_err(|error| io_error("copy_save_link", source, error))
}

#[cfg(not(any(unix, windows)))]
fn copy_link(source: &Path, _destination: &Path, _file_type: &std::fs::FileType) -> Result<()> {
    Err(user_path_err(
        "unsupported_save_link",
        "save links are not supported on this platform",
        source,
        false,
    ))
}

fn remove_if_exists<I: SaveIo>(io: &I, path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("inspect_save_entry", path, error)),
    };
    let file_type = metadata.file_type();
    if is_link_or_reparse(&metadata) {
        return remove_link(io, path, &metadata);
    }
    if file_type.is_dir() {
        let entries = std::fs::read_dir(path)
            .map_err(|error| io_error("read_save_entry", path, error))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| io_error("read_save_entry", path, error))?;
        for entry in entries {
            remove_if_exists(io, &entry.path())?;
        }
        return retry_io(io, || io.remove_dir(path))
            .map_err(|error| io_error("remove_save_directory", path, error));
    }
    retry_io(io, || io.remove_file(path)).map_err(|error| io_error("remove_save_file", path, error))
}

#[cfg(unix)]
fn remove_link<I: SaveIo>(io: &I, path: &Path, _metadata: &std::fs::Metadata) -> Result<()> {
    retry_io(io, || io.remove_file(path)).map_err(|error| io_error("remove_save_link", path, error))
}

#[cfg(windows)]
fn remove_link<I: SaveIo>(io: &I, path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let result = if metadata.file_type().is_symlink_dir() || metadata.is_dir() {
        retry_io(io, || io.remove_dir(path))
    } else {
        retry_io(io, || io.remove_file(path))
    };
    result.map_err(|error| io_error("remove_save_link", path, error))
}

#[cfg(not(any(unix, windows)))]
fn remove_link<I: SaveIo>(io: &I, path: &Path, _metadata: &std::fs::Metadata) -> Result<()> {
    retry_io(io, || io.remove_file(path)).map_err(|error| io_error("remove_save_link", path, error))
}

fn create_dir_retry<I: SaveIo>(io: &I, path: &Path) -> Result<()> {
    retry_io(io, || io.create_dir(path))
        .map_err(|error| io_error("create_save_directory", path, error))
}

fn create_dir_all_retry<I: SaveIo>(io: &I, path: &Path) -> Result<()> {
    retry_io(io, || io.create_dir_all(path))
        .map_err(|error| io_error("create_save_directory", path, error))
}

fn retry_io<I: SaveIo, T>(
    io: &I,
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut delay = Duration::from_millis(25);
    for attempt in 0..RETRY_ATTEMPTS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_sharing_violation(&error) && attempt + 1 < RETRY_ATTEMPTS => {
                io.wait(delay);
                delay = (delay * 2).min(Duration::from_millis(800));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("retry loop always returns")
}

fn is_sharing_violation(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
        || matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
        )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum SnapshotKind {
    Directory,
    File { size: u64, sha256: String },
    Link { target: PathBuf, directory: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct SnapshotEntry {
    relative: PathBuf,
    kind: SnapshotKind,
}

fn snapshot_tree<I: SaveIo>(io: &I, root: &Path) -> Result<Vec<SnapshotEntry>> {
    match root.symlink_metadata() {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("inspect_save_entry", root, error)),
    }
    let mut snapshot = Vec::new();
    snapshot_path(io, root, Path::new(""), &mut snapshot)?;
    if snapshot.first().is_some_and(|entry| {
        entry.relative.as_os_str().is_empty() && entry.kind == SnapshotKind::Directory
    }) {
        snapshot.remove(0);
    }
    snapshot.sort();
    Ok(snapshot)
}

fn snapshot_selection<I: SaveIo>(
    io: &I,
    root: &Path,
    paths: Vec<PathBuf>,
) -> Result<Vec<SnapshotEntry>> {
    let mut snapshot = Vec::new();
    for path in paths {
        let relative = path.strip_prefix(root).map_err(|error| {
            internal_err(
                "save_path_escape",
                "StarVault could not inspect save data",
                error.to_string(),
            )
        })?;
        snapshot_path(io, &path, relative, &mut snapshot)?;
    }
    snapshot.sort();
    Ok(snapshot)
}

fn snapshot_path<I: SaveIo>(
    io: &I,
    path: &Path,
    relative: &Path,
    snapshot: &mut Vec<SnapshotEntry>,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect_save_entry", path, error))?;
    let file_type = metadata.file_type();
    if is_link_or_reparse(&metadata) {
        let target =
            std::fs::read_link(path).map_err(|error| io_error("read_save_link", path, error))?;
        snapshot.push(SnapshotEntry {
            relative: relative.to_path_buf(),
            kind: SnapshotKind::Link {
                target,
                directory: link_is_directory(&file_type),
            },
        });
        return Ok(());
    }
    if file_type.is_dir() {
        snapshot.push(SnapshotEntry {
            relative: relative.to_path_buf(),
            kind: SnapshotKind::Directory,
        });
        let mut entries = std::fs::read_dir(path)
            .map_err(|error| io_error("read_save_entry", path, error))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| io_error("read_save_entry", path, error))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            snapshot_path(
                io,
                &entry.path(),
                &relative.join(entry.file_name()),
                snapshot,
            )?;
        }
        return Ok(());
    }
    if file_type.is_file() {
        snapshot.push(SnapshotEntry {
            relative: relative.to_path_buf(),
            kind: SnapshotKind::File {
                size: metadata.len(),
                sha256: hash_file_retry(io, path)?,
            },
        });
        return Ok(());
    }
    Err(user_path_err(
        "unsupported_save_entry",
        "save data contains an unsupported filesystem entry",
        path,
        false,
    ))
}

#[cfg(windows)]
fn link_is_directory(file_type: &std::fs::FileType) -> bool {
    use std::os::windows::fs::FileTypeExt;
    file_type.is_symlink_dir()
}

#[cfg(not(windows))]
fn link_is_directory(_file_type: &std::fs::FileType) -> bool {
    false
}

fn hash_file_retry<I: SaveIo>(io: &I, path: &Path) -> Result<String> {
    retry_io(io, || {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .map_err(|error| io_error("hash_save_entry", path, error))
}

fn verify_same<I: SaveIo>(io: &I, source: &Path, destination: &Path) -> Result<()> {
    compare_snapshots(
        snapshot_tree(io, source)?,
        snapshot_tree(io, destination)?,
        destination,
    )
}

fn snapshots_equal(left: Vec<SnapshotEntry>, right: Vec<SnapshotEntry>) -> bool {
    left == right
}

fn fingerprint_snapshot(snapshot: &[SnapshotEntry]) -> Result<String> {
    fingerprint_serialized(snapshot, "serialize_save_snapshot")
}

fn fingerprint_logical_sets(sets: &[LogicalSetFingerprint]) -> Result<String> {
    fingerprint_serialized(sets, "serialize_save_set_fingerprints")
}

fn fingerprint_serialized<T: Serialize + ?Sized>(value: &T, code: &str) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        internal_err(
            code,
            "StarVault could not verify save data",
            error.to_string(),
        )
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn recovery_identity_sha256(operation_id: &str, transition: &SaveTransition) -> Result<String> {
    fingerprint_serialized(
        &RecoveryProofIdentity {
            version: RECOVERY_PROOF_VERSION,
            operation_id,
            transition,
        },
        "serialize_save_transition_identity",
    )
}

fn owner_key(owner: &SaveOwner) -> String {
    match owner {
        SaveOwner::Plain => "plain".into(),
        SaveOwner::Package(id) => format!("packages/{}", id.as_str()),
    }
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn verify_snapshot_fingerprint(
    snapshot: Vec<SnapshotEntry>,
    expected: &str,
    path: &Path,
    label: &str,
) -> Result<()> {
    if fingerprint_snapshot(&snapshot)? == expected {
        Ok(())
    } else {
        Err(user_path_err(
            "save_recovery_proof_mismatch",
            format!("{label} no longer matches the operation journal"),
            path,
            false,
        ))
    }
}

fn verify_snapshot_entries_known(
    actual: Vec<SnapshotEntry>,
    previous: Vec<SnapshotEntry>,
    target: Vec<SnapshotEntry>,
    path: &Path,
) -> Result<()> {
    if actual
        .iter()
        .all(|entry| previous.binary_search(entry).is_ok() || target.binary_search(entry).is_ok())
    {
        Ok(())
    } else {
        Err(user_path_err(
            "unrecognized_live_save_state",
            "live save data contains changes that are not part of the pending operation",
            path,
            false,
        ))
    }
}

fn compare_snapshots(
    left: Vec<SnapshotEntry>,
    right: Vec<SnapshotEntry>,
    path: &Path,
) -> Result<()> {
    if snapshots_equal(left, right) {
        Ok(())
    } else {
        Err(user_path_err(
            "save_verification_failed",
            "save data changed while StarVault was preparing the transition",
            path,
            true,
        ))
    }
}

fn required_name(path: &Path) -> Result<OsString> {
    path.file_name().map(OsString::from).ok_or_else(|| {
        user_path_err(
            "invalid_save_path",
            "save entry has no file name",
            path,
            false,
        )
    })
}

fn io_error(code: &str, path: &Path, error: std::io::Error) -> crate::Error {
    user_path_err(code, error.to_string(), path, is_sharing_violation(&error))
}

fn cleanup_failure(
    code: &str,
    message: &str,
    primary: &crate::Error,
    cleanup: &crate::Error,
) -> crate::Error {
    internal_err(
        code,
        message,
        format!("operation failed: {primary}; cleanup failed: {cleanup}"),
    )
}
