//! Reversible campaign-slot transition integration tests.

use std::path::Path;

use svccm_core::config::StrategyChoice;
use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::slots::{
    rollback_paths_checked, rollback_repair_paths_checked, verify_repair_rollback_paths,
    SlotManager,
};
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
    make_map_container(&source.join(format!("Maps/campaign/{id}.SC2Map")), payload);
    let id = PackageId::parse(id).unwrap();
    let plan = plan_from_extracted(&source).unwrap();
    store.ingest(&id, faction, &plan).unwrap();
    store.load_manifest(&id).unwrap()
}

fn deploy_initial(manager: &SlotManager<'_>, manifest: &PackageManifest, operation_id: &str) {
    let transition = manager.prepare(None, Some(manifest), operation_id).unwrap();
    transition.apply().unwrap();
    manager.verify_target(&transition).unwrap();
    transition.finalize().unwrap();
}

#[test]
fn same_faction_transition_stages_target_and_rolls_back_to_previous() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::LotV, b"alpha");
    let beta = ingest(&store, &sources, "beta", SlotId::LotV, b"beta");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-alpha");
    let transition = manager
        .prepare(Some(&alpha), Some(&beta), "alpha-to-beta")
        .unwrap();
    let paths = transition.journal_paths();
    assert_eq!(paths.len(), 1);
    assert!(paths[0].staging.join("beta.SC2Map/payload.bin").is_file());

    transition.apply().unwrap();
    manager.verify_target(&transition).unwrap();
    assert_eq!(
        std::fs::read(
            layout
                .slot_dir(SlotId::LotV)
                .join("beta.SC2Map/payload.bin")
        )
        .unwrap(),
        b"beta"
    );

    transition.rollback().unwrap();
    manager.verify_current(Some(&alpha)).unwrap();
    assert_eq!(
        std::fs::read(
            layout
                .slot_dir(SlotId::LotV)
                .join("alpha.SC2Map/payload.bin")
        )
        .unwrap(),
        b"alpha"
    );
    assert!(!layout.slot_dir(SlotId::LotV).join("beta.SC2Map").exists());
}

#[test]
fn cross_faction_transition_stages_both_slots_and_rolls_back_exactly() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::LotV, b"alpha");
    let beta = ingest(&store, &sources, "beta", SlotId::Nco, b"beta");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-alpha");
    let transition = manager
        .prepare(Some(&alpha), Some(&beta), "lotv-to-nco")
        .unwrap();
    let paths = transition.journal_paths();
    assert_eq!(paths.len(), 2);
    let old = paths
        .iter()
        .find(|paths| paths.faction == SlotId::LotV)
        .unwrap();
    let target = paths
        .iter()
        .find(|paths| paths.faction == SlotId::Nco)
        .unwrap();
    assert!(std::fs::read_dir(&old.staging).unwrap().next().is_none());
    assert!(target.staging.join("beta.SC2Map/payload.bin").is_file());

    transition.apply().unwrap();
    manager.verify_target(&transition).unwrap();
    assert!(std::fs::read_dir(layout.slot_dir(SlotId::LotV))
        .unwrap()
        .next()
        .is_none());
    assert!(layout
        .slot_dir(SlotId::Nco)
        .join("beta.SC2Map/payload.bin")
        .is_file());

    transition.rollback().unwrap();
    manager.verify_current(Some(&alpha)).unwrap();
    assert!(layout
        .slot_dir(SlotId::LotV)
        .join("alpha.SC2Map/payload.bin")
        .is_file());
    assert!(layout.slot_dir(SlotId::Nco).symlink_metadata().is_err());
}

#[test]
fn active_verification_rejects_a_stale_unrelated_faction() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::LotV, b"alpha");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-alpha");
    let stale = layout.slot_dir(SlotId::HotS).join("stale.SC2Map/payload");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, b"stale").unwrap();

    let error = manager.verify_current(Some(&alpha)).unwrap_err();
    assert_eq!(error.code(), "unowned_campaign_files");
    assert_eq!(std::fs::read(stale).unwrap(), b"stale");
}

#[cfg(unix)]
#[test]
fn active_verification_accepts_the_exact_owned_deployment_link() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::LotV, b"alpha");
    let deployed = store.deploy_dir(alpha.faction, &alpha.revision).unwrap();
    store.materialize_slot(&alpha, &deployed).unwrap();
    let live = layout.slot_dir(SlotId::LotV);
    std::fs::create_dir_all(live.parent().unwrap()).unwrap();
    symlink(&deployed, &live).unwrap();

    let manager = SlotManager::new(&layout, &store);
    manager.verify_current(Some(&alpha)).unwrap();
    assert_eq!(
        std::fs::read(live.join("alpha.SC2Map/payload.bin")).unwrap(),
        b"alpha"
    );
}

#[test]
fn wol_live_marker_and_operation_substrings_are_not_hidden() {
    for name in [
        ".starvault-backup-ready",
        "rogue.backup-op",
        "rogue.staging-op",
        "void.user.backup-op",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temp.path().join("sc2"));
        let store = Store::open_for_tests(temp.path().join("store")).unwrap();
        let entry = layout.slot_dir(SlotId::Wol).join(name);
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, b"user content").unwrap();
        let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

        let error = manager.verify_current(None).unwrap_err();
        assert_eq!(error.code(), "unowned_campaign_files", "{name}");
        assert_eq!(std::fs::read(entry).unwrap(), b"user content");
    }
}

#[test]
fn wol_transition_preserves_game_owned_sibling_campaigns() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let sibling_marker = layout.slot_dir(SlotId::HotS).join("vanilla-marker");
    std::fs::create_dir_all(sibling_marker.parent().unwrap()).unwrap();
    std::fs::write(&sibling_marker, b"hots").unwrap();
    let campaign = ingest(&store, &sources, "wol-alpha", SlotId::Wol, b"wol");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    let transition = manager
        .prepare(None, Some(&campaign), "activate-wol")
        .unwrap();
    assert!(transition.journal_paths()[0]
        .staging
        .join("wol-alpha.SC2Map/payload.bin")
        .is_file());
    transition.apply().unwrap();
    manager.verify_target(&transition).unwrap();
    assert_eq!(std::fs::read(&sibling_marker).unwrap(), b"hots");

    transition.rollback().unwrap();
    assert_eq!(std::fs::read(&sibling_marker).unwrap(), b"hots");
    assert!(!layout
        .slot_dir(SlotId::Wol)
        .join("wol-alpha.SC2Map")
        .exists());
}

#[test]
fn wol_rollback_resumes_from_a_partially_restored_live_tree() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha_source = sources.join("alpha");
    make_map_container(
        &alpha_source.join("Maps/campaign/alpha-one.SC2Map"),
        b"alpha one",
    );
    make_map_container(
        &alpha_source.join("Maps/campaign/alpha-two.SC2Map"),
        b"alpha two",
    );
    let alpha_id = PackageId::parse("alpha").unwrap();
    let alpha_plan = plan_from_extracted(&alpha_source).unwrap();
    store.ingest(&alpha_id, SlotId::Wol, &alpha_plan).unwrap();
    let alpha = store.load_manifest(&alpha_id).unwrap();
    let beta = ingest(&store, &sources, "beta", SlotId::Wol, b"beta");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-alpha");
    let transition = manager
        .prepare(Some(&alpha), Some(&beta), "wol-alpha-to-beta")
        .unwrap();
    let paths = transition.journal_paths();
    transition.apply().unwrap();

    let live = layout.slot_dir(SlotId::Wol);
    std::fs::remove_dir_all(live.join("beta.SC2Map")).unwrap();
    let restored = live.join("alpha-one.SC2Map/payload.bin");
    std::fs::create_dir_all(restored.parent().unwrap()).unwrap();
    std::fs::copy(
        paths[0].backup.join("alpha-one.SC2Map/payload.bin"),
        &restored,
    )
    .unwrap();

    let unknown = live.join("unknown-after-crash.txt");
    std::fs::write(&unknown, b"do not delete").unwrap();
    let error = rollback_paths_checked(&paths, Some(&alpha), Some(&beta)).unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(std::fs::read(&unknown).unwrap(), b"do not delete");
    assert!(paths[0].backup.is_dir());

    std::fs::remove_file(unknown).unwrap();
    rollback_paths_checked(&paths, Some(&alpha), Some(&beta)).unwrap();
    assert_eq!(std::fs::read(restored).unwrap(), b"alpha one");
    assert_eq!(
        std::fs::read(live.join("alpha-two.SC2Map/payload.bin")).unwrap(),
        b"alpha two"
    );
    assert!(!paths[0].backup.exists());
    assert!(!paths[0].staging.exists());
}

#[test]
fn wol_repair_rollback_resumes_from_target_and_backup_content() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::Wol, b"original");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-alpha");
    let live = layout.slot_dir(SlotId::Wol);
    let payload = live.join("alpha.SC2Map/payload.bin");
    std::fs::write(&payload, b"user-modified").unwrap();
    std::fs::write(live.join("user-note.txt"), b"preserve me").unwrap();

    let repair = manager.prepare_repair(&alpha, "repair-alpha").unwrap();
    let paths = repair.journal_paths();
    repair.apply().unwrap();
    let backup_payload = paths[0].backup.join("alpha.SC2Map/payload.bin");
    std::fs::write(&backup_payload, b"tampered backup").unwrap();
    let error = verify_repair_rollback_paths(&paths, Some(&alpha)).unwrap_err();
    assert_eq!(error.code(), "unsafe_slot_artifact");
    assert_eq!(std::fs::read(&payload).unwrap(), b"original");
    std::fs::write(&backup_payload, b"user-modified").unwrap();
    std::fs::copy(
        paths[0].backup.join("user-note.txt"),
        live.join("user-note.txt"),
    )
    .unwrap();

    rollback_repair_paths_checked(&paths, Some(&alpha)).unwrap();
    assert_eq!(std::fs::read(payload).unwrap(), b"user-modified");
    assert_eq!(
        std::fs::read(live.join("user-note.txt")).unwrap(),
        b"preserve me"
    );
    assert!(!paths[0].backup.exists());
    assert!(!paths[0].staging.exists());
}

#[test]
fn dedicated_repair_rollback_rejects_backup_bytes_not_bound_to_the_journal() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::LotV, b"original");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-dedicated-alpha");
    let live_payload = layout
        .slot_dir(SlotId::LotV)
        .join("alpha.SC2Map/payload.bin");
    std::fs::write(&live_payload, b"user-modified").unwrap();
    let repair = manager
        .prepare_repair(&alpha, "repair-dedicated-alpha")
        .unwrap();
    let paths = repair.journal_paths();
    repair.apply().unwrap();

    let backup_payload = paths[0].backup.join("alpha.SC2Map/payload.bin");
    std::fs::write(&backup_payload, b"substituted").unwrap();
    let error = rollback_repair_paths_checked(&paths, Some(&alpha)).unwrap_err();
    assert_eq!(error.code(), "unsafe_slot_artifact");
    assert_eq!(std::fs::read(&live_payload).unwrap(), b"original");
    assert!(paths[0].backup.is_dir());

    std::fs::write(&backup_payload, b"user-modified").unwrap();
    rollback_repair_paths_checked(&paths, Some(&alpha)).unwrap();
    assert_eq!(std::fs::read(live_payload).unwrap(), b"user-modified");
}

#[test]
fn wol_repair_rollback_rejects_a_semantically_valid_substituted_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::Wol, b"original");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-wol-receipt");
    let payload = layout
        .slot_dir(SlotId::Wol)
        .join("alpha.SC2Map/payload.bin");
    std::fs::write(&payload, b"user-modified").unwrap();
    let repair = manager
        .prepare_repair(&alpha, "repair-wol-receipt")
        .unwrap();
    let paths = repair.journal_paths();
    repair.apply().unwrap();

    let receipt = paths[0].backup.join(".starvault-backup-ready");
    let original_receipt = std::fs::read(&receipt).unwrap();
    let mut substituted = original_receipt.clone();
    substituted.extend_from_slice(b"\n");
    std::fs::write(&receipt, substituted).unwrap();
    let error = rollback_repair_paths_checked(&paths, Some(&alpha)).unwrap_err();
    assert_eq!(error.code(), "unsafe_slot_artifact");
    assert_eq!(std::fs::read(&payload).unwrap(), b"original");
    assert!(paths[0].backup.is_dir());

    std::fs::write(receipt, original_receipt).unwrap();
    rollback_repair_paths_checked(&paths, Some(&alpha)).unwrap();
    assert_eq!(std::fs::read(payload).unwrap(), b"user-modified");
}

#[cfg(unix)]
#[test]
fn dedicated_repair_rollback_rejects_a_substituted_backup_link() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let alpha = ingest(&store, &sources, "alpha", SlotId::LotV, b"original");
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    deploy_initial(&manager, &alpha, "initial-linked-backup");
    let live_payload = layout
        .slot_dir(SlotId::LotV)
        .join("alpha.SC2Map/payload.bin");
    std::fs::write(&live_payload, b"user-modified").unwrap();
    let repair = manager
        .prepare_repair(&alpha, "repair-linked-backup")
        .unwrap();
    let paths = repair.journal_paths();
    repair.apply().unwrap();

    std::fs::remove_dir_all(&paths[0].backup).unwrap();
    let external = temp.path().join("external-backup");
    let sentinel = external.join("sentinel.txt");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(&sentinel, b"outside").unwrap();
    symlink(&external, &paths[0].backup).unwrap();

    let error = rollback_repair_paths_checked(&paths, Some(&alpha)).unwrap_err();
    assert_eq!(error.code(), "unsafe_slot_artifact");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
    assert_eq!(std::fs::read(live_payload).unwrap(), b"original");
}

#[cfg(unix)]
#[test]
fn wol_live_symlink_is_rejected_before_activation_repair_or_rollback_mutates_it() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temp.path().join("sc2"));
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let sources = temp.path().join("sources");
    let campaign = ingest(&store, &sources, "wol-alpha", SlotId::Wol, b"wol");
    let external = temp.path().join("external-campaign");
    let sentinel = external.join("sentinel.txt");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(&sentinel, b"outside").unwrap();
    let live = layout.slot_dir(SlotId::Wol);
    std::fs::create_dir_all(live.parent().unwrap()).unwrap();
    symlink(&external, &live).unwrap();
    let manager = SlotManager::new(&layout, &store).with_strategy(Some(StrategyChoice::Copy));

    let error = manager
        .prepare(None, Some(&campaign), "linked-activation")
        .unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    let error = manager
        .prepare_repair(&campaign, "linked-repair")
        .unwrap_err();
    assert_eq!(error.code(), "slot_drift");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"outside");
    assert!(std::fs::symlink_metadata(live)
        .unwrap()
        .file_type()
        .is_symlink());
}
