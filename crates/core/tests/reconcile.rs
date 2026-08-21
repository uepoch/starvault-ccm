//! Startup reconciliation: crash leftovers, dangling links.

use svccm_core::layout::{GameLayout, SlotId, WindowsLayout};
use svccm_core::slots::SlotManager;
use svccm_core::store::Store;

fn setup() -> (tempfile::TempDir, WindowsLayout, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(tmp.path().join("sc2"));
    std::fs::create_dir_all(layout.slot_dir(SlotId::LotV)).unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    (tmp, layout, store)
}

#[test]
fn crash_backup_is_restored_when_slot_missing() {
    let (_tmp, layout, store) = setup();

    // Simulate a crash mid-swap: slot dir renamed aside, nothing in place.
    let slot_dir = layout.slot_dir(SlotId::LotV);
    std::fs::remove_dir_all(&slot_dir).unwrap();
    let backup = sibling_backup(&slot_dir);
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("payload.txt"), b"old").unwrap();
    store.set_active_slot(SlotId::LotV, "pkg", "rev").unwrap();

    let manager = SlotManager::new(&layout, &store);
    let report = manager.reconcile().unwrap();
    assert!(report.iter().any(|r| r.contains("restored")), "{report:?}");
    assert!(slot_dir.join("payload.txt").exists());
}

#[test]
fn stale_backups_and_staging_are_reclaimed() {
    let (_tmp, layout, store) = setup();

    let slot_dir = layout.slot_dir(SlotId::LotV); // exists: committed state wins
    let backup = sibling_backup(&slot_dir);
    std::fs::create_dir_all(&backup).unwrap();
    let staging = slot_dir.with_file_name(format!("void.staging-{}", std::process::id()));
    std::fs::create_dir_all(&staging).unwrap();

    SlotManager::new(&layout, &store).reconcile().unwrap();
    assert!(!backup.exists());
    assert!(!staging.exists());
    assert!(slot_dir.exists());
}

#[cfg(unix)]
#[test]
fn dangling_link_is_removed_and_ledger_cleared() {
    use std::os::unix::fs::symlink;
    let (_tmp, layout, store) = setup();

    let slot_dir = layout.slot_dir(SlotId::LotV);
    std::fs::remove_dir_all(&slot_dir).unwrap();
    symlink("/nowhere/nothing", &slot_dir).unwrap();
    store.set_active_slot(SlotId::LotV, "pkg", "rev").unwrap();

    let report = SlotManager::new(&layout, &store).reconcile().unwrap();
    assert!(report.iter().any(|r| r.contains("dangling")), "{report:?}");
    assert!(!symlink_or_link(&slot_dir));
    assert!(store.active_slots().unwrap().is_empty());
}

#[cfg(unix)]
fn symlink_or_link(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn sibling_backup(slot_dir: &std::path::Path) -> std::path::PathBuf {
    let name = slot_dir.file_name().unwrap().to_string_lossy();
    slot_dir.with_file_name(format!("{name}.backup-{}", std::process::id()))
}
