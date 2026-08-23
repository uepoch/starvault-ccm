use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use svccm_core::config::StrategyChoice;
use svccm_core::contracts::HealthState;
use svccm_core::error::user_err;
use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::operation::{OperationPhase, PendingOperation};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::Store;
use svccm_core::workflow::{FailurePoint, Workflow};

struct Fixture {
    _temp: tempfile::TempDir,
    layout: WindowsLayout,
    store: Store,
    first: PackageId,
    second: PackageId,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temp.path().join("sc2"));
        std::fs::create_dir_all(layout.root()).unwrap();
        std::fs::write(layout.exe(), b"fake executable").unwrap();
        let store = Store::open_for_tests(temp.path().join("store")).unwrap();
        let first = ingest(
            &store,
            &temp.path().join("first-source"),
            "first",
            SlotId::LotV,
            b"first",
        );
        let second = ingest(
            &store,
            &temp.path().join("second-source"),
            "second",
            SlotId::Nco,
            b"second",
        );
        Self {
            _temp: temp,
            layout,
            store,
            first,
            second,
        }
    }

    fn workflow(&self) -> Workflow<'_> {
        Workflow::new(&self.layout, &self.store)
            .with_strategy(Some(StrategyChoice::Copy))
            .with_running_probe(|| false)
    }
}

fn ingest(store: &Store, source: &Path, id: &str, faction: SlotId, payload: &[u8]) -> PackageId {
    let map = source.join(format!("Maps/campaign/{id}.SC2Map"));
    std::fs::create_dir_all(&map).unwrap();
    std::fs::write(map.join("payload"), payload).unwrap();
    std::fs::create_dir_all(source.join("Mods")).unwrap();
    std::fs::write(source.join(format!("Mods/{id}.SC2Mod")), payload).unwrap();
    let package_id = PackageId::parse(id).unwrap();
    let plan = plan_from_extracted(source).unwrap();
    store.ingest(&package_id, faction, &plan).unwrap();
    package_id
}

fn remove_first_package_blob(store: &Store, id: &PackageId) {
    let manifest = store.load_manifest(id).unwrap();
    let hash = &manifest.files[0].sha256;
    std::fs::remove_file(store.root().join("blobs").join(&hash[..2]).join(hash)).unwrap();
}

#[cfg(any(unix, windows))]
fn mods_temporary_path(backup: &Path, relative: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};

    let normalized = relative.replace('\\', "/").to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    backup.join(format!(".mods-copy-{}.partial", hex::encode(digest)))
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) {
    junction::create(target, link).unwrap();
}

#[cfg(unix)]
fn remove_directory_link(link: &Path) {
    std::fs::remove_file(link).unwrap();
}

#[cfg(windows)]
fn remove_directory_link(link: &Path) {
    std::fs::remove_dir(link).unwrap();
}

#[test]
fn activate_switch_across_factions_and_restore_are_singleton_transitions() {
    let fixture = Fixture::new();
    let first = fixture.workflow().activate(&fixture.first).unwrap();
    assert_eq!(first.faction, SlotId::LotV);
    assert!(fixture
        .layout
        .slot_dir(SlotId::LotV)
        .join("first.SC2Map/payload")
        .is_file());

    let second = fixture.workflow().activate(&fixture.second).unwrap();
    assert_eq!(second.faction, SlotId::Nco);
    assert_eq!(fixture.store.active_campaign().unwrap(), Some(second));
    assert!(std::fs::read_dir(fixture.layout.slot_dir(SlotId::LotV))
        .unwrap()
        .next()
        .is_none());
    assert!(fixture
        .layout
        .slot_dir(SlotId::Nco)
        .join("second.SC2Map/payload")
        .is_file());
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);

    fixture.workflow().restore_vanilla().unwrap();
    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(fixture.store.managed_mods().unwrap().is_empty());
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
}

#[test]
fn session_health_reuses_startup_and_committed_verification() {
    let fixture = Fixture::new();
    let verifications = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&verifications);
    let workflow = fixture.workflow().with_verification_probe(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    });

    workflow.library_snapshot().unwrap();
    workflow.library_snapshot().unwrap();
    assert_eq!(verifications.load(Ordering::SeqCst), 1);

    workflow.activate(&fixture.first).unwrap();
    workflow.library_snapshot().unwrap();
    assert_eq!(verifications.load(Ordering::SeqCst), 3);

    workflow.restore_vanilla().unwrap();
    workflow.library_snapshot().unwrap();
    assert_eq!(verifications.load(Ordering::SeqCst), 5);
}

#[test]
fn active_health_marks_an_unexpected_campaign_file_as_repairable_drift() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    let rogue = fixture
        .layout
        .slot_dir(SlotId::Wol)
        .join("rogue.backup-user.SC2Map/payload");
    std::fs::create_dir_all(rogue.parent().unwrap()).unwrap();
    std::fs::write(&rogue, b"rogue").unwrap();

    let health = fixture.workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues.len(), 1);
    assert_eq!(health.issues[0].code, "slot_drift");
    assert!(health.issues[0].repairable);
    assert_eq!(std::fs::read(rogue).unwrap(), b"rogue");
}

#[test]
fn vanilla_health_accepts_preexisting_loose_campaign_overrides() {
    let fixture = Fixture::new();
    let rogue = fixture
        .layout
        .slot_dir(SlotId::Wol)
        .join("rogue.staging-user.SC2Map/payload");
    std::fs::create_dir_all(rogue.parent().unwrap()).unwrap();
    std::fs::write(&rogue, b"rogue").unwrap();

    let health = fixture.workflow().health();
    assert_eq!(health.state, HealthState::Ready);
    assert!(health.issues.is_empty());
    assert_eq!(std::fs::read(rogue).unwrap(), b"rogue");
}

#[test]
fn vanilla_health_preserves_an_exact_owned_slot_operation_artifact() {
    let fixture = Fixture::new();
    let artifact = fixture
        .layout
        .slot_dir(SlotId::Wol)
        .join("void.backup-op-1/payload");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, b"pending").unwrap();

    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
    assert_eq!(std::fs::read(artifact).unwrap(), b"pending");
}

#[test]
fn restart_recovers_every_activation_checkpoint_without_a_mixed_state() {
    for point in [
        FailurePoint::Preparing,
        FailurePoint::SavesPrepared,
        FailurePoint::SlotsPrepared,
        FailurePoint::ModsPrepared,
        FailurePoint::Prepared,
        FailurePoint::SavesSwapped,
        FailurePoint::SlotsSwapped,
        FailurePoint::ModsSwapped,
        FailurePoint::LedgerCommittedBeforeJournal,
        FailurePoint::LedgerCommitted,
    ] {
        let fixture = Fixture::new();
        let error = fixture
            .workflow()
            .with_fail_after(point)
            .activate(&fixture.first)
            .unwrap_err();
        assert_eq!(error.code(), "simulated_interruption", "{point:?}");
        assert!(PendingOperation::load(fixture.store.root())
            .unwrap()
            .is_some());

        fixture.workflow().recover_pending().unwrap();
        assert!(PendingOperation::load(fixture.store.root())
            .unwrap()
            .is_none());
        assert_eq!(fixture.workflow().health().state, HealthState::Ready);
        let committed = matches!(
            point,
            FailurePoint::LedgerCommittedBeforeJournal | FailurePoint::LedgerCommitted
        );
        assert_eq!(
            fixture.store.active_campaign().unwrap().is_some(),
            committed,
            "{point:?}"
        );
        assert_eq!(
            fixture
                .layout
                .slot_dir(SlotId::LotV)
                .join("first.SC2Map/payload")
                .is_file(),
            committed,
            "{point:?}"
        );
        assert_eq!(
            fixture.layout.mods_dir().join("first.SC2Mod").is_file(),
            committed,
            "{point:?}"
        );
    }
}

#[test]
fn play_launch_failure_leaves_the_fully_committed_target_active() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();

    let error = fixture
        .workflow()
        .play_with(&fixture.second, |layout| {
            assert!(layout
                .slot_dir(SlotId::Nco)
                .join("second.SC2Map/payload")
                .is_file());
            Err(user_err("test_launch_failure", "test launcher failed"))
        })
        .unwrap_err();

    assert_eq!(error.code(), "launch_failed_after_activation");
    assert_eq!(
        fixture.store.active_campaign().unwrap().unwrap().id,
        fixture.second
    );
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
}

#[test]
fn active_play_and_restore_do_not_depend_on_retained_package_blobs() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    remove_first_package_blob(&fixture.store, &fixture.first);

    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
    fixture
        .workflow()
        .play_with(&fixture.first, |_| Ok(()))
        .unwrap();
    fixture.workflow().restore_vanilla().unwrap();

    assert!(fixture.store.active_campaign().unwrap().is_none());
    let vanilla_slot = fixture.layout.campaign_dir();
    let metadata = std::fs::symlink_metadata(&vanilla_slot).unwrap();
    assert!(metadata.is_dir());
    assert!(!metadata.file_type().is_symlink());
    assert!(std::fs::read_dir(vanilla_slot).unwrap().next().is_none());
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
}

#[test]
fn rollback_recovery_uses_the_manifest_evidence_when_a_blob_is_lost() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    remove_first_package_blob(&fixture.store, &fixture.first);
    let reopened = Store::open_for_tests(fixture.store.root()).unwrap();

    Workflow::new(&fixture.layout, &reopened)
        .with_strategy(Some(StrategyChoice::Copy))
        .with_running_probe(|| false)
        .recover_pending()
        .unwrap();

    assert!(reopened.active_campaign().unwrap().is_none());
    assert!(!fixture.layout.slot_dir(SlotId::LotV).exists());
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
    assert!(PendingOperation::load(reopened.root()).unwrap().is_none());
}

#[test]
fn unresolved_required_save_isolation_blocks_transitions_before_journaling() {
    let fixture = Fixture::new();
    let unavailable = || {
        Workflow::new(&fixture.layout, &fixture.store)
            .with_strategy(Some(StrategyChoice::Copy))
            .with_save_isolation_expected(true)
            .with_running_probe(|| false)
    };

    let health = unavailable().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues[0].code, "save_profile_unavailable");
    let error = unavailable().activate(&fixture.first).unwrap_err();
    assert_eq!(error.code(), "save_profile_unavailable");
    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
    assert!(!fixture.layout.slot_dir(SlotId::LotV).exists());

    fixture.workflow().activate(&fixture.first).unwrap();
    let error = unavailable().restore_vanilla().unwrap_err();
    assert_eq!(error.code(), "save_profile_unavailable");
    assert_eq!(
        fixture.store.active_campaign().unwrap().unwrap().id,
        fixture.first
    );
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
    assert!(fixture
        .layout
        .slot_dir(SlotId::LotV)
        .join("first.SC2Map/payload")
        .is_file());
}

#[test]
fn explicit_repair_replaces_changed_created_files() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    let slot_file = fixture
        .layout
        .slot_dir(SlotId::LotV)
        .join("first.SC2Map/payload");
    let mod_file = fixture.layout.mods_dir().join("first.SC2Mod");
    std::fs::write(&slot_file, b"slot drift").unwrap();
    std::fs::write(&mod_file, b"mod drift").unwrap();
    assert_eq!(fixture.workflow().health().state, HealthState::Drifted);

    fixture.workflow().repair_active().unwrap();
    assert_eq!(std::fs::read(slot_file).unwrap(), b"first");
    assert_eq!(std::fs::read(mod_file).unwrap(), b"first");
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
}

#[test]
fn interrupted_repair_recovers_to_original_drift_or_verified_target() {
    for point in [
        FailurePoint::Preparing,
        FailurePoint::SavesPrepared,
        FailurePoint::SlotsPrepared,
        FailurePoint::ModsPrepared,
        FailurePoint::Prepared,
        FailurePoint::SavesSwapped,
        FailurePoint::SlotsSwapped,
        FailurePoint::ModsSwapped,
        FailurePoint::LedgerCommittedBeforeJournal,
        FailurePoint::LedgerCommitted,
    ] {
        let fixture = Fixture::new();
        fixture.workflow().activate(&fixture.first).unwrap();
        let slot_file = fixture
            .layout
            .slot_dir(SlotId::LotV)
            .join("first.SC2Map/payload");
        let mod_file = fixture.layout.mods_dir().join("first.SC2Mod");
        std::fs::write(&slot_file, b"slot drift").unwrap();
        std::fs::write(&mod_file, b"mod drift").unwrap();

        let error = fixture
            .workflow()
            .with_fail_after(point)
            .repair_active()
            .unwrap_err();
        assert_eq!(error.code(), "simulated_interruption", "{point:?}");
        fixture.workflow().recover_pending().unwrap();
        assert!(PendingOperation::load(fixture.store.root())
            .unwrap()
            .is_none());

        let repaired = matches!(
            point,
            FailurePoint::ModsSwapped
                | FailurePoint::LedgerCommittedBeforeJournal
                | FailurePoint::LedgerCommitted
        );
        assert_eq!(
            std::fs::read(&slot_file).unwrap(),
            if repaired {
                b"first".as_slice()
            } else {
                b"slot drift".as_slice()
            },
            "{point:?}"
        );
        assert_eq!(
            std::fs::read(&mod_file).unwrap(),
            if repaired {
                b"first".as_slice()
            } else {
                b"mod drift".as_slice()
            },
            "{point:?}"
        );
        assert_eq!(
            fixture.workflow().health().state,
            if repaired {
                HealthState::Ready
            } else {
                HealthState::Drifted
            },
            "{point:?}"
        );
    }
}

#[test]
fn running_game_blocks_before_any_staging_or_ledger_change() {
    let fixture = Fixture::new();
    let error = Workflow::new(&fixture.layout, &fixture.store)
        .with_strategy(Some(StrategyChoice::Copy))
        .with_running_probe(|| true)
        .activate(&fixture.first)
        .unwrap_err();
    assert_eq!(error.code(), "game_running");
    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
    assert!(fixture
        .layout
        .slot_dir(SlotId::LotV)
        .symlink_metadata()
        .is_err());
}

#[test]
fn game_starting_mid_transition_preserves_the_journal_until_stopped() {
    let fixture = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let probe_calls = Arc::clone(&calls);
    let error = Workflow::new(&fixture.layout, &fixture.store)
        .with_strategy(Some(StrategyChoice::Copy))
        .with_running_probe(move || probe_calls.fetch_add(1, Ordering::SeqCst) == 6)
        .activate(&fixture.first)
        .unwrap_err();

    assert_eq!(error.code(), "game_running");
    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(fixture
        .layout
        .slot_dir(SlotId::LotV)
        .join("first.SC2Map/payload")
        .is_file());
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());

    fixture.workflow().recover_pending().unwrap();

    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(!fixture.layout.slot_dir(SlotId::LotV).exists());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
}

#[test]
fn recovery_rejects_tampered_paths_and_preserves_the_journal() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::Prepared)
        .activate(&fixture.first)
        .unwrap_err();
    let mut journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    journal.paths.mods_backup = Some(Path::new("/tmp/not-starvault").to_path_buf());
    journal.persist(fixture.store.root()).unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "unsafe_operation_journal");
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
    assert!(fixture.store.active_campaign().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn recovery_rejects_a_linked_game_ancestor_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::SlotsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let maps = fixture.layout.root().join("Maps");
    let original_maps = fixture.layout.root().join("Maps-original");
    std::fs::rename(&maps, &original_maps).unwrap();
    let external = fixture._temp.path().join("external-maps");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    symlink(&external, &maps).unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();

    assert_eq!(error.code(), "unsafe_game_layout");
    assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());

    std::fs::remove_file(&maps).unwrap();
    std::fs::rename(&original_maps, &maps).unwrap();
    fixture.workflow().recover_pending().unwrap();
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
}

#[cfg(unix)]
#[test]
fn restore_rejects_a_linked_nested_slot_directory_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    let container = fixture.layout.slot_dir(SlotId::LotV).join("first.SC2Map");
    std::fs::remove_dir_all(&container).unwrap();
    let external = fixture._temp.path().join("external-map");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("payload"), b"first").unwrap();
    symlink(&external, &container).unwrap();

    assert_eq!(fixture.workflow().health().state, HealthState::Drifted);
    let error = fixture.workflow().restore_vanilla().unwrap_err();

    assert_eq!(error.code(), "slot_drift");
    assert_eq!(std::fs::read(external.join("payload")).unwrap(), b"first");
    assert!(container
        .symlink_metadata()
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[cfg(windows)]
#[test]
fn recovery_rejects_a_junctioned_game_ancestor_without_touching_its_target() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::SlotsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let maps = fixture.layout.root().join("Maps");
    let original_maps = fixture.layout.root().join("Maps-original");
    std::fs::rename(&maps, &original_maps).unwrap();
    let external = fixture._temp.path().join("external-maps");
    std::fs::create_dir_all(&external).unwrap();
    let sentinel = external.join("sentinel");
    std::fs::write(&sentinel, b"keep").unwrap();
    junction::create(&external, &maps).unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();

    assert_eq!(error.code(), "unsafe_game_layout");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());

    std::fs::remove_dir(&maps).unwrap();
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    std::fs::rename(&original_maps, &maps).unwrap();
    fixture.workflow().recover_pending().unwrap();
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[test]
fn recovery_never_deletes_a_slot_changed_after_interruption() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::SlotsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let live = fixture
        .layout
        .slot_dir(SlotId::LotV)
        .join("first.SC2Map/payload");
    std::fs::write(&live, b"changed while app was closed").unwrap();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(
        std::fs::read(&live).unwrap(),
        b"changed while app was closed"
    );
    assert!(journal.paths.slots[0].backup.symlink_metadata().is_ok());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn recovery_never_deletes_a_managed_mod_changed_after_interruption() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let live = fixture.layout.mods_dir().join("first.SC2Mod");
    std::fs::write(&live, b"changed while app was closed").unwrap();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "managed_file_changed");
    assert_eq!(
        std::fs::read(&live).unwrap(),
        b"changed while app was closed"
    );
    assert!(journal
        .paths
        .mods_backup
        .unwrap()
        .symlink_metadata()
        .is_ok());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn restore_recovery_never_overwrites_a_new_file_at_a_backed_up_mod_path() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .restore_vanilla()
        .unwrap_err();
    let live = fixture.layout.mods_dir().join("first.SC2Mod");
    assert!(!live.exists());
    std::fs::write(&live, b"new external file").unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();

    assert_eq!(error.code(), "managed_file_changed");
    assert_eq!(std::fs::read(&live).unwrap(), b"new external file");
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[cfg(any(unix, windows))]
#[test]
fn recovery_rejects_a_linked_mod_temporary_without_touching_its_target() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .restore_vanilla()
        .unwrap_err();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    let backup = journal.paths.mods_backup.unwrap();
    let temporary = mods_temporary_path(&backup, "first.SC2Mod");
    let external = fixture._temp.path().join("external-temporary");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external, &temporary);

    let error = fixture.workflow().recover_pending().unwrap_err();

    assert_eq!(error.code(), "unsafe_operation_artifact");
    assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
    assert!(temporary.symlink_metadata().is_ok());
    assert!(backup.is_dir());
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());

    remove_directory_link(&temporary);
    fixture.workflow().recover_pending().unwrap();
    assert_eq!(
        std::fs::read(fixture.layout.mods_dir().join("first.SC2Mod")).unwrap(),
        b"first"
    );
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[cfg(any(unix, windows))]
#[test]
fn recovery_cleans_a_bound_regular_mod_temporary_left_by_a_copy_crash() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::SlotsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    assert_eq!(journal.phase, OperationPhase::SlotsSwapped);
    let backup = journal.paths.mods_backup.unwrap();
    let temporary = mods_temporary_path(&backup, "first.SC2Mod");
    std::fs::write(backup.join(".apply-started"), b"started").unwrap();
    std::fs::write(&temporary, b"partial target bytes").unwrap();
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());

    fixture.workflow().recover_pending().unwrap();

    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
    assert!(!fixture.layout.slot_dir(SlotId::LotV).exists());
    assert!(!backup.exists());
    assert!(!temporary.exists());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[test]
fn recovery_rejects_a_replaced_mods_plan_before_touching_live_mods() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    let backup = journal.paths.mods_backup.unwrap();
    let plan = backup.join("mods-plan.json");
    let live = fixture.layout.mods_dir().join("first.SC2Mod");
    assert_eq!(std::fs::read(&live).unwrap(), b"first");

    std::fs::write(&plan, b"{}").unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "corrupt_operation_journal");
    assert_eq!(std::fs::read(&live).unwrap(), b"first");
    assert_eq!(std::fs::read(&plan).unwrap(), b"{}");
    assert!(backup.is_dir());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn recovery_rechecks_the_mods_plan_binding_after_global_preflight() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    let plan = journal
        .paths
        .mods_backup
        .as_ref()
        .unwrap()
        .join("mods-plan.json");
    let mut substituted = std::fs::read(&plan).unwrap();
    substituted.push(b'\n');

    let error = fixture
        .workflow()
        .with_rollback_pre_mutation_hook(move || {
            std::fs::write(&plan, &substituted).unwrap();
        })
        .recover_pending()
        .unwrap_err();

    assert_eq!(error.code(), "corrupt_operation_journal");
    assert_eq!(
        std::fs::read(fixture.layout.mods_dir().join("first.SC2Mod")).unwrap(),
        b"first"
    );
    assert_eq!(
        std::fs::read(
            fixture
                .layout
                .slot_dir(SlotId::LotV)
                .join("first.SC2Map/payload")
        )
        .unwrap(),
        b"first"
    );
    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn incomplete_managed_mods_ledger_blocks_restore_without_deleting_mods() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    let live = fixture.layout.mods_dir().join("first.SC2Mod");
    let managed = fixture.store.managed_mods().unwrap();
    assert_eq!(managed.len(), 1);

    let connection = rusqlite::Connection::open(fixture.store.root().join("ledger.db")).unwrap();
    assert_eq!(
        connection
            .execute(
                "DELETE FROM managed_mods WHERE path = ?1",
                [&managed[0].path],
            )
            .unwrap(),
        1
    );
    drop(connection);

    let health = fixture.workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues[0].code, "managed_mods_manifest_mismatch");

    let error = fixture.workflow().restore_vanilla().unwrap_err();
    assert_eq!(error.code(), "managed_mods_manifest_mismatch");
    assert_eq!(std::fs::read(&live).unwrap(), b"first");
    assert!(fixture.store.active_campaign().unwrap().is_some());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[test]
fn vanilla_managed_mod_rows_are_nonrepairable_orphaned_state() {
    let fixture = Fixture::new();
    let connection = rusqlite::Connection::open(fixture.store.root().join("ledger.db")).unwrap();
    connection
        .execute(
            "INSERT INTO managed_mods(path, sha256, disposition) VALUES (?1, ?2, 'created')",
            ["orphan.SC2Mod", &"a".repeat(64)],
        )
        .unwrap();
    drop(connection);

    let health = fixture.workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues.len(), 1);
    assert_eq!(health.issues[0].code, "orphaned_managed_mods");
    assert!(!health.issues[0].repairable);

    let error = fixture.workflow().restore_vanilla().unwrap_err();
    assert_eq!(error.code(), "orphaned_managed_mods");
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[test]
fn active_manifest_replacement_is_not_hidden_by_the_store_cache() {
    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    let cached = fixture.store.load_manifest(&fixture.first).unwrap();

    let replacement_store =
        Store::open_for_tests(fixture._temp.path().join("replacement-store")).unwrap();
    ingest(
        &replacement_store,
        &fixture._temp.path().join("replacement-source"),
        "first",
        SlotId::LotV,
        b"replacement",
    );
    let replacement = replacement_store.load_manifest(&fixture.first).unwrap();
    assert_ne!(cached.revision, replacement.revision);
    let replacement_manifest = replacement_store
        .root()
        .join("packages/first/manifest.json");
    let active_manifest = fixture.store.root().join("packages/first/manifest.json");
    std::fs::copy(replacement_manifest, active_manifest).unwrap();

    let health = fixture.workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues[0].code, "active_campaign_manifest_mismatch");
    assert!(!health.issues[0].repairable);

    let error = fixture.workflow().restore_vanilla().unwrap_err();
    assert_eq!(error.code(), "active_campaign_manifest_mismatch");
    assert_eq!(
        std::fs::read(
            fixture
                .layout
                .slot_dir(SlotId::LotV)
                .join("first.SC2Map/payload")
        )
        .unwrap(),
        b"first"
    );
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[test]
fn recovery_rejects_incomplete_slot_state_bindings_before_mutation() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::Prepared)
        .activate(&fixture.first)
        .unwrap_err();
    let path = PendingOperation::path(fixture.store.root());
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["paths"]["slots"]["previous_states"] = serde_json::Value::Array(Vec::new());
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "unsafe_operation_journal");
    assert!(!fixture
        .layout
        .slot_dir(SlotId::LotV)
        .join("first.SC2Map/payload")
        .exists());
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[cfg(unix)]
#[test]
fn active_external_slot_link_is_nonrepairable_and_never_followed() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    let live = fixture.layout.campaign_dir();
    let external = fixture._temp.path().join("external-active-slot");
    std::fs::rename(&live, &external).unwrap();
    symlink(&external, &live).unwrap();

    let health = fixture.workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues[0].code, "unowned_campaign_slot_link");
    assert!(!health.issues[0].repairable);
    let error = fixture.workflow().restore_vanilla().unwrap_err();
    assert_eq!(error.code(), "unowned_campaign_slot_link");
    assert_eq!(
        std::fs::read(external.join("void/first.SC2Map/payload")).unwrap(),
        b"first"
    );
    assert!(std::fs::symlink_metadata(&live)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());
}

#[cfg(unix)]
#[test]
fn dangling_owned_slot_link_is_repaired_to_a_verified_copy_before_restore() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    let manifest = fixture.store.load_manifest(&fixture.first).unwrap();
    let deployed = fixture
        .store
        .deploy_dir(manifest.faction, &manifest.revision)
        .unwrap();
    fixture
        .store
        .materialize_campaign(&manifest, &deployed)
        .unwrap();
    let live = fixture.layout.campaign_dir();
    std::fs::remove_dir_all(&live).unwrap();
    symlink(&deployed, &live).unwrap();
    std::fs::remove_dir_all(&deployed).unwrap();

    let health = fixture.workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues[0].code, "slot_drift");
    assert!(health.issues[0].repairable);
    let error = fixture.workflow().restore_vanilla().unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert!(live.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_none());

    fixture.workflow().repair_active().unwrap();
    assert!(!live.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        std::fs::read(live.join("void/first.SC2Map/payload")).unwrap(),
        b"first"
    );
    assert_eq!(fixture.workflow().health().state, HealthState::Ready);

    fixture.workflow().restore_vanilla().unwrap();
    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(std::fs::read_dir(&live).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn recovery_rejects_a_copy_to_link_target_substitution_before_mods_rollback() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let live = fixture.layout.slot_dir(SlotId::LotV);
    let external = fixture._temp.path().join("substituted-target-slot");
    std::fs::rename(&live, &external).unwrap();
    symlink(&external, &live).unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(
        std::fs::read(external.join("first.SC2Map/payload")).unwrap(),
        b"first"
    );
    assert_eq!(
        std::fs::read(fixture.layout.mods_dir().join("first.SC2Mod")).unwrap(),
        b"first"
    );
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[cfg(unix)]
#[test]
fn recovery_rejects_a_copy_to_link_backup_substitution_before_mods_rollback() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fixture.workflow().activate(&fixture.first).unwrap();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .activate(&fixture.second)
        .unwrap_err();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    let previous_slot = &journal.paths.slots[0];
    let external = fixture._temp.path().join("substituted-previous-slot");
    std::fs::rename(&previous_slot.backup, &external).unwrap();
    symlink(&external, &previous_slot.backup).unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(
        std::fs::read(external.join("void/first.SC2Map/payload")).unwrap(),
        b"first"
    );
    assert_eq!(
        std::fs::read(fixture.layout.mods_dir().join("second.SC2Mod")).unwrap(),
        b"second"
    );
    assert_eq!(
        fixture.store.active_campaign().unwrap().unwrap().id,
        fixture.first
    );
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn committed_recovery_rejects_loss_of_the_preserved_plain_tree() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::LedgerCommitted)
        .activate(&fixture.first)
        .unwrap_err();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    for slot in &journal.paths.slots {
        if slot.backup.symlink_metadata().is_ok() {
            std::fs::remove_dir_all(&slot.backup).unwrap();
        }
    }
    let error = fixture.workflow().recover_pending().unwrap_err();

    assert_eq!(error.code(), "slot_drift");
    assert!(fixture.store.active_campaign().unwrap().is_some());
    assert_eq!(
        std::fs::read(fixture.layout.mods_dir().join("first.SC2Mod")).unwrap(),
        b"first"
    );
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn committed_cleanup_rejects_a_same_path_slot_backup_substitution_before_any_deletion() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::LedgerCommitted)
        .activate(&fixture.first)
        .unwrap_err();
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    let slot_backup = journal.paths.slots[0].backup.clone();
    let mods_backup = journal.paths.mods_backup.clone().unwrap();
    let mods_staging = journal.paths.mods_staging.clone().unwrap();
    std::fs::remove_dir_all(&slot_backup).unwrap();
    std::fs::create_dir(&slot_backup).unwrap();
    std::fs::write(slot_backup.join("unrelated"), b"preserve me").unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(
        std::fs::read(slot_backup.join("unrelated")).unwrap(),
        b"preserve me"
    );
    assert!(mods_backup.is_dir());
    assert!(mods_staging.is_dir());
    assert_eq!(
        fixture.store.active_campaign().unwrap().unwrap().id,
        fixture.first
    );
    assert_eq!(
        std::fs::read(
            fixture
                .layout
                .slot_dir(SlotId::LotV)
                .join("first.SC2Map/payload")
        )
        .unwrap(),
        b"first"
    );
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn direct_committed_cleanup_globally_preflights_before_deleting_slot_artifacts() {
    let fixture = Fixture::new();
    let substituted = Arc::new(AtomicBool::new(false));
    let hook_substituted = Arc::clone(&substituted);
    let store_root = fixture.store.root().to_path_buf();
    let error = Workflow::new(&fixture.layout, &fixture.store)
        .with_strategy(Some(StrategyChoice::Copy))
        .with_running_probe(move || {
            let Ok(Some(journal)) = PendingOperation::load(&store_root) else {
                return false;
            };
            if journal.phase == OperationPhase::ModsSwapped
                && !hook_substituted.swap(true, Ordering::SeqCst)
            {
                let staging = journal.paths.mods_staging.unwrap();
                std::fs::remove_dir_all(&staging).unwrap();
                std::fs::create_dir(&staging).unwrap();
                std::fs::write(staging.join("unrelated"), b"preserve me").unwrap();
            }
            false
        })
        .activate(&fixture.first)
        .unwrap_err();

    assert!(substituted.load(Ordering::SeqCst));
    assert_eq!(error.code(), "operation_recovery_failed");
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    let staging = journal.paths.mods_staging.clone().unwrap();
    let backup = journal.paths.mods_backup.clone().unwrap();
    assert_eq!(
        std::fs::read(staging.join("unrelated")).unwrap(),
        b"preserve me"
    );
    assert!(backup.is_dir());
    assert!(journal.paths.slots[0].backup.symlink_metadata().is_ok());
    assert!(!fixture.layout.plain_campaign_dir().exists());
    assert_eq!(
        fixture.store.active_campaign().unwrap().unwrap().id,
        fixture.first
    );
    assert_eq!(
        std::fs::read(
            fixture
                .layout
                .slot_dir(SlotId::LotV)
                .join("first.SC2Map/payload")
        )
        .unwrap(),
        b"first"
    );
}

#[test]
fn rollback_verified_cleanup_rejects_a_same_path_mods_staging_substitution_before_any_deletion() {
    let fixture = Fixture::new();
    fixture
        .workflow()
        .with_fail_after(FailurePoint::ModsSwapped)
        .activate(&fixture.first)
        .unwrap_err();
    let error = fixture
        .workflow()
        .with_fail_after(FailurePoint::RollbackVerified)
        .recover_pending()
        .unwrap_err();
    assert_eq!(error.code(), "simulated_interruption");
    let journal = PendingOperation::load(fixture.store.root())
        .unwrap()
        .unwrap();
    assert_eq!(journal.phase, OperationPhase::RollbackVerified);
    let mods_backup = journal.paths.mods_backup.clone().unwrap();
    let mods_staging = journal.paths.mods_staging.clone().unwrap();
    let plan = std::fs::read(mods_backup.join("mods-plan.json")).unwrap();
    std::fs::remove_dir_all(&mods_staging).unwrap();
    std::fs::create_dir(&mods_staging).unwrap();
    std::fs::write(mods_staging.join("unrelated"), b"preserve me").unwrap();

    let error = fixture.workflow().recover_pending().unwrap_err();
    assert_eq!(error.code(), "unsafe_operation_artifact");
    assert_eq!(
        std::fs::read(mods_staging.join("unrelated")).unwrap(),
        b"preserve me"
    );
    assert!(mods_backup.is_dir());
    assert_eq!(
        std::fs::read(mods_backup.join("mods-plan.json")).unwrap(),
        plan
    );
    assert!(fixture.store.active_campaign().unwrap().is_none());
    assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
    assert!(!fixture.layout.slot_dir(SlotId::LotV).exists());
    assert!(PendingOperation::load(fixture.store.root())
        .unwrap()
        .is_some());
}

#[test]
fn rollback_restarts_after_every_cross_resource_checkpoint() {
    for point in [
        FailurePoint::RollbackModsRestored,
        FailurePoint::RollbackSlotsRestored,
        FailurePoint::RollbackSavesRestored,
        FailurePoint::RollbackVerified,
    ] {
        let fixture = Fixture::new();
        fixture
            .workflow()
            .with_fail_after(FailurePoint::ModsSwapped)
            .activate(&fixture.first)
            .unwrap_err();

        let error = fixture
            .workflow()
            .with_fail_after(point)
            .recover_pending()
            .unwrap_err();
        assert_eq!(error.code(), "simulated_interruption", "{point:?}");
        assert!(PendingOperation::load(fixture.store.root())
            .unwrap()
            .is_some());

        fixture.workflow().recover_pending().unwrap();
        assert_eq!(fixture.workflow().health().state, HealthState::Ready);
        assert!(fixture.store.active_campaign().unwrap().is_none());
        assert!(!fixture.layout.mods_dir().join("first.SC2Mod").exists());
        assert!(PendingOperation::load(fixture.store.root())
            .unwrap()
            .is_none());
    }
}
