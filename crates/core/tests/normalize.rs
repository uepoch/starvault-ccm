//! Normalizer tests: arbitrary layouts → canonical `slot/` + `mods/` form.
//!
//! Trees are assembled from the real fixtures extracted from `example.zip`
//! (see tests/fixtures/). Layout cases mirror docs/design/package-model.md.

use std::path::Path;

use svccm_core::package::metadata::SlotGuessKind;
use svccm_core::package::normalize::plan_from_extracted;

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Create a directory container with the RaynorRogue metadata (which declares
/// the nested `SCORE\SCORE-Other.SC2Mod` dependency).
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
    // One payload file so the container has content.
    std::fs::write(dir.join("payload.txt"), b"payload").unwrap();
}

/// Minimal valid map container whose declarations match tarcade's
/// DocumentInfo (kit_liberty_story + RaynorRogue).
fn make_tarcade_container(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();

    // Synthetic H2CS header agreeing with tarcade.DocumentInfo.
    let deps = [
        r"file:Mods\kit_liberty_story.SC2Mod",
        r"file:Mods\RaynorRogue.SC2Mod",
    ];
    let mut header = b"H2CS".to_vec();
    header.extend_from_slice(&[0u8; 40]); // padding up to offset 44
    header.extend_from_slice(&(deps.len() as u32).to_le_bytes());
    for dep in deps {
        header.extend_from_slice(dep.as_bytes());
        header.push(0);
    }
    assert_eq!(&header[0..4], b"H2CS");

    std::fs::write(dir.join("DocumentHeader"), &header).unwrap();
    std::fs::copy(fixture("tarcade.DocumentInfo"), dir.join("DocumentInfo")).unwrap();
    std::fs::write(dir.join("map-payload.txt"), b"map").unwrap();
}

#[test]
fn game_mirror_layout_maps_to_canonical_form() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // example.zip shape: Maps/campaign/*.SC2Map + Mods/ at zip root.
    make_tarcade_container(&root.join("Maps/campaign/tarcade.SC2Map"));
    make_raynorrogue_container(&root.join("Mods/RaynorRogue.SC2Mod"));
    make_raynorrogue_container(&root.join("Mods/SCORE/SCORE-Other.SC2Mod"));
    make_raynorrogue_container(&root.join("Mods/kit_liberty_story.SC2Mod"));
    std::fs::copy(
        fixture("RandomBuff.SC2Mod"),
        root.join("Mods/RandomBuff.SC2Mod"),
    )
    .unwrap();
    std::fs::write(root.join("readme.txt"), b"hi").unwrap();

    let plan = plan_from_extracted(root).unwrap();

    let targets: Vec<&str> = plan.files.iter().map(|f| f.target.as_str()).collect();
    // Map goes to slot root regardless of source nesting…
    assert!(
        targets.contains(&"slot/tarcade.SC2Map/map-payload.txt"),
        "{targets:?}"
    );
    // …nested mod keeps its structure (the case old CCM broke)…
    assert!(
        targets.contains(&"mods/RaynorRogue.SC2Mod/payload.txt"),
        "{targets:?}"
    );
    assert!(
        targets.contains(&"mods/SCORE/SCORE-Other.SC2Mod/payload.txt"),
        "{targets:?}"
    );
    assert!(targets.contains(&"mods/RandomBuff.SC2Mod"), "{targets:?}");
    // …stray files travel with the slot.
    assert!(targets.contains(&"slot/readme.txt"), "{targets:?}");

    // Dependencies collected from all containers, deduplicated.
    let refs: Vec<&str> = plan
        .dependencies
        .iter()
        .map(|d| d.reference.as_str())
        .collect();
    assert!(
        refs.iter().any(|r| r.contains(r"SCORE\SCORE-Other.SC2Mod")),
        "{refs:?}"
    );
    assert!(refs.iter().any(|r| r.starts_with("bnet:")), "{refs:?}");

    // Bundled references (including the nested SCORE one) raise no warning;
    // RaynorRogueRaw is genuinely missing from this tree and must be flagged.
    let unresolved: Vec<&String> = plan
        .warnings
        .iter()
        .filter(|w| w.contains("unresolved"))
        .collect();
    assert_eq!(unresolved.len(), 1, "{:?}", plan.warnings);
    assert!(unresolved[0].contains("RaynorRogueRaw"));
}

#[test]
fn ccm_flat_layout_is_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    make_tarcade_container(&root.join("tarcade.SC2Map"));
    make_raynorrogue_container(&root.join("Mods/RaynorRogue.SC2Mod"));

    let plan = plan_from_extracted(root).unwrap();
    let targets: Vec<&str> = plan.files.iter().map(|f| f.target.as_str()).collect();
    assert!(
        targets.contains(&"slot/tarcade.SC2Map/map-payload.txt"),
        "{targets:?}"
    );
    assert!(
        targets.contains(&"mods/RaynorRogue.SC2Mod/payload.txt"),
        "{targets:?}"
    );
}

#[test]
fn wrapper_folder_is_stripped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Wrapper containing a CCM-flat package + metadata at wrapper root.
    std::fs::create_dir_all(root.join("Wrapper")).unwrap();
    std::fs::write(
        root.join("Wrapper/metadata.txt"),
        b"title=Test\ncampaign=lotv\n",
    )
    .unwrap();
    make_tarcade_container(&root.join("Wrapper/tarcade.SC2Map"));
    make_raynorrogue_container(&root.join("Wrapper/Mods/RaynorRogue.SC2Mod"));

    let plan = plan_from_extracted(root).unwrap();
    let targets: Vec<&str> = plan.files.iter().map(|f| f.target.as_str()).collect();
    assert!(
        targets.contains(&"slot/tarcade.SC2Map/map-payload.txt"),
        "{targets:?}"
    );
    assert!(
        !targets.iter().any(|t| t.starts_with("slot/Wrapper/")),
        "{targets:?}"
    );

    assert_eq!(plan.slot_guess.kind, SlotGuessKind::LotV);
    assert_eq!(
        plan.metadata.as_ref().unwrap().title.as_deref(),
        Some("Test")
    );
}

#[test]
fn loose_mod_outside_mods_uses_legacy_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Old-style packages drop .SC2Mod anywhere; maps reference them at
    // Mods\<basename>, so canonical form places them there.
    make_tarcade_container(&root.join("maps/somewhere/tarcade.SC2Map"));
    make_raynorrogue_container(&root.join("deps/RaynorRogue.SC2Mod"));

    let plan = plan_from_extracted(root).unwrap();
    let targets: Vec<&str> = plan.files.iter().map(|f| f.target.as_str()).collect();
    assert!(
        targets.contains(&"mods/RaynorRogue.SC2Mod/payload.txt"),
        "{targets:?}"
    );
}

#[test]
fn packed_mod_in_flat_layout_ships_to_mods_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Swarm-Reborn shape: packed .SC2Mod files and maps side by side at the
    // zip root, no Mods/ folder at all. The mods must land at the Mods root
    // (maps reference them as Mods\<name>); maps keep their subfolder.
    make_tarcade_container(&root.join("zlab01.SC2Map"));
    std::fs::create_dir_all(root.join("evolution")).unwrap();
    std::fs::write(root.join("evolution/zchar01.SC2Map"), b"packed map").unwrap();
    std::fs::copy(
        fixture("RandomBuff.SC2Mod"),
        root.join("crys_assets.SC2Mod"),
    )
    .unwrap();

    let plan = plan_from_extracted(root).unwrap();
    let targets: Vec<&str> = plan.files.iter().map(|f| f.target.as_str()).collect();
    assert!(
        targets.contains(&"mods/crys_assets.SC2Mod"),
        "packed mod must ship to the Mods root: {targets:?}"
    );
    assert!(
        targets.contains(&"slot/zlab01.SC2Map/DocumentInfo"),
        "packed map keeps the loose rule: {targets:?}"
    );
    assert!(
        targets.contains(&"slot/evolution/zchar01.SC2Map"),
        "map subfolder is load-bearing: {targets:?}"
    );
}

#[test]
fn differing_collision_is_a_hard_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Two same-named maps in different places with different content.
    make_tarcade_container(&root.join("a/tarcade.SC2Map"));
    let other = root.join("b/tarcade.SC2Map");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::copy(
        fixture("raynorrogue.DocumentHeader"),
        other.join("DocumentHeader"),
    )
    .unwrap();
    std::fs::copy(fixture("tarcade.DocumentInfo"), other.join("DocumentInfo")).unwrap();
    std::fs::write(other.join("map-payload.txt"), b"different bytes").unwrap();

    assert!(plan_from_extracted(root).is_err());
}

#[test]
fn identical_collision_deduplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    make_tarcade_container(&root.join("a/tarcade.SC2Map"));
    make_tarcade_container(&root.join("b/tarcade.SC2Map"));

    let plan = plan_from_extracted(root).unwrap();
    let count = plan
        .files
        .iter()
        .filter(|f| f.target == "slot/tarcade.SC2Map/map-payload.txt")
        .count();
    assert_eq!(count, 1);
}

#[test]
fn no_containers_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("only-a-file.txt"), b"x").unwrap();
    assert!(plan_from_extracted(tmp.path()).is_err());
}
