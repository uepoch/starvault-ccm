//! Windows-only coverage for the single `Maps\\Campaign` NTFS junction.

#![cfg(windows)]

use std::path::Path;

use svccm_core::config::StrategyChoice;
use svccm_core::contracts::HealthState;
use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::operation::PendingOperation;
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::slots::SlotManager;
use svccm_core::store::Store;
use svccm_core::workflow::Workflow;

fn make_map_container(directory: &Path, payload: &[u8]) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("payload.txt"), payload).unwrap();
}

fn ingest(store: &Store, source: &Path, id: &str, faction: SlotId) -> PackageId {
    make_map_container(
        &source.join(format!("Maps/Campaign/{id}.SC2Map")),
        id.as_bytes(),
    );
    let id = PackageId::parse(id).unwrap();
    store
        .ingest(&id, faction, &plan_from_extracted(source).unwrap())
        .unwrap();
    id
}

fn assert_junction(path: &Path) {
    assert!(
        std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink(),
        "{} should be an NTFS junction",
        path.display()
    );
}

#[test]
fn mutation_roots_reject_linked_parents_but_allow_the_campaign_junction() {
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

    for component in ["Maps", "Mods"] {
        let root = temp.path().join(format!("sc2-{component}"));
        std::fs::create_dir_all(&root).unwrap();
        let external = temp.path().join(format!("external-{component}"));
        std::fs::create_dir_all(&external).unwrap();
        let linked = root.join(component);
        junction::create(&external, &linked).unwrap();
        assert_rejected(&WindowsLayout::new(&root), &linked);
    }

    let layout = WindowsLayout::new(temp.path().join("allowed-campaign"));
    std::fs::create_dir_all(layout.root().join("Maps")).unwrap();
    std::fs::create_dir_all(layout.mods_dir()).unwrap();
    let deployment = temp.path().join("deployment");
    std::fs::create_dir_all(&deployment).unwrap();
    junction::create(&deployment, layout.campaign_dir()).unwrap();
    layout.validate_mutation_roots().unwrap();
}

#[test]
fn every_faction_is_exposed_through_one_campaign_root_junction() {
    for (faction, relative) in [
        (SlotId::Wol, "campaign.SC2Map/payload.txt"),
        (SlotId::HotS, "swarm/campaign.SC2Map/payload.txt"),
        (SlotId::LotV, "void/campaign.SC2Map/payload.txt"),
        (SlotId::Nco, "nova/campaign.SC2Map/payload.txt"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temp.path().join("sc2"));
        let store = Store::open_for_tests(temp.path().join("store")).unwrap();
        let source = temp.path().join("source");
        let id = ingest(&store, &source, "campaign", faction);
        let manifest = store.load_manifest(&id).unwrap();
        let manager =
            SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Junction));

        let transition = manager
            .prepare(None, Some(&manifest), "junction-activation")
            .unwrap();
        transition.apply().unwrap();
        transition.finalize().unwrap();
        manager.verify_current(Some(&manifest)).unwrap();

        assert_junction(&layout.campaign_dir());
        assert!(layout.campaign_dir().join(relative).is_file());
        assert!(layout.plain_campaign_dir().is_dir());
        for directory in ["swarm", "void", "voidprologue", "nova"] {
            assert!(layout.campaign_dir().join(directory).is_dir());
        }
    }
}

#[test]
fn activation_and_restore_preserve_the_plain_override_tree() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let plain = layout.campaign_dir().join("external/readme.txt");
    std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
    std::fs::write(&plain, b"keep").unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    let id = ingest(&store, &source, "tarcade", SlotId::LotV);
    let manifest = store.load_manifest(&id).unwrap();
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Junction));

    let activation = manager
        .prepare(None, Some(&manifest), "plain-activation")
        .unwrap();
    activation.apply().unwrap();
    activation.finalize().unwrap();
    assert_junction(&layout.campaign_dir());
    assert_eq!(
        std::fs::read(layout.plain_campaign_dir().join("external/readme.txt")).unwrap(),
        b"keep"
    );

    let restore = manager
        .prepare(Some(&manifest), None, "plain-restore")
        .unwrap();
    restore.apply().unwrap();
    restore.finalize().unwrap();
    manager.verify_current(None).unwrap();

    assert_eq!(std::fs::read(&plain).unwrap(), b"keep");
    assert!(!layout.plain_campaign_dir().exists());
    assert!(!std::fs::symlink_metadata(layout.campaign_dir())
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn dangling_owned_root_junction_can_be_restored() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    std::fs::create_dir_all(layout.root()).unwrap();
    std::fs::write(layout.exe(), b"fake executable").unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    let id = ingest(&store, &source, "tarcade", SlotId::LotV);
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
    assert_junction(&layout.campaign_dir());
    std::fs::remove_dir_all(&deployed).unwrap();

    let health = workflow().health();
    assert_eq!(health.state, HealthState::Drifted);
    assert_eq!(health.issues[0].code, "slot_drift");
    workflow().restore_vanilla().unwrap();
    assert!(store.active_campaign().unwrap().is_none());
}

#[test]
fn a_foreign_campaign_root_junction_is_never_followed() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    let id = ingest(&store, &source, "wol-alpha", SlotId::Wol);
    let manifest = store.load_manifest(&id).unwrap();
    let external = temp.path().join("external-campaign");
    std::fs::create_dir_all(&external).unwrap();
    let sentinel = external.join("sentinel.txt");
    std::fs::write(&sentinel, b"outside").unwrap();
    std::fs::create_dir_all(layout.campaign_dir().parent().unwrap()).unwrap();
    junction::create(&external, layout.campaign_dir()).unwrap();
    let manager = SlotManager::new(&layout, &store);

    let error = manager
        .prepare(None, Some(&manifest), "foreign-activation")
        .unwrap_err();
    assert_eq!(error.code(), "unowned_campaign_slot_link");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
    assert_junction(&layout.campaign_dir());
}

#[test]
fn activation_reuses_a_complete_deployment_without_opening_every_map_file() {
    use std::os::windows::fs::OpenOptionsExt;

    const SHARE_NONE: u32 = 0;

    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    std::fs::create_dir_all(layout.root()).unwrap();
    std::fs::write(layout.exe(), b"fake executable").unwrap();
    let store_root = temp.path().join("store");
    let store = Store::open_for_tests(&store_root).unwrap();
    let source = temp.path().join("source");
    let id = ingest(&store, &source, "locked-map", SlotId::Wol);
    let workflow = || {
        Workflow::new(&layout, &store)
            .with_strategy(Some(StrategyChoice::Junction))
            .with_running_probe(|| false)
    };

    workflow().activate(&id).unwrap();
    workflow().restore_vanilla().unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let deployment_file = store
        .deploy_dir(manifest.faction, &manifest.revision)
        .unwrap()
        .join("locked-map.SC2Map/payload.txt");
    drop(store);

    let reopened = Store::open_for_tests(&store_root).unwrap();
    let _lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_NONE)
        .open(&deployment_file)
        .unwrap();
    let active = Workflow::new(&layout, &reopened)
        .with_strategy(Some(StrategyChoice::Junction))
        .with_running_probe(|| false)
        .activate(&id)
        .unwrap();

    assert_eq!(active.id, id);
    assert_junction(&layout.campaign_dir());
}
