//! Launch pre-flight drift detection and legacy migration candidates.

use svccm_core::launch::preflight;
use svccm_core::layout::{GameLayout, SlotId, WindowsLayout};
use svccm_core::library::migration_candidates;
use svccm_core::slots::SlotManager;
use svccm_core::store::Store;

fn make_map_container(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("payload.txt"), b"map").unwrap();
}

#[test]
fn missing_exe_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(tmp.path());
    let store = Store::open(tmp.path().join("store")).unwrap();
    let report = preflight(&layout, &store);
    assert!(!report.exe_ok);
    assert!(!report.ok());
}

#[test]
fn intact_install_has_no_drift_but_damaged_slot_is_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(tmp.path().join("sc2"));
    std::fs::create_dir_all(layout.slot_dir(SlotId::LotV)).unwrap();
    std::fs::write(layout.exe(), b"fake").unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();

    let src = tmp.path().join("src");
    make_map_container(&src.join("Maps/campaign/tarcade.SC2Map"));
    let plan = svccm_core::package::normalize::plan_from_extracted(&src).unwrap();
    let rev = store.ingest("tarcade", SlotId::LotV, &plan).unwrap();
    SlotManager::new(&layout, &store)
        .activate(SlotId::LotV, "tarcade", &rev)
        .unwrap();

    // Intact: only the running-instance check may flag (no SC2 here).
    let report = preflight(&layout, &store);
    assert!(report.drift.is_empty(), "{:?}", report.drift);
    assert!(report.no_running_instance);

    // Damage the deployed slot; pre-flight must name it.
    std::fs::remove_file(
        layout
            .slot_dir(SlotId::LotV)
            .join("tarcade.SC2Map/payload.txt"),
    )
    .unwrap();
    let report = preflight(&layout, &store);
    assert!(
        report
            .drift
            .iter()
            .any(|d| d.contains("lotv") && d.contains("files")),
        "{:?}",
        report.drift
    );
}

#[test]
fn migration_candidates_skip_slot_owned_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(tmp.path());
    let campaign = layout.slot_dir(SlotId::Wol);

    make_map_container(&campaign.join("My Cool Campaign"));
    make_map_container(&campaign.join("swarm")); // slot-owned: skipped
    make_map_container(&campaign.join("nova")); // slot-owned: skipped

    let candidates = migration_candidates(&layout);
    let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["My Cool Campaign"]);
}
