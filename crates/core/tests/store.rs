//! One-manifest storage, schema, ledger, and fail-closed garbage collection.

use std::path::Path;

use svccm_core::contracts::ActiveCampaign;
use svccm_core::identity::PackageId;
use svccm_core::layout::SlotId;
use svccm_core::operation::{OperationKind, OperationPaths, PendingOperation};
use svccm_core::package::metadata::LegacyMetadata;
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::{ManagedMod, ManagedModDisposition, Store};

fn map_container(directory: &Path, content: &[u8]) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("payload.bin"), content).unwrap();
}

fn plan(source: &Path) -> svccm_core::package::normalize::PackagePlan {
    plan_from_extracted(source).unwrap()
}

fn ingest(store: &Store, source: &Path, id: &str, faction: SlotId) -> String {
    store
        .ingest(&PackageId::parse(id).unwrap(), faction, &plan(source))
        .unwrap()
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) {
    junction::create(target, link).unwrap();
}

#[cfg(unix)]
fn remove_directory_link(link: &Path) {
    std::fs::remove_file(link).unwrap();
}

#[cfg(windows)]
fn remove_directory_link(link: &Path) {
    std::fs::remove_dir(link).unwrap();
}

#[cfg(any(unix, windows))]
fn assert_sentinel_only(directory: &Path) {
    assert_eq!(std::fs::read(directory.join("sentinel")).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
}

#[test]
fn creates_only_the_version_two_single_campaign_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let _store = Store::open_for_tests(&root).unwrap();
    let connection = rusqlite::Connection::open(root.join("ledger.db")).unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap();
    let tables: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(tables, ["active_campaign", "managed_mods"]);
}

#[test]
fn rejects_a_legacy_schema_without_migrating_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    std::fs::create_dir_all(&root).unwrap();
    let connection = rusqlite::Connection::open(root.join("ledger.db")).unwrap();
    connection
        .execute("CREATE TABLE active_slots(slot TEXT PRIMARY KEY)", [])
        .unwrap();
    drop(connection);

    let error = Store::open_for_tests(&root)
        .err()
        .expect("legacy schema was accepted");
    assert_eq!(error.code(), "unsupported_store_schema");
    let connection = rusqlite::Connection::open(root.join("ledger.db")).unwrap();
    assert!(connection.prepare("SELECT slot FROM active_slots").is_ok());
    assert!(connection.prepare("SELECT * FROM active_campaign").is_err());
}

#[test]
fn rejects_legacy_revision_directories_even_without_a_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    std::fs::create_dir_all(root.join("packages/alpha/deadbeef")).unwrap();
    std::fs::write(root.join("packages/alpha/deadbeef/manifest.json"), b"{}").unwrap();

    let error = Store::open_for_tests(&root)
        .err()
        .expect("legacy package layout was accepted");
    assert_eq!(error.code(), "unsupported_store_format");
    assert!(!root.join("ledger.db").exists());
}

#[test]
fn one_manifest_reimport_is_content_addressed_and_metadata_independent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"same payload");
    let mut first_plan = plan(&source);
    first_plan.metadata = Some(LegacyMetadata {
        title: Some("First title".into()),
        ..LegacyMetadata::default()
    });
    let alpha = PackageId::parse("alpha").unwrap();
    let first = store.ingest(&alpha, SlotId::LotV, &first_plan).unwrap();

    let mut second_plan = first_plan.clone();
    second_plan.metadata.as_mut().unwrap().title = Some("Changed metadata".into());
    let second = store.ingest(&alpha, SlotId::LotV, &second_plan).unwrap();
    let beta = store
        .ingest(
            &PackageId::parse("beta").unwrap(),
            SlotId::LotV,
            &second_plan,
        )
        .unwrap();
    let other_faction = store
        .ingest(
            &PackageId::parse("gamma").unwrap(),
            SlotId::Wol,
            &second_plan,
        )
        .unwrap();

    assert_eq!(first, second);
    assert_eq!(first, beta, "package id must not affect the revision");
    assert_ne!(first, other_faction, "faction must affect the revision");
    assert_eq!(
        store.load_manifest(&alpha).unwrap().title.as_deref(),
        Some("Changed metadata")
    );
    assert!(store.root().join("packages/alpha/manifest.json").is_file());
    assert_eq!(
        std::fs::read_dir(store.root().join("packages/alpha"))
            .unwrap()
            .count(),
        1
    );

    let manifest_path = store.root().join("packages/alpha/manifest.json");
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    json["imported_at"] = serde_json::json!(1);
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    let reopened = Store::open_for_tests(store.root()).unwrap();
    assert_eq!(reopened.load_manifest(&alpha).unwrap().revision, first);
}

#[test]
fn active_package_reimport_and_removal_require_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let id = PackageId::parse("alpha").unwrap();
    let revision = store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap();
    store
        .set_active_campaign(&ActiveCampaign {
            id: id.clone(),
            revision: revision.clone(),
            faction: SlotId::LotV,
        })
        .unwrap();

    std::fs::write(source.join("campaign.SC2Map/payload.bin"), b"replacement").unwrap();
    let reimport_error = store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap_err();
    assert_eq!(reimport_error.code(), "active_package_requires_restore");
    let removal_error = store.remove_package(&id).unwrap_err();
    assert_eq!(removal_error.code(), "active_package_requires_restore");
    assert_eq!(store.load_manifest(&id).unwrap().revision, revision);
    assert!(store.root().join("packages/alpha/manifest.json").is_file());
}

#[test]
fn removing_a_duplicate_package_preserves_the_active_shared_deployment() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"shared payload");
    let alpha = PackageId::parse("alpha").unwrap();
    let beta = PackageId::parse("beta").unwrap();
    let revision = store.ingest(&alpha, SlotId::LotV, &plan(&source)).unwrap();
    assert_eq!(
        store.ingest(&beta, SlotId::LotV, &plan(&source)).unwrap(),
        revision
    );
    store
        .set_active_campaign(&ActiveCampaign {
            id: alpha.clone(),
            revision: revision.clone(),
            faction: SlotId::LotV,
        })
        .unwrap();

    let deployment = store.deploy_dir(SlotId::LotV, &revision).unwrap();
    std::fs::create_dir_all(&deployment).unwrap();
    std::fs::write(deployment.join("sentinel"), b"active target").unwrap();

    store.remove_package(&beta).unwrap();

    assert!(store.load_manifest(&alpha).is_ok());
    assert_eq!(
        std::fs::read(deployment.join("sentinel")).unwrap(),
        b"active target"
    );

    store.clear_active_campaign().unwrap();
    store.remove_package(&alpha).unwrap();
    assert!(!deployment.exists());
}

#[test]
fn pending_operation_blocks_every_package_storage_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let alpha = PackageId::parse("alpha").unwrap();
    let revision = store.ingest(&alpha, SlotId::LotV, &plan(&source)).unwrap();
    let target = ActiveCampaign {
        id: alpha.clone(),
        revision,
        faction: SlotId::LotV,
    };
    PendingOperation::new_preparing(
        "pending-storage-test".into(),
        OperationKind::Activate,
        None,
        Some(target),
        OperationPaths::default(),
    )
    .persist(store.root())
    .unwrap();

    let beta = PackageId::parse("beta").unwrap();
    assert_eq!(
        store
            .ingest(&beta, SlotId::LotV, &plan(&source))
            .unwrap_err()
            .code(),
        "recovery_required"
    );
    assert_eq!(
        store
            .set_metadata(&alpha, "title", "", "", "")
            .unwrap_err()
            .code(),
        "recovery_required"
    );
    assert_eq!(
        store.remove_package(&alpha).unwrap_err().code(),
        "recovery_required"
    );
    assert_eq!(store.gc().unwrap_err().code(), "recovery_required");
    assert!(store.root().join("packages/alpha/manifest.json").is_file());
    assert!(!store.root().join("packages/beta").exists());
}

#[test]
fn active_campaign_and_managed_mods_commit_together() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    map_container(&source.join("Mods/Borrowed.SC2Mod"), b"borrowed");
    map_container(&source.join("Mods/Shared.SC2Mod"), b"created");
    let id = PackageId::parse("alpha").unwrap();
    let revision = store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let campaign = ActiveCampaign {
        id,
        revision,
        faction: SlotId::LotV,
    };
    let managed: Vec<ManagedMod> = manifest
        .files
        .iter()
        .filter_map(|file| {
            file.path.strip_prefix("mods/").map(|path| ManagedMod {
                path: path.into(),
                sha256: file.sha256.clone(),
                disposition: if path.starts_with("Borrowed") {
                    ManagedModDisposition::Borrowed
                } else {
                    ManagedModDisposition::Created
                },
            })
        })
        .collect();
    store
        .commit_active_state(Some(&campaign), &managed)
        .unwrap();
    assert_eq!(store.active_campaign().unwrap(), Some(campaign));
    assert_eq!(store.managed_mods().unwrap(), managed);

    store.clear_active_campaign().unwrap();
    assert!(store.active_campaign().unwrap().is_none());
    assert!(store.managed_mods().unwrap().is_empty());
}

#[test]
fn active_state_commit_rejects_an_incomplete_mods_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    map_container(&source.join("Mods/Required.SC2Mod"), b"required");
    let id = PackageId::parse("alpha").unwrap();
    let revision = store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap();
    let campaign = ActiveCampaign {
        id,
        revision,
        faction: SlotId::LotV,
    };

    let error = store.commit_active_state(Some(&campaign), &[]).unwrap_err();
    assert_eq!(error.code(), "managed_mods_manifest_mismatch");
    assert!(store.active_campaign().unwrap().is_none());
}

#[test]
fn corrupt_inventory_blocks_gc_before_any_blob_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    ingest(&store, &source, "alpha", SlotId::LotV);

    let orphan_hash = "f".repeat(64);
    let orphan = root.join("blobs/ff").join(&orphan_hash);
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"orphan").unwrap();
    std::fs::write(root.join("packages/alpha/manifest.json"), b"broken").unwrap();

    let inventory = store.inventory().unwrap();
    assert!(inventory.packages.is_empty());
    assert_eq!(inventory.corrupt.len(), 1);
    let error = store.gc().unwrap_err();
    assert_eq!(error.code(), "corrupt_package_inventory");
    assert!(orphan.is_file());
}

#[test]
fn oversized_manifest_is_rejected_before_deserialization() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let package = root.join("packages/alpha");
    std::fs::create_dir(&package).unwrap();
    let manifest = std::fs::File::create(package.join("manifest.json")).unwrap();
    manifest.set_len(17 * 1024 * 1024).unwrap();
    drop(manifest);
    let id = PackageId::parse("alpha").unwrap();

    assert_eq!(
        store.load_manifest(&id).unwrap_err().code(),
        "manifest_size_limit"
    );
    let inventory = store.inventory().unwrap();
    assert_eq!(inventory.corrupt.len(), 1);
    assert_eq!(inventory.corrupt[0].code, "manifest_size_limit");
}

#[test]
fn ingestion_cancels_during_a_large_file_and_cleans_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(
        &source.join("campaign.SC2Map"),
        &vec![5_u8; 9 * 1024 * 1024],
    );
    let id = PackageId::parse("alpha").unwrap();
    let mut checks = 0;
    let result = store
        .ingest_with_progress(&id, SlotId::LotV, &plan(&source), |_| {
            checks += 1;
            checks < 3
        })
        .unwrap();
    assert_eq!(result, None);
    assert_eq!(checks, 3);
    assert!(!root.join("packages/alpha/manifest.json").exists());
    assert_eq!(
        std::fs::read_dir(root.join("blob-staging"))
            .unwrap()
            .count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn ingestion_reports_and_retains_an_unsafe_staging_cleanup_target() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let external = tmp.path().join("external-staging-sentinel");
    std::fs::write(&external, b"keep").unwrap();
    let id = PackageId::parse("alpha").unwrap();
    let mut staged_link = None;

    let error = store
        .ingest_with_progress(&id, SlotId::LotV, &plan(&source), |_| {
            if staged_link.is_none() {
                let temporary = std::fs::read_dir(root.join("blob-staging"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                std::fs::remove_file(&temporary).unwrap();
                std::os::unix::fs::symlink(&external, &temporary).unwrap();
                staged_link = Some(temporary);
            }
            false
        })
        .unwrap_err();

    assert_eq!(error.code(), "unsafe_store_path");
    assert_eq!(std::fs::read(&external).unwrap(), b"keep");
    let staged_link = staged_link.unwrap();
    assert!(std::fs::symlink_metadata(&staged_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!root.join("packages/alpha/manifest.json").exists());

    // Cleanup is explicit because retaining the suspicious path is the safety
    // behavior under test.
    std::fs::remove_file(staged_link).unwrap();
}

#[test]
fn materialization_replaces_both_file_kinds() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let unpacked = tmp.path().join("unpacked");
    map_container(&unpacked.join("campaign.SC2Map"), b"map");
    std::fs::create_dir_all(unpacked.join("Mods/Thing.SC2Mod")).unwrap();
    std::fs::write(unpacked.join("Mods/Thing.SC2Mod/member"), b"tree").unwrap();
    let id = PackageId::parse("alpha").unwrap();
    store.ingest(&id, SlotId::LotV, &plan(&unpacked)).unwrap();
    let destination = tmp.path().join("Mods");
    std::fs::create_dir_all(&destination).unwrap();
    std::fs::write(destination.join("Thing.SC2Mod"), b"old packed").unwrap();
    store
        .materialize_mods(&store.load_manifest(&id).unwrap(), &destination)
        .unwrap();
    assert_eq!(
        std::fs::read(destination.join("Thing.SC2Mod/member")).unwrap(),
        b"tree"
    );

    let packed = tmp.path().join("packed");
    map_container(&packed.join("campaign.SC2Map"), b"map");
    std::fs::create_dir_all(packed.join("Mods")).unwrap();
    std::fs::write(packed.join("Mods/Thing.SC2Mod"), b"packed").unwrap();
    store.ingest(&id, SlotId::LotV, &plan(&packed)).unwrap();
    store
        .materialize_mods(&store.load_manifest(&id).unwrap(), &destination)
        .unwrap();
    assert_eq!(
        std::fs::read(destination.join("Thing.SC2Mod")).unwrap(),
        b"packed"
    );
}

#[test]
fn inventory_reports_unsafe_directory_aliases() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    std::fs::create_dir_all(root.join("packages/Alpha")).unwrap();
    std::fs::write(root.join("packages/Alpha/manifest.json"), b"{}").unwrap();
    let inventory = store.inventory().unwrap();
    assert_eq!(inventory.corrupt.len(), 1);
    assert_eq!(inventory.corrupt[0].code, "invalid_package_id");
}

#[cfg(any(unix, windows))]
#[test]
fn open_rejects_linked_store_and_owned_directories_without_touching_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let external = tmp.path().join("external-root");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    let linked_root = tmp.path().join("linked-store");
    create_directory_link(&external, &linked_root);

    let error = Store::open_for_tests(&linked_root)
        .err()
        .expect("linked store root was accepted");
    assert_eq!(error.code(), "unsafe_store_path");
    assert_sentinel_only(&external);

    for child in ["packages", "blobs", "blob-staging", "deploy"] {
        let case = tempfile::tempdir().unwrap();
        let root = case.path().join("store");
        let external = case.path().join("external");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        create_directory_link(&external, &root.join(child));

        let error = Store::open_for_tests(&root)
            .err()
            .unwrap_or_else(|| panic!("linked `{child}` directory was accepted"));
        assert_eq!(error.code(), "unsafe_store_path", "child `{child}`");
        assert_sentinel_only(&external);
    }
}

#[cfg(unix)]
#[test]
fn open_rejects_a_linked_ledger_without_modifying_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let external_ledger = tmp.path().join("external.db");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(&external_ledger, b"not sqlite; keep exactly").unwrap();
    std::os::unix::fs::symlink(&external_ledger, root.join("ledger.db")).unwrap();

    let error = Store::open_for_tests(&root)
        .err()
        .expect("linked ledger was accepted");
    assert_eq!(error.code(), "unsafe_store_path");
    assert_eq!(
        std::fs::read(&external_ledger).unwrap(),
        b"not sqlite; keep exactly"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn linked_package_directory_blocks_cached_reads_writes_gc_and_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let id = PackageId::parse("alpha").unwrap();
    store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap();
    store.load_manifest(&id).unwrap();

    let package = root.join("packages/alpha");
    let package_backup = root.join("alpha-package-backup");
    std::fs::rename(&package, &package_backup).unwrap();
    let external = tmp.path().join("external-package");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external, &package);

    assert_eq!(
        store.load_manifest(&id).unwrap_err().code(),
        "unsafe_store_path"
    );
    assert_eq!(
        store
            .set_metadata(&id, "title", "", "", "")
            .unwrap_err()
            .code(),
        "unsafe_store_path"
    );
    assert_eq!(
        store
            .ingest(&id, SlotId::LotV, &plan(&source))
            .unwrap_err()
            .code(),
        "unsafe_store_path"
    );
    let inventory = store.inventory().unwrap();
    assert_eq!(inventory.corrupt.len(), 1);
    assert_eq!(inventory.corrupt[0].code, "corrupt_package_directory");
    assert_eq!(store.gc().unwrap_err().code(), "corrupt_package_inventory");
    assert_eq!(
        store.remove_package(&id).unwrap_err().code(),
        "corrupt_package_inventory"
    );
    assert_sentinel_only(&external);
    assert!(package_backup.join("manifest.json").is_file());
}

#[cfg(any(unix, windows))]
#[test]
fn linked_blob_shard_blocks_reads_writes_and_fail_closed_gc() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let alpha = PackageId::parse("alpha").unwrap();
    store.ingest(&alpha, SlotId::LotV, &plan(&source)).unwrap();
    let manifest = store.load_manifest(&alpha).unwrap();
    let hash = &manifest.files[0].sha256;
    let shard_name = &hash[..2];
    let shard = root.join("blobs").join(shard_name);
    std::fs::rename(&shard, root.join("blob-shard-backup")).unwrap();

    let external = tmp.path().join("external-shard");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external, &shard);

    let orphan_prefix = if shard_name == "00" { "ff" } else { "00" };
    let orphan_hash = format!("{orphan_prefix}{}", "0".repeat(62));
    let orphan = root.join("blobs").join(orphan_prefix).join(orphan_hash);
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"orphan").unwrap();

    assert_eq!(
        store.verify_package(&alpha).unwrap_err().code(),
        "unsafe_store_path"
    );
    assert_eq!(
        store
            .ingest(
                &PackageId::parse("beta").unwrap(),
                SlotId::LotV,
                &plan(&source),
            )
            .unwrap_err()
            .code(),
        "unsafe_store_path"
    );
    assert_eq!(store.gc().unwrap_err().code(), "corrupt_blob_store");
    assert!(
        orphan.is_file(),
        "GC deleted a blob before completing its scan"
    );
    assert_sentinel_only(&external);
}

#[cfg(any(unix, windows))]
#[test]
fn linked_blob_staging_blocks_ingest_without_external_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let external = tmp.path().join("external-staging");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external, &root.join("blob-staging"));
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");

    let error = store
        .ingest(
            &PackageId::parse("alpha").unwrap(),
            SlotId::LotV,
            &plan(&source),
        )
        .unwrap_err();
    assert_eq!(error.code(), "unsafe_store_path");
    assert_sentinel_only(&external);
    assert!(!root.join("packages/alpha").exists());
}

#[cfg(any(unix, windows))]
#[test]
fn linked_materialization_root_is_rejected_without_touching_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(tmp.path().join("store")).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let id = PackageId::parse("alpha").unwrap();
    store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap();
    let manifest = store.load_manifest(&id).unwrap();
    let external = tmp.path().join("external-materialization");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    let destination = tmp.path().join("linked-materialization");
    create_directory_link(&external, &destination);

    let error = store.materialize_slot(&manifest, &destination).unwrap_err();
    assert_eq!(error.code(), "unsafe_store_path");
    assert_sentinel_only(&external);
}

#[cfg(any(unix, windows))]
#[test]
fn linked_deploy_root_and_tree_block_removal_without_external_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let id = PackageId::parse("alpha").unwrap();
    let revision = store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap();

    let external_root = tmp.path().join("external-deploy-root");
    std::fs::create_dir(&external_root).unwrap();
    std::fs::write(external_root.join("sentinel"), b"keep").unwrap();
    let deploy = root.join("deploy");
    create_directory_link(&external_root, &deploy);
    assert_eq!(
        store
            .deploy_dir(SlotId::LotV, &revision)
            .unwrap_err()
            .code(),
        "unsafe_store_path"
    );
    assert_eq!(
        store.remove_package(&id).unwrap_err().code(),
        "unsafe_store_path"
    );
    assert_sentinel_only(&external_root);
    assert!(root.join("packages/alpha/manifest.json").is_file());

    remove_directory_link(&deploy);
    std::fs::create_dir(&deploy).unwrap();
    let external_tree = tmp.path().join("external-deploy-tree");
    std::fs::create_dir(&external_tree).unwrap();
    std::fs::write(external_tree.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external_tree, &deploy.join(format!("lotv-{revision}")));
    assert_eq!(
        store.remove_package(&id).unwrap_err().code(),
        "unsafe_store_path"
    );
    assert_sentinel_only(&external_tree);
    assert!(root.join("packages/alpha/manifest.json").is_file());

    remove_directory_link(&deploy.join(format!("lotv-{revision}")));
    let deployment_tree = deploy.join(format!("lotv-{revision}"));
    std::fs::create_dir(&deployment_tree).unwrap();
    let external_nested = tmp.path().join("external-nested-deploy-tree");
    std::fs::create_dir(&external_nested).unwrap();
    std::fs::write(external_nested.join("sentinel"), b"keep").unwrap();
    let nested_link = deployment_tree.join("substituted");
    create_directory_link(&external_nested, &nested_link);
    assert_eq!(
        store.remove_package(&id).unwrap_err().code(),
        "unsafe_store_path"
    );
    assert_sentinel_only(&external_nested);
    assert!(root.join("packages/alpha/manifest.json").is_file());
    remove_directory_link(&nested_link);
}

#[cfg(any(unix, windows))]
#[test]
fn unexpected_link_inside_package_directory_blocks_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    let id = PackageId::parse("alpha").unwrap();
    store.ingest(&id, SlotId::LotV, &plan(&source)).unwrap();

    let external = tmp.path().join("external-package-entry");
    std::fs::create_dir(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    let nested_link = root.join("packages/alpha/unexpected");
    create_directory_link(&external, &nested_link);

    let inventory = store.inventory().unwrap();
    assert_eq!(inventory.corrupt.len(), 1);
    assert_eq!(inventory.corrupt[0].code, "corrupt_package_directory");
    assert_eq!(
        store.remove_package(&id).unwrap_err().code(),
        "corrupt_package_inventory"
    );
    assert_sentinel_only(&external);
    assert!(root.join("packages/alpha/manifest.json").is_file());
    remove_directory_link(&nested_link);
}

#[cfg(unix)]
#[test]
fn linked_blob_file_blocks_gc_before_deleting_a_real_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let orphan_hash = format!("00{}", "0".repeat(62));
    let orphan = root.join("blobs/00").join(&orphan_hash);
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"orphan").unwrap();

    let external = tmp.path().join("external-blob");
    std::fs::write(&external, b"keep").unwrap();
    let linked_hash = format!("ff{}", "f".repeat(62));
    let linked_blob = root.join("blobs/ff").join(linked_hash);
    std::fs::create_dir_all(linked_blob.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&external, &linked_blob).unwrap();

    assert_eq!(store.gc().unwrap_err().code(), "corrupt_blob_store");
    assert!(orphan.is_file());
    assert_eq!(std::fs::read(&external).unwrap(), b"keep");
}

#[test]
fn ingest_rejects_a_case_alias_before_creating_the_requested_package() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let alias = root.join("packages/Alpha");
    std::fs::create_dir(&alias).unwrap();
    std::fs::write(alias.join("sentinel"), b"keep").unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");

    let error = store
        .ingest(
            &PackageId::parse("alpha").unwrap(),
            SlotId::LotV,
            &plan(&source),
        )
        .unwrap_err();
    assert_eq!(error.code(), "package_id_case_alias");
    let package_names = std::fs::read_dir(root.join("packages"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(package_names, [std::ffi::OsString::from("Alpha")]);
    assert_eq!(std::fs::read(alias.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn ingest_rejects_dos_device_segments_before_persisting_a_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let store = Store::open_for_tests(&root).unwrap();
    let source = tmp.path().join("source");
    map_container(&source.join("campaign.SC2Map"), b"payload");
    std::fs::write(source.join("campaign.SC2Map/CON.txt"), b"reserved").unwrap();

    let error = store
        .ingest(
            &PackageId::parse("alpha").unwrap(),
            SlotId::LotV,
            &plan(&source),
        )
        .unwrap_err();
    assert_eq!(error.code(), "invalid_package");
    assert!(!root.join("packages/alpha/manifest.json").exists());
}

#[cfg(unix)]
#[test]
fn ingest_rejects_windows_invalid_manifest_segments() {
    for segment in [
        "bad<name",
        "bad>name",
        "bad:name",
        "bad\"name",
        "bad|name",
        "bad?name",
        "bad*name",
        "bad\u{1f}name",
        "bad\u{7f}name",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let store = Store::open_for_tests(&root).unwrap();
        let source = tmp.path().join("source");
        map_container(&source.join("campaign.SC2Map"), b"payload");
        std::fs::write(source.join("campaign.SC2Map").join(segment), b"invalid").unwrap();

        let error = store
            .ingest(
                &PackageId::parse("alpha").unwrap(),
                SlotId::LotV,
                &plan(&source),
            )
            .unwrap_err();
        assert_eq!(error.code(), "invalid_package", "segment `{segment:?}`");
        assert!(!root.join("packages/alpha/manifest.json").exists());
    }
}
