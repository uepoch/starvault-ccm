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

#[test]
fn set_metadata_rewrites_fields_without_new_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    make_map_container(&tmp.path().join("src/a.SC2Map"));
    let rev = ingest(
        &store,
        &tmp.path().join("src/a.SC2Map"),
        "alpha",
        SlotId::LotV,
    );

    store
        .set_metadata("alpha", "New Title", " Auth ", " 1.2 ", "Desc")
        .unwrap();
    let e = &library::scan(&store).unwrap()[0];
    assert_eq!(e.title.as_deref(), Some("New Title"));
    assert_eq!(e.author.as_deref(), Some("Auth")); // trimmed
    assert_eq!(e.version.as_deref(), Some("1.2"));
    assert_eq!(e.desc.as_deref(), Some("Desc"));
    assert_eq!(e.rev, rev, "metadata is excluded from the revision hash");

    // Blank fields clear back to None.
    store.set_metadata("alpha", "  ", "", "", "").unwrap();
    let e = &library::scan(&store).unwrap()[0];
    assert!(e.title.is_none() && e.author.is_none() && e.version.is_none() && e.desc.is_none());

    assert!(store.set_metadata("ghost", "t", "a", "v", "d").is_err());
}

#[test]
fn remove_package_reclaims_storage_and_refuses_active() {
    let tmp = tempfile::tempdir().unwrap();
    let sc2 = tempfile::tempdir().unwrap();
    let layout = svccm_core::layout::WindowsLayout::new(sc2.path());
    let store = Store::open(tmp.path().join("store")).unwrap();

    // Distinct payloads so each package owns distinct blobs.
    let a = tmp.path().join("src/a");
    std::fs::create_dir_all(a.join("a.SC2Map")).unwrap();
    std::fs::write(a.join("a.SC2Map/payload.txt"), b"alpha-payload").unwrap();
    let b = tmp.path().join("src/b");
    std::fs::create_dir_all(b.join("b.SC2Map")).unwrap();
    std::fs::write(b.join("b.SC2Map/payload.txt"), b"beta-payload").unwrap();

    let rev_a = ingest(&store, &a, "alpha", SlotId::LotV);
    let rev_b = ingest(&store, &b, "beta", SlotId::Wol);

    // A deploy tree for alpha's rev (the shape activation leaves behind).
    let deploy = store.deploy_dir("lotv", &rev_a);
    std::fs::create_dir_all(&deploy).unwrap();
    std::fs::write(deploy.join("file.txt"), b"x").unwrap();

    // Active packages refuse removal.
    let manager = SlotManager::new(&layout, &store);
    manager.activate(SlotId::Wol, "beta", &rev_b).unwrap();
    assert!(store.remove_package("beta").is_err());

    // Removing alpha drops its package dir, deploy tree and blobs.
    let alpha_blob = store.load_manifest("alpha", &rev_a).unwrap().files[0]
        .sha256
        .clone();
    store.remove_package("alpha").unwrap();
    assert!(!store.root().join("packages/alpha").exists());
    assert!(!deploy.exists());
    assert!(!store
        .root()
        .join("blobs")
        .join(&alpha_blob[..2])
        .join(&alpha_blob)
        .exists());
    assert!(
        store.root().join("packages/beta").exists(),
        "beta untouched"
    );
    let entries = library::scan(&store).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "beta");

    assert!(store.remove_package("alpha").is_err(), "already removed");
    assert!(store.remove_package("ghost").is_err());
}
