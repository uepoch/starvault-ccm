//! Campaign-root transition integration tests.

use std::path::Path;

use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::slots::{rollback_paths_checked, SlotManager};
use svccm_core::store::{PackageManifest, Store};

fn make_map_container(directory: &Path, payload: &[u8]) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("payload.bin"), payload).unwrap();
}

fn ingest(
    store: &Store,
    sources: &Path,
    id: &str,
    faction: SlotId,
    payload: &[u8],
) -> PackageManifest {
    let source = sources.join(id);
    make_map_container(&source.join(format!("Maps/Campaign/{id}.SC2Map")), payload);
    let id = PackageId::parse(id).unwrap();
    store
        .ingest(&id, faction, &plan_from_extracted(&source).unwrap())
        .unwrap();
    store.load_manifest(&id).unwrap()
}

fn deploy_initial(manager: &SlotManager<'_>, manifest: &PackageManifest, operation_id: &str) {
    let transition = manager.prepare(None, Some(manifest), operation_id).unwrap();
    transition.apply().unwrap();
    transition.finalize().unwrap();
    manager.verify_target(&transition).unwrap();
}

fn assert_link(path: &Path) {
    assert!(std::fs::symlink_metadata(path)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn every_faction_uses_one_campaign_root_link() {
    for (faction, prefix) in [
        (SlotId::Wol, ""),
        (SlotId::HotS, "swarm/"),
        (SlotId::LotV, "void/"),
        (SlotId::Nco, "nova/"),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temporary.path().join("sc2"));
        let store = Store::open_for_tests(temporary.path().join("store")).unwrap();
        let manifest = ingest(
            &store,
            &temporary.path().join("sources"),
            "campaign",
            faction,
            b"payload",
        );
        let manager = SlotManager::new(&layout, &store);

        deploy_initial(&manager, &manifest, "initial");

        assert_link(&layout.campaign_dir());
        assert!(layout
            .campaign_dir()
            .join(format!("{prefix}campaign.SC2Map/payload.bin"))
            .is_file());
        assert!(layout.plain_campaign_dir().is_dir());
        for inactive in ["swarm", "void", "voidprologue", "nova"] {
            let path = layout.campaign_dir().join(inactive);
            assert!(path.is_dir());
            if inactive != prefix.trim_end_matches('/') {
                assert!(std::fs::read_dir(path).unwrap().next().is_none());
            }
        }
    }
}

#[test]
fn activation_and_restore_preserve_the_exact_plain_override_tree() {
    let temporary = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temporary.path().join("sc2"));
    let store = Store::open_for_tests(temporary.path().join("store")).unwrap();
    let plain_file = layout.campaign_dir().join("external/readme.txt");
    std::fs::create_dir_all(plain_file.parent().unwrap()).unwrap();
    std::fs::write(&plain_file, b"keep").unwrap();
    let manifest = ingest(
        &store,
        &temporary.path().join("sources"),
        "alpha",
        SlotId::Wol,
        b"alpha",
    );
    let manager = SlotManager::new(&layout, &store);

    deploy_initial(&manager, &manifest, "activate-alpha");
    assert_eq!(
        std::fs::read(layout.plain_campaign_dir().join("external/readme.txt")).unwrap(),
        b"keep"
    );
    assert!(!layout.campaign_dir().join("external/readme.txt").exists());

    let restore = manager
        .prepare(Some(&manifest), None, "restore-alpha")
        .unwrap();
    restore.apply().unwrap();
    restore.finalize().unwrap();
    manager.verify_current(None).unwrap();

    assert!(!layout.plain_campaign_dir().exists());
    assert_eq!(std::fs::read(plain_file).unwrap(), b"keep");
    assert!(!std::fs::symlink_metadata(layout.campaign_dir())
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn cross_faction_switch_has_one_path_and_rolls_back_exactly() {
    let temporary = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temporary.path().join("sc2"));
    let store = Store::open_for_tests(temporary.path().join("store")).unwrap();
    let sources = temporary.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::LotV, b"alpha");
    let beta = ingest(&store, &sources, "beta", SlotId::Nco, b"beta");
    let manager = SlotManager::new(&layout, &store);

    deploy_initial(&manager, &alpha, "initial-alpha");
    let transition = manager
        .prepare(Some(&alpha), Some(&beta), "alpha-to-beta")
        .unwrap();
    let journal = transition.journal_paths();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].live, layout.campaign_dir());
    assert!(journal[0]
        .staging
        .join("nova/beta.SC2Map/payload.bin")
        .is_file());

    transition.apply().unwrap();
    manager.verify_target(&transition).unwrap();
    assert!(layout
        .campaign_dir()
        .join("nova/beta.SC2Map/payload.bin")
        .is_file());
    assert!(std::fs::read_dir(layout.campaign_dir().join("void"))
        .unwrap()
        .next()
        .is_none());

    transition.rollback().unwrap();
    manager.verify_current(Some(&alpha)).unwrap();
    assert_eq!(
        std::fs::read(layout.campaign_dir().join("void/alpha.SC2Map/payload.bin")).unwrap(),
        b"alpha"
    );
}

#[test]
fn rollback_refuses_a_changed_live_target_and_preserves_the_backup() {
    let temporary = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temporary.path().join("sc2"));
    let store = Store::open_for_tests(temporary.path().join("store")).unwrap();
    let sources = temporary.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::Wol, b"alpha");
    let beta = ingest(&store, &sources, "beta", SlotId::Wol, b"beta");
    let manager = SlotManager::new(&layout, &store);
    deploy_initial(&manager, &alpha, "initial-alpha");

    let transition = manager
        .prepare(Some(&alpha), Some(&beta), "alpha-to-beta")
        .unwrap();
    let journal = transition.journal_paths();
    transition.apply().unwrap();
    remove_link(&layout.campaign_dir());
    std::fs::create_dir_all(layout.campaign_dir()).unwrap();
    std::fs::write(layout.campaign_dir().join("unknown"), b"preserve").unwrap();

    let error = rollback_paths_checked(&journal, Some(&alpha), Some(&beta)).unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(
        std::fs::read(layout.campaign_dir().join("unknown")).unwrap(),
        b"preserve"
    );
    assert!(journal[0].backup.symlink_metadata().is_ok());
}

#[test]
fn unknown_campaign_root_link_is_never_followed() {
    let temporary = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temporary.path().join("sc2"));
    let store = Store::open_for_tests(temporary.path().join("store")).unwrap();
    let external = temporary.path().join("external");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    std::fs::create_dir_all(layout.campaign_dir().parent().unwrap()).unwrap();
    create_link(&external, &layout.campaign_dir());
    let manifest = ingest(
        &store,
        &temporary.path().join("sources"),
        "alpha",
        SlotId::Wol,
        b"alpha",
    );

    let error = SlotManager::new(&layout, &store)
        .prepare(None, Some(&manifest), "unsafe-link")
        .unwrap_err();
    assert_eq!(error.code(), "unowned_campaign_slot_link");
    assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
}

#[cfg(unix)]
fn create_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_link(target: &Path, link: &Path) {
    junction::create(target, link).unwrap();
}

#[cfg(unix)]
fn remove_link(link: &Path) {
    std::fs::remove_file(link).unwrap();
}

#[cfg(windows)]
fn remove_link(link: &Path) {
    std::fs::remove_dir(link).unwrap();
}
