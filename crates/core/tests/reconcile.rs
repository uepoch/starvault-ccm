//! Rollback helpers used by startup journal recovery.

use std::path::Path;

use svccm_core::config::StrategyChoice;
use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::mods::{rollback_from_paths as rollback_mods, PreparedModsTransition};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::slots::{rollback_paths_checked, SlotManager};
use svccm_core::store::Store;

fn make_source(root: &Path) {
    let map = root.join("Maps/campaign/recovery.SC2Map");
    std::fs::create_dir_all(&map).unwrap();
    std::fs::write(map.join("payload"), b"map").unwrap();
    std::fs::create_dir_all(root.join("Mods/Recovery.SC2Mod")).unwrap();
    std::fs::write(root.join("Mods/Recovery.SC2Mod/payload"), b"mod").unwrap();
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) {
    junction::create(target, link).unwrap();
}

#[test]
fn slot_rollback_can_resume_from_journal_paths_and_manifest_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_source(&source);
    let id = PackageId::parse("recovery").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));
    let transition = manager
        .prepare(None, Some(&manifest), "crashed-slot")
        .unwrap();
    let paths = transition.journal_paths();
    transition.apply().unwrap();
    assert!(layout
        .slot_dir(SlotId::LotV)
        .join("recovery.SC2Map/payload")
        .is_file());
    drop(transition);

    rollback_paths_checked(&paths, None, Some(&manifest)).unwrap();
    assert!(layout.slot_dir(SlotId::LotV).symlink_metadata().is_err());
    assert!(paths
        .iter()
        .all(|paths| !paths.staging.exists() && !paths.backup.exists()));
}

#[cfg(any(unix, windows))]
#[test]
fn wol_rollback_rejects_a_linked_backup_before_external_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_source(&source);
    let id = PackageId::parse("recovery").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::Wol, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));
    let transition = manager
        .prepare(None, Some(&manifest), "linked-wol-backup")
        .unwrap();
    let paths = transition.journal_paths();
    transition.apply().unwrap();

    std::fs::remove_dir_all(&paths[0].backup).unwrap();
    let external = temp.path().join("external-backup");
    let sentinel = external.join("sentinel.txt");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(&sentinel, b"outside").unwrap();
    create_directory_link(&external, &paths[0].backup);

    let error = rollback_paths_checked(&paths, None, Some(&manifest)).unwrap_err();
    assert_eq!(error.code(), "unsafe_slot_artifact");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
    assert!(layout
        .slot_dir(SlotId::Wol)
        .join("recovery.SC2Map/payload")
        .is_file());
}

#[test]
fn mods_rollback_can_resume_from_only_journal_paths() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_source(&source);
    let id = PackageId::parse("recovery").unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, SlotId::LotV, &plan).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let transition = PreparedModsTransition::prepare(
        &store,
        &layout.mods_dir(),
        &[],
        Some(&manifest),
        "crashed-mods",
    )
    .unwrap();
    let staging = transition.staging_path();
    let backup = transition.backup_path();
    transition.apply().unwrap();
    assert!(layout.mods_dir().join("Recovery.SC2Mod/payload").is_file());
    drop(transition);

    rollback_mods(&layout.mods_dir(), &backup, &staging).unwrap();
    assert!(!layout.mods_dir().join("Recovery.SC2Mod").exists());
    assert!(!staging.exists());
    assert!(!backup.exists());
}
