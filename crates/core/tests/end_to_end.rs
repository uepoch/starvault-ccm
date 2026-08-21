//! M1 exit gate: import → ingest → activate → golden tree.
//!
//! Drives the whole core against a fake SC2 install in a temp dir.

use std::path::Path;

use svccm_core::layout::{GameLayout, SlotId, WindowsLayout};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::slots::SlotManager;
use svccm_core::store::Store;

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A map container whose declarations match tarcade.DocumentInfo.
fn make_tarcade_container(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let deps = [
        r"file:Mods\kit_liberty_story.SC2Mod",
        r"file:Mods\RaynorRogue.SC2Mod",
    ];
    let mut header = b"H2CS".to_vec();
    header.extend_from_slice(&[0u8; 40]);
    header.extend_from_slice(&(deps.len() as u32).to_le_bytes());
    for dep in deps {
        header.extend_from_slice(dep.as_bytes());
        header.push(0);
    }
    std::fs::write(dir.join("DocumentHeader"), &header).unwrap();
    std::fs::copy(fixture("tarcade.DocumentInfo"), dir.join("DocumentInfo")).unwrap();
    std::fs::write(dir.join("map-payload.txt"), b"map").unwrap();
}

/// Mod container declaring kit_liberty_story + RaynorRogue + nested SCORE dep.
fn make_raynorrogue_container(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(
        fixture("raynorrogue.DocumentHeader"),
        dir.join("DocumentHeader"),
    )
    .unwrap();
    std::fs::copy(
        fixture("raynorrogue.DocumentInfo"),
        dir.join("DocumentInfo"),
    )
    .unwrap();
    std::fs::write(dir.join("payload.txt"), b"payload").unwrap();
}

/// Ingest a game-mirror-shaped package as `id` into `store`.
fn ingest_package(store: &Store, extracted: &Path, id: &str, slot: SlotId) -> String {
    let plan = plan_from_extracted(extracted).expect("plan");
    store.ingest(id, slot, &plan).expect("ingest")
}

#[test]
fn import_activate_restore_end_to_end() {
    let sc2 = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(sc2.path());
    let store = Store::open(sc2.path().join("store")).unwrap();

    // --- build a game-mirror package -------------------------------------
    make_tarcade_container(&src.path().join("Maps/campaign/tarcade.SC2Map"));
    make_raynorrogue_container(&src.path().join("Mods/RaynorRogue.SC2Mod"));
    make_raynorrogue_container(&src.path().join("Mods/SCORE/SCORE-Other.SC2Mod"));
    std::fs::copy(
        fixture("RandomBuff.SC2Mod"),
        src.path().join("Mods/RandomBuff.SC2Mod"),
    )
    .unwrap();

    // --- import -----------------------------------------------------------
    let rev = ingest_package(&store, src.path(), "tarcade", SlotId::LotV);

    // --- activate ----------------------------------------------------------
    let manager = SlotManager::new(&layout, &store);
    manager.activate(SlotId::LotV, "tarcade", &rev).unwrap();

    // Golden tree checks.
    assert!(layout
        .slot_dir(SlotId::LotV)
        .join("tarcade.SC2Map/map-payload.txt")
        .is_file());
    assert!(layout
        .slot_dir(SlotId::LotV)
        .join("tarcade.SC2Map/DocumentInfo")
        .is_file());
    // Nested mod path preserved under the real Mods\ tree:
    assert!(layout
        .mods_dir()
        .join("RaynorRogue.SC2Mod/payload.txt")
        .is_file());
    assert!(layout
        .mods_dir()
        .join("SCORE/SCORE-Other.SC2Mod/payload.txt")
        .is_file());
    assert!(layout.mods_dir().join("RandomBuff.SC2Mod").is_file());

    // Ledger agrees.
    let active = store.active_slots().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].0, "lotv");

    // No staging leftovers.
    let void_dir = layout.slot_dir(SlotId::LotV);
    let siblings: Vec<_> = std::fs::read_dir(void_dir.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !siblings.iter().any(|n| n.contains("staging")),
        "{siblings:?}"
    );

    // --- restore ------------------------------------------------------------
    manager.restore(SlotId::LotV).unwrap();
    assert!(std::fs::read_dir(layout.slot_dir(SlotId::LotV))
        .unwrap()
        .next()
        .is_none());
    assert!(store.active_slots().unwrap().is_empty());
}

#[test]
fn two_slots_sharing_identical_deps_coexist() {
    let sc2 = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(sc2.path());
    let store = Store::open(sc2.path().join("store")).unwrap();

    for (dir, name) in [(a.path(), "tarcade"), (b.path(), "tarcade-two")] {
        make_tarcade_container(&dir.join(format!("Maps/campaign/{name}.SC2Map")));
        make_raynorrogue_container(&dir.join("Mods/RaynorRogue.SC2Mod"));
    }

    let rev_a = ingest_package(&store, a.path(), "pkg-a", SlotId::LotV);
    let rev_b = ingest_package(&store, b.path(), "pkg-b", SlotId::Nco);

    let manager = SlotManager::new(&layout, &store);
    manager.activate(SlotId::LotV, "pkg-a", &rev_a).unwrap();
    manager.activate(SlotId::Nco, "pkg-b", &rev_b).unwrap();

    assert_eq!(store.active_slots().unwrap().len(), 2);
    assert!(layout
        .slot_dir(SlotId::Nco)
        .join("tarcade-two.SC2Map/map-payload.txt")
        .is_file());
}

#[test]
fn cross_slot_conflict_blocks_activation() {
    let sc2 = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(sc2.path());
    let store = Store::open(sc2.path().join("store")).unwrap();

    // Both ship Mods/Shared.SC2Mod with DIFFERENT content.
    for (dir, byte) in [(a.path(), 1u8), (b.path(), 2u8)] {
        make_tarcade_container(&dir.join("Maps/campaign/map.SC2Map"));
        std::fs::create_dir_all(dir.join("Mods/Shared.SC2Mod")).unwrap();
        std::fs::write(dir.join("Mods/Shared.SC2Mod/data.bin"), vec![byte; 16]).unwrap();
    }

    let rev_a = ingest_package(&store, a.path(), "pkg-a", SlotId::LotV);
    let rev_b = ingest_package(&store, b.path(), "pkg-b", SlotId::Nco);

    let manager = SlotManager::new(&layout, &store);
    manager.activate(SlotId::LotV, "pkg-a", &rev_a).unwrap();

    // Activating pkg-b conflicts on Shared.SC2Mod and must be blocked…
    assert!(manager.activate(SlotId::Nco, "pkg-b", &rev_b).is_err());

    // …with the first slot untouched.
    assert_eq!(
        store.active_slots().unwrap(),
        vec![("lotv".to_string(), "pkg-a".to_string(), rev_a.clone())]
    );
    assert!(layout
        .slot_dir(SlotId::LotV)
        .join("map.SC2Map/map-payload.txt")
        .is_file());
}

#[test]
fn wol_switch_preserves_sibling_campaign_dirs() {
    let sc2 = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(sc2.path());
    let store = Store::open(sc2.path().join("store")).unwrap();

    // Pre-existing HotS content inside the shared Campaign root.
    std::fs::create_dir_all(layout.slot_dir(SlotId::HotS)).unwrap();
    std::fs::write(
        layout.slot_dir(SlotId::HotS).join("hots-marker.txt"),
        b"hots",
    )
    .unwrap();

    make_tarcade_container(&src.path().join("Maps/campaign/tarcade.SC2Map"));
    let rev = ingest_package(&store, src.path(), "wol-pkg", SlotId::Wol);

    let manager = SlotManager::new(&layout, &store);
    manager.activate(SlotId::Wol, "wol-pkg", &rev).unwrap();

    // Package landed in the shared root…
    assert!(layout
        .slot_dir(SlotId::Wol)
        .join("tarcade.SC2Map/map-payload.txt")
        .is_file());
    // …and the sibling survived.
    assert!(layout
        .slot_dir(SlotId::HotS)
        .join("hots-marker.txt")
        .is_file());
}
