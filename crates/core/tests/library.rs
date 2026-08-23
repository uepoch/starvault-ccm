//! Library snapshots and legacy-CCM detection.

use std::path::Path;

use svccm_core::contracts::HealthState;
use svccm_core::identity::PackageId;
use svccm_core::layout::{SlotId, WindowsLayout};
use svccm_core::library::{self, LegacyCcmInstall};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::Store;

fn make_map_container(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("map-payload.txt"), b"map").unwrap();
}

fn ingest(store: &Store, source: &Path, id: &str, faction: SlotId) -> String {
    store
        .ingest(
            &PackageId::parse(id).unwrap(),
            faction,
            &plan_from_extracted(source).unwrap(),
        )
        .unwrap()
}

#[test]
fn empty_store_has_a_ready_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let snapshot = library::scan(&store).unwrap();
    assert!(snapshot.entries.is_empty());
    assert!(snapshot.active_campaign.is_none());
    assert_eq!(snapshot.health.state, HealthState::Ready);
}

#[test]
fn snapshot_has_one_row_per_package_and_one_authoritative_active_campaign() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    make_map_container(&tmp.path().join("alpha/a.SC2Map"));
    make_map_container(&tmp.path().join("beta/b.SC2Map"));
    let alpha_revision = ingest(&store, &tmp.path().join("alpha"), "alpha", SlotId::LotV);
    ingest(&store, &tmp.path().join("beta"), "beta", SlotId::Wol);

    let active = svccm_core::contracts::ActiveCampaign {
        id: PackageId::parse("alpha").unwrap(),
        revision: alpha_revision,
        faction: SlotId::LotV,
    };
    store.set_active_campaign(&active).unwrap();
    let snapshot = library::scan(&store).unwrap();
    assert_eq!(snapshot.entries.len(), 2);
    assert!(snapshot
        .entries
        .iter()
        .any(|entry| entry.id.as_str() == "alpha"));
    assert!(snapshot
        .entries
        .iter()
        .any(|entry| entry.id.as_str() == "beta"));
    assert_eq!(snapshot.active_campaign, Some(active));
}

#[test]
fn corrupt_manifest_remains_visible_as_a_health_issue() {
    let tmp = tempfile::tempdir().unwrap();
    let store_root = tmp.path().join("store");
    let store = Store::open_for_tests(&store_root).unwrap();
    make_map_container(&tmp.path().join("alpha/a.SC2Map"));
    ingest(&store, &tmp.path().join("alpha"), "alpha", SlotId::LotV);
    std::fs::write(store_root.join("packages/alpha/manifest.json"), b"not JSON").unwrap();

    let snapshot = library::scan(&store).unwrap();
    assert!(snapshot.entries.is_empty());
    assert_eq!(snapshot.health.state, HealthState::Drifted);
    assert_eq!(snapshot.health.issues.len(), 1);
    assert_eq!(snapshot.health.issues[0].code, "corrupt_package_manifest");
}

#[test]
fn metadata_edit_changes_fields_without_changing_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    make_map_container(&tmp.path().join("alpha/a.SC2Map"));
    let revision = ingest(&store, &tmp.path().join("alpha"), "alpha", SlotId::LotV);
    let id = PackageId::parse("alpha").unwrap();

    store
        .set_metadata(&id, " New Title ", " Auth ", " 1.2 ", " Desc ")
        .unwrap();
    let entry = &library::scan(&store).unwrap().entries[0];
    assert_eq!(entry.revision, revision);
    assert_eq!(entry.title.as_deref(), Some("New Title"));
    assert_eq!(entry.author.as_deref(), Some("Auth"));
    assert_eq!(entry.version.as_deref(), Some("1.2"));
    assert_eq!(entry.desc.as_deref(), Some("Desc"));
}

#[test]
fn legacy_detection_reads_exe_hint() {
    let appdata = tempfile::tempdir().unwrap();
    assert_eq!(LegacyCcmInstall::detect(appdata.path()), None);

    let config = LegacyCcmInstall::config_path(appdata.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        "C:\\Games\\StarCraft II\\Versions\\Base69232\\SC2_x64.exe\nextra\n",
    )
    .unwrap();
    assert_eq!(
        LegacyCcmInstall::detect(appdata.path()),
        Some(LegacyCcmInstall {
            exe_hint: Some("C:\\Games\\StarCraft II\\Versions\\Base69232\\SC2_x64.exe".into()),
        })
    );
}

#[test]
fn migration_candidates_skip_slot_owned_directories() {
    let temporary = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temporary.path());
    let campaign = layout.slot_dir(SlotId::Wol);

    make_map_container(&campaign.join("My Cool Campaign/MyMap.SC2Map"));
    make_map_container(&campaign.join("swarm"));
    make_map_container(&campaign.join("nova"));

    let names: Vec<String> = library::migration_candidates(&layout)
        .into_iter()
        .map(|candidate| candidate.name)
        .collect();
    assert_eq!(names, ["My Cool Campaign"]);
}

#[test]
fn migration_candidates_skip_only_exact_slot_operation_artifacts() {
    let temporary = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(temporary.path());
    let campaign = layout.slot_dir(SlotId::Wol);

    make_map_container(&campaign.join("void.backup-operation/ignored.SC2Map"));
    make_map_container(&campaign.join("A.backup-story/A.SC2Map"));
    make_map_container(&campaign.join("void.backup-/B.SC2Map"));

    let names: Vec<String> = library::migration_candidates(&layout)
        .into_iter()
        .map(|candidate| candidate.name)
        .collect();
    assert_eq!(names, ["A.backup-story", "void.backup-"]);
}
