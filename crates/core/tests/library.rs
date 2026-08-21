//! Library scan and legacy-CCM detection.

use std::path::Path;

use svccm_core::layout::SlotId;
use svccm_core::library::{self, LegacyCcmInstall};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::slots::SlotManager;
use svccm_core::store::Store;

/// Minimal map container: a `.SC2Map` directory with a payload file.
fn make_map_container(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("map-payload.txt"), b"map").unwrap();
}

fn ingest(store: &Store, src: &Path, id: &str, slot: SlotId) -> String {
    let plan = plan_from_extracted(src).expect("plan");
    store.ingest(id, slot, &plan).expect("ingest")
}

#[test]
fn scan_empty_store_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    assert!(library::scan(&store).unwrap().is_empty());
}

#[test]
fn scan_reports_active_status_per_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let sc2 = tempfile::tempdir().unwrap();
    let layout = svccm_core::layout::WindowsLayout::new(sc2.path());
    let store = Store::open(tmp.path().join("store")).unwrap();

    make_map_container(&tmp.path().join("src/a/a.SC2Map"));
    make_map_container(&tmp.path().join("src/b/b.SC2Map"));
    let rev_a = ingest(&store, &tmp.path().join("src/a"), "alpha", SlotId::LotV);
    ingest(&store, &tmp.path().join("src/b"), "beta", SlotId::Wol);

    // Nothing active yet.
    let entries = library::scan(&store).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.active_on.is_empty()));

    // Activate alpha on LotV.
    let manager = SlotManager::new(&layout, &store);
    manager.activate(SlotId::LotV, "alpha", &rev_a).unwrap();

    let entries = library::scan(&store).unwrap();
    let alpha = entries.iter().find(|e| e.id == "alpha").unwrap();
    assert_eq!(alpha.active_on, vec!["lotv".to_string()]);
    assert!(entries
        .iter()
        .find(|e| e.id == "beta")
        .unwrap()
        .active_on
        .is_empty());
}

#[test]
fn legacy_detection_reads_exe_hint() {
    let appdata = tempfile::tempdir().unwrap();

    // No old install.
    assert_eq!(LegacyCcmInstall::detect(appdata.path()), None);

    // Config present with an exe path on the first line.
    let cfg_dir = LegacyCcmInstall::config_path(appdata.path());
    std::fs::create_dir_all(cfg_dir.parent().unwrap()).unwrap();
    std::fs::write(
        &cfg_dir,
        "C:\\Games\\StarCraft II\\Versions\\Base69232\\SC2_x64.exe\nextra\n",
    )
    .unwrap();
    assert_eq!(
        LegacyCcmInstall::detect(appdata.path()),
        Some(LegacyCcmInstall {
            exe_hint: Some("C:\\Games\\StarCraft II\\Versions\\Base69232\\SC2_x64.exe".into()),
        })
    );

    // Empty file still counts as detected, without a hint.
    std::fs::write(&cfg_dir, "").unwrap();
    assert_eq!(
        LegacyCcmInstall::detect(appdata.path()),
        Some(LegacyCcmInstall { exe_hint: None })
    );
}
