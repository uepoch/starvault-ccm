use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use svccm_core::config::StrategyChoice;
use svccm_core::contracts::{ActiveCampaign, HealthState};
use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::operation::PendingOperation;
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::saves::{SaveOwner, SaveTransition, SavesManager};
use svccm_core::store::Store;
use svccm_core::workflow::{FailurePoint, Workflow};

const FAILURE_POINTS: [FailurePoint; 10] = [
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
];

#[derive(Debug, Clone, Copy)]
enum Scenario {
    VanillaToA,
    SameFaction,
    CrossFaction,
    RestoreVanilla,
}

impl Scenario {
    const ALL: [Self; 4] = [
        Self::VanillaToA,
        Self::SameFaction,
        Self::CrossFaction,
        Self::RestoreVanilla,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeEntry {
    Directory,
    File(Vec<u8>),
    Link(PathBuf),
}

type ProfileTree = BTreeMap<String, TreeEntry>;

struct Fixture {
    _temporary: tempfile::TempDir,
    layout: WindowsLayout,
    store_root: PathBuf,
    profile_root: PathBuf,
    saves: PathBuf,
    banks: PathBuf,
    a: PackageId,
    b: PackageId,
    c: PackageId,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temporary.path().join("sc2"));
        std::fs::create_dir_all(layout.root()).unwrap();
        std::fs::write(layout.exe(), b"fake executable").unwrap();
        let store_root = temporary.path().join("store");
        let profile_root = temporary.path().join("profile");
        let saves = profile_root.join("Saves");
        let banks = profile_root.join("Banks");
        let store = Store::open_for_tests(&store_root).unwrap();
        let a = ingest(
            &store,
            &temporary.path().join("source-a"),
            "campaign-a",
            SlotId::LotV,
        );
        let b = ingest(
            &store,
            &temporary.path().join("source-b"),
            "campaign-b",
            SlotId::LotV,
        );
        let c = ingest(
            &store,
            &temporary.path().join("source-c"),
            "campaign-c",
            SlotId::HotS,
        );
        drop(store);
        Self {
            _temporary: temporary,
            layout,
            store_root,
            profile_root,
            saves,
            banks,
            a,
            b,
            c,
        }
    }

    fn open_store(&self) -> Store {
        Store::open_for_tests(&self.store_root).unwrap()
    }

    fn saves_manager(&self) -> SavesManager {
        SavesManager::new(self.saves.clone(), &self.store_root)
    }

    fn workflow<'a>(&'a self, store: &'a Store) -> Workflow<'a> {
        Workflow::new(&self.layout, store)
            .with_strategy(Some(StrategyChoice::Copy))
            .with_saves(Some(self.saves_manager()))
            .with_running_probe(|| false)
    }

    fn seed_plain(&self) {
        touch(
            &self.saves.join("LibertyCampaignSave.SC2Save"),
            b"plain-wol",
        );
        touch(&self.saves.join("SwarmCampaignSave.SC2Save"), b"plain-hots");
        touch(&self.saves.join("VoidCampaignSave.SC2Save"), b"plain-lotv");
        touch(&self.saves.join("NovaCampaign01Save.SC2Save"), b"plain-nco");
        touch(
            &self.saves.join("Campaign/plain.SC2Save"),
            b"plain-campaign",
        );
        touch(&self.saves.join("Unsaved/plain.SC2Save"), b"plain-unsaved");
        touch(
            &self.saves.join("Multiplayer/shared.SC2Save"),
            b"multiplayer",
        );
        touch(&self.saves.join("Challenge/shared.SC2Save"), b"challenge");
        touch(&self.saves.join("unowned.txt"), b"unowned");
        touch(&self.banks.join("author/plain.SC2Bank"), b"plain-bank");
        touch(&self.banks.join("ZCampaignStats.SC2Bank"), b"vanilla-bank");
    }

    fn write_active_state(&self, label: &str, faction: SlotId) {
        let root_name = match faction {
            SlotId::Wol => "LibertyCampaignSave.SC2Save",
            SlotId::HotS => "SwarmCampaignSave.SC2Save",
            SlotId::LotV => "VoidCampaignSave.SC2Save",
            SlotId::Nco => "NovaCampaign01Save.SC2Save",
        };
        touch(
            &self.saves.join(root_name),
            format!("{label}-root").as_bytes(),
        );
        touch(
            &self.saves.join(format!("Campaign/{label}.SC2Save")),
            format!("{label}-campaign").as_bytes(),
        );
        touch(
            &self.saves.join(format!("Unsaved/{label}.SC2Save")),
            format!("{label}-unsaved").as_bytes(),
        );
        touch(
            &self.banks.join(format!("author/{label}.SC2Bank")),
            format!("{label}-bank").as_bytes(),
        );
    }

    fn snapshot(&self) -> ProfileTree {
        let mut tree = BTreeMap::new();
        snapshot_path(&self.profile_root, &self.saves, &mut tree);
        snapshot_path(&self.profile_root, &self.banks, &mut tree);
        tree
    }

    fn game_snapshot(&self) -> ProfileTree {
        let mut tree = BTreeMap::new();
        snapshot_path(self.layout.root(), self.layout.root(), &mut tree);
        tree
    }
}

struct ExpectedTransition {
    previous: ProfileTree,
    target: ProfileTree,
    previous_campaign: Option<PackageId>,
    target_campaign: Option<PackageId>,
}

fn ingest(store: &Store, source: &Path, id: &str, faction: SlotId) -> PackageId {
    let payload = id.as_bytes();
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

fn touch(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn snapshot_path(profile_root: &Path, path: &Path, tree: &mut ProfileTree) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    let relative = path
        .strip_prefix(profile_root)
        .unwrap()
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        tree.insert(relative, TreeEntry::Link(std::fs::read_link(path).unwrap()));
    } else if file_type.is_dir() {
        tree.insert(relative, TreeEntry::Directory);
        let mut children = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            snapshot_path(profile_root, &child, tree);
        }
    } else {
        tree.insert(relative, TreeEntry::File(std::fs::read(path).unwrap()));
    }
}

fn prepare_scenario(fixture: &Fixture, store: &Store, scenario: Scenario) -> ExpectedTransition {
    fixture.seed_plain();
    let workflow = fixture.workflow(store);
    match scenario {
        Scenario::VanillaToA => {
            let previous = fixture.snapshot();
            workflow.activate(&fixture.a).unwrap();
            let target = fixture.snapshot();
            workflow.restore_vanilla().unwrap();
            assert_eq!(fixture.snapshot(), previous);
            ExpectedTransition {
                previous,
                target,
                previous_campaign: None,
                target_campaign: Some(fixture.a.clone()),
            }
        }
        Scenario::SameFaction => {
            workflow.activate(&fixture.b).unwrap();
            fixture.write_active_state("b", SlotId::LotV);
            workflow.activate(&fixture.a).unwrap();
            fixture.write_active_state("a", SlotId::LotV);
            workflow.activate(&fixture.b).unwrap();
            let target = fixture.snapshot();
            workflow.activate(&fixture.a).unwrap();
            let previous = fixture.snapshot();
            ExpectedTransition {
                previous,
                target,
                previous_campaign: Some(fixture.a.clone()),
                target_campaign: Some(fixture.b.clone()),
            }
        }
        Scenario::CrossFaction => {
            workflow.activate(&fixture.c).unwrap();
            fixture.write_active_state("c", SlotId::HotS);
            workflow.activate(&fixture.a).unwrap();
            fixture.write_active_state("a", SlotId::LotV);
            workflow.activate(&fixture.c).unwrap();
            let target = fixture.snapshot();
            workflow.activate(&fixture.a).unwrap();
            let previous = fixture.snapshot();
            ExpectedTransition {
                previous,
                target,
                previous_campaign: Some(fixture.a.clone()),
                target_campaign: Some(fixture.c.clone()),
            }
        }
        Scenario::RestoreVanilla => {
            let target = fixture.snapshot();
            workflow.activate(&fixture.a).unwrap();
            fixture.write_active_state("a", SlotId::LotV);
            ExpectedTransition {
                previous: fixture.snapshot(),
                target,
                previous_campaign: Some(fixture.a.clone()),
                target_campaign: None,
            }
        }
    }
}

fn invoke_interrupted(fixture: &Fixture, store: &Store, scenario: Scenario, point: FailurePoint) {
    let workflow = fixture.workflow(store).with_fail_after(point);
    let error = match scenario {
        Scenario::VanillaToA => workflow.activate(&fixture.a).err(),
        Scenario::SameFaction => workflow.activate(&fixture.b).err(),
        Scenario::CrossFaction => workflow.activate(&fixture.c).err(),
        Scenario::RestoreVanilla => workflow.restore_vanilla().err(),
    }
    .expect("failure point must interrupt the operation");
    assert_eq!(
        error.code(),
        "simulated_interruption",
        "{scenario:?} {point:?}"
    );
}

fn expected_active(transition: &ExpectedTransition, committed: bool) -> Option<&PackageId> {
    if committed {
        transition.target_campaign.as_ref()
    } else {
        transition.previous_campaign.as_ref()
    }
}

fn is_committed(point: FailurePoint) -> bool {
    matches!(
        point,
        FailurePoint::LedgerCommittedBeforeJournal | FailurePoint::LedgerCommitted
    )
}

fn operation_files(store_root: &Path) -> Vec<PathBuf> {
    let root = store_root.join("save-operations");
    if !root.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    collect_files(&root, &mut files);
    files.sort();
    files
}

fn first_file(root: &Path) -> PathBuf {
    let mut files = Vec::new();
    collect_files(root, &mut files);
    files.sort();
    files.into_iter().next().expect("expected a staged file")
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        if metadata.file_type().is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn owner(campaign: Option<&ActiveCampaign>) -> SaveOwner {
    campaign
        .map(|campaign| SaveOwner::Package(campaign.id.clone()))
        .unwrap_or(SaveOwner::Plain)
}

fn save_transition(journal: &PendingOperation) -> SaveTransition {
    SaveTransition {
        previous_owner: owner(journal.previous_campaign.as_ref()),
        previous_faction: journal
            .previous_campaign
            .as_ref()
            .map(|campaign| campaign.faction),
        target_owner: owner(journal.target_campaign.as_ref()),
        target_faction: journal
            .target_campaign
            .as_ref()
            .map(|campaign| campaign.faction),
    }
}

#[test]
fn full_save_isolation_lifecycle_preserves_each_owner_and_play_is_a_noop() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    fixture.seed_plain();
    let vanilla = fixture.snapshot();
    let workflow = fixture.workflow(&store);

    workflow.activate(&fixture.a).unwrap();
    fixture.write_active_state("a", SlotId::LotV);
    let active_a = fixture.snapshot();
    workflow
        .play_with(&fixture.a, |_| {
            assert_eq!(fixture.snapshot(), active_a);
            Ok(())
        })
        .unwrap();
    assert_eq!(fixture.snapshot(), active_a);

    workflow.activate(&fixture.b).unwrap();
    fixture.write_active_state("b", SlotId::LotV);
    let active_b = fixture.snapshot();
    workflow.activate(&fixture.a).unwrap();
    assert_eq!(fixture.snapshot(), active_a);
    workflow.activate(&fixture.b).unwrap();
    assert_eq!(fixture.snapshot(), active_b);

    workflow.activate(&fixture.c).unwrap();
    fixture.write_active_state("c", SlotId::HotS);
    let active_c = fixture.snapshot();
    workflow.activate(&fixture.b).unwrap();
    assert_eq!(fixture.snapshot(), active_b);
    workflow.activate(&fixture.c).unwrap();
    assert_eq!(fixture.snapshot(), active_c);

    workflow.restore_vanilla().unwrap();
    assert_eq!(fixture.snapshot(), vanilla);
    assert!(store.active_campaign().unwrap().is_none());
    assert_eq!(workflow.health().state, HealthState::Ready);
}

#[test]
fn every_checkpoint_recovers_exact_saves_and_banks_for_every_transition_shape() {
    for scenario in Scenario::ALL {
        for point in FAILURE_POINTS {
            let fixture = Fixture::new();
            let store = fixture.open_store();
            let expected = prepare_scenario(&fixture, &store, scenario);
            invoke_interrupted(&fixture, &store, scenario, point);
            assert!(PendingOperation::load(store.root()).unwrap().is_some());
            drop(store);

            let reopened = fixture.open_store();
            let workflow = fixture.workflow(&reopened);
            workflow.recover_pending().unwrap();
            let committed = is_committed(point);
            assert_eq!(
                fixture.snapshot(),
                if committed {
                    expected.target.clone()
                } else {
                    expected.previous.clone()
                },
                "profile tree mismatch after {scenario:?} at {point:?}"
            );
            assert_eq!(
                reopened
                    .active_campaign()
                    .unwrap()
                    .as_ref()
                    .map(|campaign| &campaign.id),
                expected_active(&expected, committed),
                "ledger mismatch after {scenario:?} at {point:?}"
            );
            assert!(PendingOperation::load(reopened.root()).unwrap().is_none());
            assert!(
                operation_files(reopened.root()).is_empty(),
                "save artifacts remain after {scenario:?} at {point:?}"
            );
            assert_eq!(workflow.health().state, HealthState::Ready);
        }
    }
}

#[test]
fn restart_after_save_finalize_receipt_finishes_the_committed_workflow() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    let expected = prepare_scenario(&fixture, &store, Scenario::SameFaction);
    invoke_interrupted(
        &fixture,
        &store,
        Scenario::SameFaction,
        FailurePoint::LedgerCommitted,
    );
    let journal = PendingOperation::load(store.root()).unwrap().unwrap();
    let saves = fixture
        .saves_manager()
        .prepared(
            save_transition(&journal),
            &journal.operation_id,
            journal.paths.save_recovery_proof.clone().unwrap(),
        )
        .unwrap();
    saves.finalize().unwrap();
    assert!(!saves.paths().saves_backup.exists());
    assert_eq!(operation_files(store.root()).len(), 1);
    drop(store);

    let reopened = fixture.open_store();
    fixture.workflow(&reopened).recover_pending().unwrap();
    assert_eq!(fixture.snapshot(), expected.target);
    assert_eq!(reopened.active_campaign().unwrap().unwrap().id, fixture.b);
    assert!(PendingOperation::load(reopened.root()).unwrap().is_none());
    assert!(operation_files(reopened.root()).is_empty());
}

#[test]
fn tampered_save_recovery_path_is_rejected_before_any_external_mutation() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    fixture.seed_plain();
    let previous = fixture.snapshot();
    invoke_interrupted(
        &fixture,
        &store,
        Scenario::VanillaToA,
        FailurePoint::Prepared,
    );
    let outside = fixture
        ._temporary
        .path()
        .join("outside-do-not-touch/sentinel");
    touch(&outside, b"sentinel");
    let mut journal = PendingOperation::load(store.root()).unwrap().unwrap();
    journal.paths.saves_backup = Some(outside.parent().unwrap().to_path_buf());
    journal.persist(store.root()).unwrap();

    let error = fixture.workflow(&store).recover_pending().unwrap_err();
    assert_eq!(error.code(), "unsafe_operation_journal");
    assert_eq!(fixture.snapshot(), previous);
    assert_eq!(std::fs::read(&outside).unwrap(), b"sentinel");
    assert!(PendingOperation::load(store.root()).unwrap().is_some());
}

#[test]
fn changed_or_missing_save_backups_block_recovery_before_any_resource_changes() {
    for (point, banks, remove) in [
        (FailurePoint::SavesSwapped, false, false),
        (FailurePoint::SavesSwapped, true, true),
        (FailurePoint::ModsSwapped, false, true),
        (FailurePoint::ModsSwapped, true, false),
    ] {
        let fixture = Fixture::new();
        let store = fixture.open_store();
        fixture.seed_plain();
        invoke_interrupted(&fixture, &store, Scenario::VanillaToA, point);
        let journal = PendingOperation::load(store.root()).unwrap().unwrap();
        let backup = if banks {
            journal.paths.banks_backup.unwrap()
        } else {
            journal.paths.saves_backup.unwrap()
        };
        let file = first_file(&backup.join("live"));
        if remove {
            std::fs::remove_file(file).unwrap();
        } else {
            std::fs::write(file, b"substituted backup bytes").unwrap();
        }
        let live_before = fixture.snapshot();
        let game_before = fixture.game_snapshot();

        let error = fixture.workflow(&store).recover_pending().unwrap_err();

        assert!(
            matches!(
                error.code(),
                "save_recovery_proof_mismatch" | "missing_save_operation_artifact"
            ),
            "{point:?}, banks={banks}, remove={remove}: {}",
            error.code()
        );
        assert_eq!(fixture.snapshot(), live_before);
        assert_eq!(fixture.game_snapshot(), game_before);
        assert!(PendingOperation::load(store.root()).unwrap().is_some());
    }
}

#[test]
fn live_save_edits_after_the_swap_block_rollback_before_other_resources_change() {
    for banks in [false, true] {
        let fixture = Fixture::new();
        let store = fixture.open_store();
        fixture.seed_plain();
        invoke_interrupted(
            &fixture,
            &store,
            Scenario::VanillaToA,
            FailurePoint::SavesSwapped,
        );
        if banks {
            touch(
                &fixture.banks.join("author/after-crash.SC2Bank"),
                b"new bank bytes after the interrupted swap",
            );
        } else {
            touch(
                &fixture.saves.join("VoidCampaignSave.SC2Save"),
                b"new save bytes after the interrupted swap",
            );
        }
        let live_before = fixture.snapshot();
        let game_before = fixture.game_snapshot();

        let error = fixture.workflow(&store).recover_pending().unwrap_err();

        assert_eq!(error.code(), "save_recovery_proof_mismatch");
        assert_eq!(fixture.snapshot(), live_before);
        assert_eq!(fixture.game_snapshot(), game_before);
        assert!(PendingOperation::load(store.root()).unwrap().is_some());
    }
}

#[test]
fn changed_staged_archive_blocks_committed_recovery_before_cleanup() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    fixture.seed_plain();
    invoke_interrupted(
        &fixture,
        &store,
        Scenario::VanillaToA,
        FailurePoint::LedgerCommitted,
    );
    let journal = PendingOperation::load(store.root()).unwrap().unwrap();
    let staging = journal.paths.saves_staging.unwrap().join("set-updates");
    std::fs::write(first_file(&staging), b"changed staged archive").unwrap();
    let live_before = fixture.snapshot();
    let game_before = fixture.game_snapshot();

    let error = fixture.workflow(&store).recover_pending().unwrap_err();

    assert_eq!(error.code(), "save_recovery_proof_mismatch");
    assert_eq!(fixture.snapshot(), live_before);
    assert_eq!(fixture.game_snapshot(), game_before);
    assert!(PendingOperation::load(store.root()).unwrap().is_some());
}

#[test]
fn rollback_cleanup_rechecks_live_saves_before_deleting_other_backups() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    fixture.seed_plain();
    invoke_interrupted(
        &fixture,
        &store,
        Scenario::VanillaToA,
        FailurePoint::ModsSwapped,
    );
    let error = fixture
        .workflow(&store)
        .with_fail_after(FailurePoint::RollbackVerified)
        .recover_pending()
        .unwrap_err();
    assert_eq!(error.code(), "simulated_interruption");
    assert_eq!(
        PendingOperation::load(store.root()).unwrap().unwrap().phase,
        svccm_core::operation::OperationPhase::RollbackVerified
    );
    touch(
        &fixture.saves.join("VoidCampaignSave.SC2Save"),
        b"changed after rollback verification",
    );
    let live_before = fixture.snapshot();
    let game_before = fixture.game_snapshot();

    let error = fixture.workflow(&store).recover_pending().unwrap_err();

    assert_eq!(error.code(), "save_verification_failed");
    assert_eq!(fixture.snapshot(), live_before);
    assert_eq!(fixture.game_snapshot(), game_before);
    assert!(PendingOperation::load(store.root()).unwrap().is_some());
}

#[test]
fn malformed_save_proof_is_rejected_before_recovery_mutates_resources() {
    let fixture = Fixture::new();
    let store = fixture.open_store();
    fixture.seed_plain();
    invoke_interrupted(
        &fixture,
        &store,
        Scenario::VanillaToA,
        FailurePoint::Prepared,
    );
    let mut journal = PendingOperation::load(store.root()).unwrap().unwrap();
    journal
        .paths
        .save_recovery_proof
        .as_mut()
        .unwrap()
        .transition_sha256 = "not-a-sha256".into();
    journal.persist(store.root()).unwrap();
    let live_before = fixture.snapshot();
    let game_before = fixture.game_snapshot();

    let error = fixture.workflow(&store).recover_pending().unwrap_err();

    assert_eq!(error.code(), "invalid_save_recovery_proof");
    assert_eq!(fixture.snapshot(), live_before);
    assert_eq!(fixture.game_snapshot(), game_before);
    assert!(PendingOperation::load(store.root()).unwrap().is_some());
}

#[test]
fn committed_receipt_and_archives_remain_bound_to_the_atomic_journal() {
    for tamper_receipt in [true, false] {
        let fixture = Fixture::new();
        let store = fixture.open_store();
        fixture.seed_plain();
        invoke_interrupted(
            &fixture,
            &store,
            Scenario::VanillaToA,
            FailurePoint::LedgerCommitted,
        );
        let journal = PendingOperation::load(store.root()).unwrap().unwrap();
        let saves = fixture
            .saves_manager()
            .prepared(
                save_transition(&journal),
                &journal.operation_id,
                journal.paths.save_recovery_proof.clone().unwrap(),
            )
            .unwrap();
        saves.finalize().unwrap();
        if tamper_receipt {
            let receipt = store
                .root()
                .join("save-operations/receipts")
                .join(format!("{}.json", journal.operation_id));
            std::fs::write(receipt, b"{}").unwrap();
        } else {
            std::fs::write(
                store
                    .root()
                    .join("saves/v2/plain/global/Campaign/plain.SC2Save"),
                b"changed committed archive",
            )
            .unwrap();
        }
        let live_before = fixture.snapshot();
        let game_before = fixture.game_snapshot();

        let error = fixture.workflow(&store).recover_pending().unwrap_err();

        assert_eq!(
            error.code(),
            if tamper_receipt {
                "invalid_save_commit_receipt"
            } else {
                "committed_save_archives_drifted"
            }
        );
        assert_eq!(fixture.snapshot(), live_before);
        assert_eq!(fixture.game_snapshot(), game_before);
        assert!(PendingOperation::load(store.root()).unwrap().is_some());
    }
}
