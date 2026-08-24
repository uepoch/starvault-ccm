//! Durable journal for campaign mutations.

use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::contracts::ActiveCampaign;
use crate::error::{package_err, Result};
use crate::filesystem::is_link_or_reparse;
use crate::layout::SlotId;

pub const JOURNAL_VERSION: u32 = 7;
pub const JOURNAL_FILE: &str = "pending-operation.json";
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;

#[cfg(windows)]
type OpenIdentity = File;

#[cfg(not(windows))]
struct OpenIdentity;

struct OpenedJournal {
    file: File,
    metadata: Metadata,
    identity: OpenIdentity,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Activate,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Last completed filesystem checkpoint. The durable `Preparing`/`Prepared`
/// record is written before any live resource changes; later values are
/// persisted after each completed swap. Recovery also reads the ledger because
/// a process can stop after the SQLite commit but before the final rewrite.
pub enum OperationPhase {
    /// The journal owns deterministic staging and backup paths, but no live
    /// game resource has been changed yet.
    Preparing,
    Prepared,
    SavesSwapped,
    SlotsSwapped,
    ModsSwapped,
    LedgerCommitted,
    /// The previous live state has been restored and verified. Only owned
    /// staging/backup cleanup remains, so recovery no longer needs sidecars.
    RollbackVerified,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationPaths {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saves_staging: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saves_backup: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banks_staging: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banks_backup: Option<PathBuf>,
    /// Fixed-size proof of every immutable save artifact prepared before the
    /// live profile changes. The atomic journal owns this proof so a replaced
    /// backup, target staging tree, or archived save set cannot become trusted
    /// merely by changing an operation sidecar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_recovery_proof: Option<SaveRecoveryProof>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<SlotOperationJournal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mods_staging: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mods_backup: Option<PathBuf>,
    /// SHA-256 of the exact serialized Mods rollback plan. The plan lives in
    /// the backup directory, but its digest lives in the independently atomic
    /// operation journal so recovery cannot accept an unrelated valid plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mods_plan_sha256: Option<String>,
}

/// Journal-bound fingerprints for one prepared save transition.
///
/// Each fingerprint is lowercase SHA-256. `set_updates_sha256` covers a
/// sorted list of logical owner/faction save sets rather than filesystem
/// staging paths, which lets recovery verify the same data after commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveRecoveryProof {
    pub version: u32,
    pub operation_id: String,
    pub transition_sha256: String,
    pub previous_saves_sha256: String,
    pub previous_banks_sha256: String,
    pub target_saves_sha256: String,
    pub target_banks_sha256: String,
    pub set_updates_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotOperationPaths {
    pub faction: SlotId,
    pub live: PathBuf,
    pub staging: PathBuf,
    pub backup: PathBuf,
}

/// Campaign-root paths plus the actual previous and target object identities
/// captured before the journal advances to `prepared`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotOperationJournal {
    pub paths: SlotOperationPaths,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_state: Option<SlotStateBinding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_state: Option<SlotStateBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SlotStateKind {
    Absent,
    Directory,
    Junction,
    SharedDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SlotStateBinding {
    pub faction: SlotId,
    pub kind: SlotStateKind,
    pub sha256: String,
}

impl SlotOperationJournal {
    pub(crate) fn new(
        paths: SlotOperationPaths,
        previous_state: Option<SlotStateBinding>,
        target_state: Option<SlotStateBinding>,
    ) -> Self {
        Self {
            paths,
            previous_state,
            target_state,
        }
    }

    pub(crate) fn previous_state(&self) -> Option<&SlotStateBinding> {
        self.previous_state.as_ref()
    }

    pub(crate) fn target_state(&self) -> Option<&SlotStateBinding> {
        self.target_state.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingOperation {
    pub version: u32,
    pub operation_id: String,
    pub kind: OperationKind,
    pub phase: OperationPhase,
    pub previous_campaign: Option<ActiveCampaign>,
    pub target_campaign: Option<ActiveCampaign>,
    /// Redundant with the four save paths by design. Recovery rejects a
    /// journal where the flag and paths disagree instead of silently treating
    /// a damaged isolation operation as isolation-off.
    pub saves_participated: bool,
    pub paths: OperationPaths,
}

impl PendingOperation {
    pub fn new_preparing(
        operation_id: String,
        kind: OperationKind,
        previous_campaign: Option<ActiveCampaign>,
        target_campaign: Option<ActiveCampaign>,
        paths: OperationPaths,
    ) -> Self {
        Self::with_phase(
            operation_id,
            kind,
            OperationPhase::Preparing,
            previous_campaign,
            target_campaign,
            paths,
        )
    }

    pub fn new(
        operation_id: String,
        kind: OperationKind,
        previous_campaign: Option<ActiveCampaign>,
        target_campaign: Option<ActiveCampaign>,
        paths: OperationPaths,
    ) -> Self {
        Self::with_phase(
            operation_id,
            kind,
            OperationPhase::Prepared,
            previous_campaign,
            target_campaign,
            paths,
        )
    }

    fn with_phase(
        operation_id: String,
        kind: OperationKind,
        phase: OperationPhase,
        previous_campaign: Option<ActiveCampaign>,
        target_campaign: Option<ActiveCampaign>,
        paths: OperationPaths,
    ) -> Self {
        let saves_participated = paths.saves_staging.is_some()
            && paths.saves_backup.is_some()
            && paths.banks_staging.is_some()
            && paths.banks_backup.is_some();
        Self {
            version: JOURNAL_VERSION,
            operation_id,
            kind,
            phase,
            previous_campaign,
            target_campaign,
            saves_participated,
            paths,
        }
    }

    pub fn path(store_root: &Path) -> PathBuf {
        store_root.join(JOURNAL_FILE)
    }

    pub fn load(store_root: &Path) -> Result<Option<Self>> {
        let path = Self::path(store_root);
        let Some(opened) = open_verified_journal(&path)? else {
            return Ok(None);
        };
        parse_journal(&opened.bytes).map(Some)
    }

    pub fn persist(&self, store_root: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            crate::error::internal_err(
                "serialize_operation_journal",
                "StarVault could not prepare the operation",
                error.to_string(),
            )
        })?;
        crate::atomic_file::write(&Self::path(store_root), &bytes)
    }

    pub fn advance(&mut self, store_root: &Path, phase: OperationPhase) -> Result<()> {
        if phase < self.phase {
            return Err(crate::error::internal_err(
                "journal_phase_regression",
                "StarVault could not complete the operation",
                format!("cannot move journal from {:?} to {phase:?}", self.phase),
            ));
        }
        self.phase = phase;
        self.persist(store_root)
    }

    pub fn remove_expected(store_root: &Path, expected: &Self) -> Result<()> {
        let path = Self::path(store_root);
        let Some(opened) = open_verified_journal(&path)? else {
            return Ok(());
        };
        let actual = parse_journal(&opened.bytes)?;
        if &actual != expected {
            return Err(package_err(
                "unsafe_operation_journal",
                "operation journal changed before cleanup",
            ));
        }
        if !opened_file_is_current(
            &path,
            None,
            &opened.identity,
            &opened.file,
            &opened.metadata,
        )? {
            return Err(package_err(
                "unsafe_operation_journal",
                "operation journal changed before cleanup",
            ));
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn parse_journal(bytes: &[u8]) -> Result<PendingOperation> {
    let operation: PendingOperation = serde_json::from_slice(bytes)
        .map_err(|error| package_err("corrupt_operation_journal", error.to_string()))?;
    if operation.version != JOURNAL_VERSION {
        return Err(package_err(
            "unsupported_operation_journal",
            format!(
                "unsupported operation journal version {}",
                operation.version
            ),
        ));
    }
    Ok(operation)
}

fn open_verified_journal(path: &Path) -> Result<Option<OpenedJournal>> {
    let expected = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_journal_metadata(&expected)?;
    let identity = capture_open_identity(path)?;
    let mut file = OpenOptions::new().read(true).open(path).map_err(|error| {
        crate::error::user_path_err("read_operation_journal", error.to_string(), path, true)
    })?;
    let opened = file.metadata().map_err(|error| {
        crate::error::user_path_err("inspect_operation_journal", error.to_string(), path, true)
    })?;
    validate_journal_metadata(&opened)?;
    if !opened_file_is_current(path, Some(&expected), &identity, &file, &opened)? {
        return Err(package_err(
            "unsafe_operation_journal",
            "operation journal changed while it was being opened",
        ));
    }
    let mut bytes = Vec::with_capacity(opened.len().min(MAX_JOURNAL_BYTES).try_into().unwrap_or(0));
    file.by_ref()
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            crate::error::user_path_err("read_operation_journal", error.to_string(), path, true)
        })?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(package_err(
            "unsafe_operation_journal",
            "operation journal exceeds the 1 MiB safety limit",
        ));
    }
    if !opened_file_is_current(path, None, &identity, &file, &opened)? {
        return Err(package_err(
            "unsafe_operation_journal",
            "operation journal changed while it was being read",
        ));
    }
    Ok(Some(OpenedJournal {
        file,
        metadata: opened,
        identity,
        bytes,
    }))
}

fn validate_journal_metadata(metadata: &Metadata) -> Result<()> {
    if !metadata.is_file() || is_link_or_reparse(metadata) {
        return Err(package_err(
            "unsafe_operation_journal",
            "operation journal must be a regular file",
        ));
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(package_err(
            "unsafe_operation_journal",
            "operation journal exceeds the 1 MiB safety limit",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn opened_file_is_current(
    path: &Path,
    initial: Option<&Metadata>,
    _identity: &OpenIdentity,
    _file: &File,
    opened: &Metadata,
) -> Result<bool> {
    let current = std::fs::symlink_metadata(path).map_err(|error| {
        crate::error::user_path_err("inspect_operation_journal", error.to_string(), path, true)
    })?;
    Ok(crate::filesystem::same_file(&current, opened)
        && initial.is_none_or(|initial| crate::filesystem::same_file(initial, opened)))
}

#[cfg(windows)]
fn opened_file_is_current(
    path: &Path,
    _initial: Option<&Metadata>,
    identity: &OpenIdentity,
    file: &File,
    _opened: &Metadata,
) -> Result<bool> {
    let current_metadata = std::fs::symlink_metadata(path).map_err(|error| {
        crate::error::user_path_err("inspect_operation_journal", error.to_string(), path, true)
    })?;
    validate_journal_metadata(&current_metadata)?;
    let current = open_identity_file(path)?;
    validate_identity_handle(path, &current)?;
    let current_metadata = std::fs::symlink_metadata(path).map_err(|error| {
        crate::error::user_path_err("inspect_operation_journal", error.to_string(), path, true)
    })?;
    validate_journal_metadata(&current_metadata)?;
    Ok(
        windows_file_identity(identity, path)? == windows_file_identity(file, path)?
            && windows_file_identity(file, path)? == windows_file_identity(&current, path)?,
    )
}

#[cfg(not(any(unix, windows)))]
fn opened_file_is_current(
    _path: &Path,
    _initial: Option<&Metadata>,
    _identity: &OpenIdentity,
    _file: &File,
    _opened: &Metadata,
) -> Result<bool> {
    Ok(false)
}

#[cfg(windows)]
fn capture_open_identity(path: &Path) -> Result<OpenIdentity> {
    let identity = open_identity_file(path)?;
    validate_identity_handle(path, &identity)?;
    let current = std::fs::symlink_metadata(path).map_err(|error| {
        crate::error::user_path_err("inspect_operation_journal", error.to_string(), path, true)
    })?;
    validate_journal_metadata(&current)?;
    Ok(identity)
}

#[cfg(not(windows))]
fn capture_open_identity(_path: &Path) -> Result<OpenIdentity> {
    Ok(OpenIdentity)
}

#[cfg(windows)]
fn open_identity_file(path: &Path) -> Result<File> {
    crate::filesystem::open_reparse_point(path).map_err(|error| {
        crate::error::user_path_err("read_operation_journal", error.to_string(), path, true)
    })
}

#[cfg(windows)]
fn validate_identity_handle(path: &Path, file: &File) -> Result<()> {
    let metadata = file.metadata().map_err(|error| {
        crate::error::user_path_err("inspect_operation_journal", error.to_string(), path, true)
    })?;
    validate_journal_metadata(&metadata)
}

#[cfg(windows)]
fn windows_file_identity(file: &File, path: &Path) -> Result<(u32, u64)> {
    crate::filesystem::file_identity(file).map_err(|error| {
        crate::error::user_path_err("inspect_operation_journal", error.to_string(), path, true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::PackageId;
    use crate::layout::SlotId;

    #[test]
    fn journal_round_trips_every_phase_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let target = ActiveCampaign {
            id: PackageId::parse("raynor-rogue").unwrap(),
            revision: "abc".into(),
            faction: SlotId::LotV,
        };
        let mut journal = PendingOperation::new_preparing(
            "operation-1".into(),
            OperationKind::Activate,
            None,
            Some(target),
            OperationPaths::default(),
        );
        journal.persist(directory.path()).unwrap();
        for phase in [
            OperationPhase::Prepared,
            OperationPhase::SavesSwapped,
            OperationPhase::SlotsSwapped,
            OperationPhase::ModsSwapped,
            OperationPhase::LedgerCommitted,
            OperationPhase::RollbackVerified,
        ] {
            journal.advance(directory.path(), phase).unwrap();
            assert_eq!(
                PendingOperation::load(directory.path())
                    .unwrap()
                    .unwrap()
                    .phase,
                phase
            );
        }
        PendingOperation::remove_expected(directory.path(), &journal).unwrap();
        assert!(PendingOperation::load(directory.path()).unwrap().is_none());
    }

    #[test]
    fn rejects_unknown_journal_versions() {
        let directory = tempfile::tempdir().unwrap();
        let path = PendingOperation::path(directory.path());
        std::fs::write(
            path,
            r#"{"version":99,"operation_id":"x","kind":"activate","phase":"prepared","previous_campaign":null,"target_campaign":null,"paths":{}}"#,
        )
        .unwrap();
        assert!(PendingOperation::load(directory.path()).is_err());
    }

    #[test]
    fn slot_state_identities_round_trip_in_the_atomic_journal() {
        let directory = tempfile::tempdir().unwrap();
        let slot = SlotOperationPaths {
            faction: SlotId::LotV,
            live: directory.path().join("void"),
            staging: directory.path().join("void.staging-operation-1"),
            backup: directory.path().join("void.backup-operation-1"),
        };
        let paths = OperationPaths {
            slot: Some(SlotOperationJournal::new(
                slot,
                Some(SlotStateBinding {
                    faction: SlotId::LotV,
                    kind: SlotStateKind::Directory,
                    sha256: "a".repeat(64),
                }),
                Some(SlotStateBinding {
                    faction: SlotId::LotV,
                    kind: SlotStateKind::Junction,
                    sha256: "b".repeat(64),
                }),
            )),
            ..OperationPaths::default()
        };
        let journal = PendingOperation::new(
            "operation-1".into(),
            OperationKind::Activate,
            None,
            None,
            paths,
        );

        journal.persist(directory.path()).unwrap();
        assert_eq!(
            PendingOperation::load(directory.path()).unwrap(),
            Some(journal)
        );
        let serialized = std::fs::read_to_string(PendingOperation::path(directory.path())).unwrap();
        assert!(serialized.contains("previous_state"));
        assert!(serialized.contains("target_state"));
        assert!(serialized.contains(&"a".repeat(64)));
        assert!(serialized.contains(&"b".repeat(64)));
    }

    #[cfg(unix)]
    #[test]
    fn linked_journal_is_rejected_without_reading_or_removing_its_target() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let external = directory.path().join("external.json");
        std::fs::write(&external, b"private data").unwrap();
        symlink(&external, PendingOperation::path(directory.path())).unwrap();

        let error = PendingOperation::load(directory.path()).unwrap_err();
        assert_eq!(error.code(), "unsafe_operation_journal");
        std::fs::remove_file(PendingOperation::path(directory.path())).unwrap();
        assert_eq!(std::fs::read(&external).unwrap(), b"private data");
    }

    #[test]
    fn oversized_journal_is_rejected_before_parsing() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            PendingOperation::path(directory.path()),
            vec![b' '; MAX_JOURNAL_BYTES as usize + 1],
        )
        .unwrap();

        let error = PendingOperation::load(directory.path()).unwrap_err();
        assert_eq!(error.code(), "unsafe_operation_journal");
    }

    #[test]
    fn cleanup_requires_the_full_expected_journal_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let journal = PendingOperation::new_preparing(
            "operation-1".into(),
            OperationKind::Activate,
            None,
            None,
            OperationPaths::default(),
        );
        journal.persist(directory.path()).unwrap();

        let mut substituted_expectation = journal.clone();
        substituted_expectation.operation_id = "operation-2".into();
        let error = PendingOperation::remove_expected(directory.path(), &substituted_expectation)
            .unwrap_err();
        assert_eq!(error.code(), "unsafe_operation_journal");
        assert_eq!(
            PendingOperation::load(directory.path()).unwrap(),
            Some(journal.clone())
        );

        PendingOperation::remove_expected(directory.path(), &journal).unwrap();
        PendingOperation::remove_expected(directory.path(), &journal).unwrap();
        assert!(PendingOperation::load(directory.path()).unwrap().is_none());
    }

    #[test]
    fn unknown_journal_fields_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let journal = PendingOperation::new_preparing(
            "operation-1".into(),
            OperationKind::Activate,
            None,
            None,
            OperationPaths::default(),
        );
        let mut value = serde_json::to_value(journal).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        std::fs::write(
            PendingOperation::path(directory.path()),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();

        let error = PendingOperation::load(directory.path()).unwrap_err();
        assert_eq!(error.code(), "corrupt_operation_journal");
    }
}
