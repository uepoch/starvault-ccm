#![cfg(windows)]

//! Native Windows sharing-violation regressions for managed Mods transitions.

use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use svccm_core::identity::PackageId;
use svccm_core::layout::SlotId;
use svccm_core::mods::PreparedModsTransition;
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::{ManagedMod, PackageManifest, Store};

fn make_map(root: &Path, name: &str) {
    let map = root.join(format!("Maps/campaign/{name}.SC2Map"));
    std::fs::create_dir_all(&map).unwrap();
    std::fs::write(map.join("payload"), name.as_bytes()).unwrap();
}

fn make_packed_mod(root: &Path, name: &str, payload: &[u8]) {
    std::fs::create_dir_all(root.join("Mods")).unwrap();
    std::fs::write(root.join(format!("Mods/{name}.SC2Mod")), payload).unwrap();
}

fn ingest(store: &Store, source: &Path, id: &str) -> PackageManifest {
    let id = PackageId::parse(id).unwrap();
    store
        .ingest(&id, SlotId::LotV, &plan_from_extracted(source).unwrap())
        .unwrap();
    store.load_manifest(&id).unwrap()
}

fn activate_mods(store: &Store, mods_root: &Path, target: &PackageManifest) -> Vec<ManagedMod> {
    let transition = PreparedModsTransition::prepare(
        store,
        mods_root,
        &[],
        Some(target),
        "windows-locked-initial",
    )
    .unwrap();
    transition.apply().unwrap();
    let rows = transition.target_rows().to_vec();
    transition.finalize().unwrap();
    rows
}

#[test]
fn exclusive_locked_staged_mod_rolls_back_a_partially_applied_switch() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();

    let source_a = temp.path().join("source-a");
    make_map(&source_a, "campaign-a");
    make_packed_mod(&source_a, "AlphaA", b"exact alpha A bytes");
    make_packed_mod(&source_a, "OmegaA", b"exact omega A bytes");
    let campaign_a = ingest(&store, &source_a, "campaign-a");

    let source_b = temp.path().join("source-b");
    make_map(&source_b, "campaign-b");
    make_packed_mod(&source_b, "FirstB", b"first B bytes");
    make_packed_mod(&source_b, "LockedB", b"locked B bytes");
    let campaign_b = ingest(&store, &source_b, "campaign-b");

    let mods_root = temp.path().join("Mods");
    let rows_a = activate_mods(&store, &mods_root, &campaign_a);
    let switch = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &rows_a,
        Some(&campaign_b),
        "windows-locked-switch",
    )
    .unwrap();

    // Deny every subsequent open of the second staged B artifact. The first
    // B file is deployed before this one, so the failure occurs only after A
    // has been removed and a genuinely mixed partial target exists.
    let locked_staged = switch.staging_path().join("LockedB.SC2Mod");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked_staged)
        .unwrap();

    let error = switch.apply().unwrap_err();
    assert_eq!(error.code(), "open_staged_mod");
    assert!(!mods_root.join("AlphaA.SC2Mod").exists());
    assert!(!mods_root.join("OmegaA.SC2Mod").exists());
    assert_eq!(
        std::fs::read(mods_root.join("FirstB.SC2Mod")).unwrap(),
        b"first B bytes"
    );
    assert!(!mods_root.join("LockedB.SC2Mod").exists());

    drop(lock);
    switch.rollback().unwrap();

    assert_eq!(
        std::fs::read(mods_root.join("AlphaA.SC2Mod")).unwrap(),
        b"exact alpha A bytes"
    );
    assert_eq!(
        std::fs::read(mods_root.join("OmegaA.SC2Mod")).unwrap(),
        b"exact omega A bytes"
    );
    assert!(!mods_root.join("FirstB.SC2Mod").exists());
    assert!(!mods_root.join("LockedB.SC2Mod").exists());
    assert!(!switch.staging_path().exists());
    assert!(!switch.backup_path().exists());
}
