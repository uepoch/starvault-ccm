//! Windows-only coverage for a real NTFS junction transition and rollback.

#![cfg(windows)]

use std::path::Path;

use svccm_core::config::StrategyChoice;
use svccm_core::contracts::HealthState;
use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::operation::PendingOperation;
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::slots::{rollback_repair_paths_checked, SlotManager};
use svccm_core::store::Store;
use svccm_core::workflow::Workflow;

fn make_map_container(directory: &Path) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("payload.txt"), b"map").unwrap();
}

#[test]
fn mutation_roots_reject_junctions_at_the_root_or_shared_ancestors() {
    fn assert_rejected(layout: &WindowsLayout, expected_path: &Path) {
        let error = layout.validate_mutation_roots().unwrap_err();
        assert_eq!(error.code(), "unsafe_game_layout");
        assert_eq!(error.path(), Some(expected_path));
    }

    let temp = tempfile::tempdir().unwrap();
    let external_root = temp.path().join("external-root");
    std::fs::create_dir_all(&external_root).unwrap();
    let linked_root = temp.path().join("linked-root");
    junction::create(&external_root, &linked_root).unwrap();
    assert_rejected(&WindowsLayout::new(&linked_root), &linked_root);

    for component in ["Maps", "Campaign", "Mods"] {
        let root = temp.path().join(format!("sc2-{component}"));
        std::fs::create_dir_all(&root).unwrap();
        let external = temp.path().join(format!("external-{component}"));
        std::fs::create_dir_all(&external).unwrap();
        let linked = match component {
            "Maps" => root.join("Maps"),
            "Campaign" => {
                std::fs::create_dir(root.join("Maps")).unwrap();
                root.join("Maps/Campaign")
            }
            "Mods" => root.join("Mods"),
            _ => unreachable!(),
        };
        junction::create(&external, &linked).unwrap();
        assert_rejected(&WindowsLayout::new(&root), &linked);
    }
}

#[test]
fn mutation_roots_allow_a_dedicated_slot_junction() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    std::fs::create_dir_all(layout.slot_dir(SlotId::Wol)).unwrap();
    std::fs::create_dir_all(layout.mods_dir()).unwrap();
    let deployment = temp.path().join("deployment");
    std::fs::create_dir_all(&deployment).unwrap();
    junction::create(&deployment, layout.slot_dir(SlotId::LotV)).unwrap();

    layout.validate_mutation_roots().unwrap();
}

#[test]
fn junction_transition_reads_through_and_rollback_restores_plain_slot() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    std::fs::create_dir_all(layout.slot_dir(SlotId::LotV)).unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map_container(&source.join("Maps/campaign/tarcade.SC2Map"));
    let id = PackageId::parse("tarcade").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();

    let manager = SlotManager::new(&layout, &store);
    let transition = manager
        .prepare(None, Some(&manifest), "junction-activation")
        .unwrap();
    transition.apply().unwrap();
    manager.verify_target(&transition).unwrap();

    let slot = layout.slot_dir(SlotId::LotV);
    assert!(
        std::fs::symlink_metadata(&slot)
            .unwrap()
            .file_type()
            .is_symlink(),
        "slot should be an NTFS junction"
    );
    assert!(slot.join("tarcade.SC2Map/payload.txt").is_file());
    manager.verify_current(Some(&manifest)).unwrap();

    transition.rollback().unwrap();
    assert!(!std::fs::symlink_metadata(&slot)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_dir(slot).unwrap().next().is_none());
}

#[test]
fn dangling_owned_junction_repairs_to_a_verified_copy_and_can_restore_vanilla() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    std::fs::create_dir_all(layout.root()).unwrap();
    std::fs::write(layout.exe(), b"fake executable").unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map_container(&source.join("Maps/campaign/tarcade.SC2Map"));
    let id = PackageId::parse("tarcade").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let workflow = || {
        Workflow::new(&layout, &store)
            .with_strategy(Some(StrategyChoice::Junction))
            .with_running_probe(|| false)
    };

    workflow().activate(&id).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let deployed = store
        .deploy_dir(manifest.faction, &manifest.revision)
        .unwrap();
    let live = layout.slot_dir(SlotId::LotV);
    assert!(live.symlink_metadata().unwrap().file_type().is_symlink());
    std::fs::remove_dir_all(&deployed).unwrap();
    assert_eq!(std::fs::read_link(&live).unwrap(), deployed);

    let health = workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues[0].code, "slot_drift");
    assert!(health.issues[0].repairable);
    let error = workflow().restore_vanilla().unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert!(live.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(PendingOperation::load(store.root()).unwrap().is_none());

    workflow().repair_active().unwrap();
    assert!(!live.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        std::fs::read(live.join("tarcade.SC2Map/payload.txt")).unwrap(),
        b"map"
    );
    assert_eq!(workflow().health().state, HealthState::Ready);

    workflow().restore_vanilla().unwrap();
    assert!(store.active_campaign().unwrap().is_none());
    assert!(std::fs::read_dir(&live).unwrap().next().is_none());
}

#[test]
fn active_verification_rejects_an_identical_external_junction() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map_container(&source.join("Maps/campaign/tarcade.SC2Map"));
    let id = PackageId::parse("tarcade").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let manager = SlotManager::new(&layout, &store);

    let activation = manager
        .prepare(None, Some(&manifest), "external-live-activation")
        .unwrap();
    activation.apply().unwrap();
    activation.finalize().unwrap();
    let live = layout.slot_dir(SlotId::LotV);
    std::fs::remove_dir(&live).unwrap();
    let external = temp.path().join("external-identical-slot");
    store.materialize_slot(&manifest, &external).unwrap();
    junction::create(&external, &live).unwrap();

    let error = manager.verify_current(Some(&manifest)).unwrap_err();
    assert_eq!(error.code(), "unowned_campaign_slot_link");
    assert_eq!(
        std::fs::read(external.join("tarcade.SC2Map/payload.txt")).unwrap(),
        b"map"
    );
    assert!(std::fs::symlink_metadata(live)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn ordinary_rollback_rejects_a_junction_with_identical_backup_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map_container(&source.join("Maps/campaign/tarcade.SC2Map"));
    let id = PackageId::parse("tarcade").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let manager = SlotManager::new(&layout, &store);

    let activation = manager
        .prepare(None, Some(&manifest), "backup-substitution-activation")
        .unwrap();
    activation.apply().unwrap();
    activation.finalize().unwrap();
    let restore = manager
        .prepare(Some(&manifest), None, "backup-substitution-restore")
        .unwrap();
    let paths = restore.journal_paths();
    restore.apply().unwrap();
    assert!(std::fs::read_dir(&paths[0].live).unwrap().next().is_none());

    std::fs::remove_dir(&paths[0].backup).unwrap();
    let external = temp.path().join("external-identical-backup");
    store.materialize_slot(&manifest, &external).unwrap();
    junction::create(&external, &paths[0].backup).unwrap();

    let error = restore.rollback().unwrap_err();
    assert_eq!(error.code(), "unsafe_slot_artifact");
    assert!(std::fs::read_dir(&paths[0].live).unwrap().next().is_none());
    assert_eq!(
        std::fs::read(external.join("tarcade.SC2Map/payload.txt")).unwrap(),
        b"map"
    );
}

#[test]
fn wol_live_junction_is_rejected_without_touching_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map_container(&source.join("Maps/campaign/wol-alpha.SC2Map"));
    let id = PackageId::parse("wol-alpha").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::Wol, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();

    let external = temp.path().join("external-campaign");
    std::fs::create_dir_all(&external).unwrap();
    let sentinel = external.join("sentinel.txt");
    std::fs::write(&sentinel, b"outside").unwrap();
    let live = layout.slot_dir(SlotId::Wol);
    std::fs::create_dir_all(live.parent().unwrap()).unwrap();
    junction::create(&external, &live).unwrap();
    let manager = SlotManager::new(&layout, &store);

    let error = manager
        .prepare(None, Some(&manifest), "linked-wol-activation")
        .unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    let error = manager
        .prepare_repair(&manifest, "linked-wol-repair")
        .unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
    assert!(std::fs::symlink_metadata(live)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn dedicated_repair_rollback_restores_the_expected_starvault_junction() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map_container(&source.join("Maps/campaign/tarcade.SC2Map"));
    let id = PackageId::parse("tarcade").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let manager = SlotManager::new(&layout, &store);

    let activation = manager
        .prepare(None, Some(&manifest), "junction-repair-activation")
        .unwrap();
    activation.apply().unwrap();
    activation.finalize().unwrap();
    let live = layout.slot_dir(SlotId::LotV);
    let payload = live.join("tarcade.SC2Map/payload.txt");
    std::fs::write(&payload, b"user-modified").unwrap();

    let repair = manager
        .prepare_repair(&manifest, "junction-repair")
        .unwrap();
    let paths = repair.journal_paths();
    repair.apply().unwrap();
    assert!(!std::fs::symlink_metadata(&live)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(&payload).unwrap(), b"map");

    rollback_repair_paths_checked(&paths, Some(&manifest)).unwrap();
    assert!(std::fs::symlink_metadata(&live)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(payload).unwrap(), b"user-modified");
}

#[test]
fn dedicated_repair_rollback_rejects_a_substituted_junction() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map_container(&source.join("Maps/campaign/tarcade.SC2Map"));
    let id = PackageId::parse("tarcade").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let manager = SlotManager::new(&layout, &store);

    let activation = manager
        .prepare(None, Some(&manifest), "substitution-activation")
        .unwrap();
    activation.apply().unwrap();
    activation.finalize().unwrap();
    let repair = manager
        .prepare_repair(&manifest, "substitution-repair")
        .unwrap();
    let paths = repair.journal_paths();
    repair.apply().unwrap();

    std::fs::remove_dir(&paths[0].backup).unwrap();
    let external = temp.path().join("external-junction-target");
    let sentinel = external.join("sentinel.txt");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(&sentinel, b"outside").unwrap();
    junction::create(&external, &paths[0].backup).unwrap();

    let error = rollback_repair_paths_checked(&paths, Some(&manifest)).unwrap_err();
    assert_eq!(error.code(), "unsafe_slot_artifact");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
    assert_eq!(
        std::fs::read(
            layout
                .slot_dir(SlotId::LotV)
                .join("tarcade.SC2Map/payload.txt")
        )
        .unwrap(),
        b"map"
    );
}
