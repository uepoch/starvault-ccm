//! Windows-only: real NTFS junction swaps. Skipped elsewhere; the copy
//! fallback path is exercised by the cross-platform end-to-end test.

#![cfg(windows)]

use std::path::Path;

use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::slots::SlotManager;
use svccm_core::store::Store;

fn make_map_container(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("payload.txt"), b"map").unwrap();
}

fn ingest(store: &Store, src: &Path, id: &str, slot: SlotId) -> String {
    let plan = svccm_core::package::normalize::plan_from_extracted(src).unwrap();
    store.ingest(id, slot, &plan).unwrap()
}

#[test]
fn junction_swap_points_at_deploy_tree_and_reads_through() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(tmp.path().join("sc2"));
    std::fs::create_dir_all(layout.slot_dir(SlotId::LotV)).unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();

    let src = tmp.path().join("src");
    make_map_container(&src.join("Maps/campaign/tarcade.SC2Map"));
    let rev = ingest(&store, &src, "tarcade", SlotId::LotV);

    let manager = SlotManager::new(&layout, &store); // auto = junction first
    manager.activate(SlotId::LotV, "tarcade", &rev).unwrap();

    // The slot is a reparse point reading through to the deploy tree.
    let slot_dir = layout.slot_dir(SlotId::LotV);
    let meta = std::fs::symlink_metadata(&slot_dir).unwrap();
    assert!(meta.file_type().is_symlink(), "slot should be a junction");
    assert!(slot_dir.join("tarcade.SC2Map/payload.txt").is_file());

    // Replacing works through the same path.
    make_map_container(&src.join("Maps/campaign/other.SC2Map"));
    let plan = svccm_core::package::normalize::plan_from_extracted(&src).unwrap();
    let rev2 = store.ingest("other", SlotId::LotV, &plan).unwrap();
    manager.activate(SlotId::LotV, "other", &rev2).unwrap();
    assert!(slot_dir.join("other.SC2Map/payload.txt").is_file());

    // Restore removes the junction and returns the slot to plain.
    manager.restore(SlotId::LotV).unwrap();
    assert!(!std::fs::symlink_metadata(&slot_dir)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(store.active_slots().unwrap().len(), 0);
}
