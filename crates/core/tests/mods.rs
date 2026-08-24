//! Managed Mods ownership, conflicts, replacement, and rollback.

use std::path::Path;

use svccm_core::identity::PackageId;
use svccm_core::mods::{ExternalModsPolicy, PreparedModsTransition};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::{ManagedMod, ManagedModDisposition, PackageManifest, Store};

fn make_map(root: &Path, name: &str) {
    let map = root.join(format!("Maps/campaign/{name}.SC2Map"));
    std::fs::create_dir_all(&map).unwrap();
    std::fs::write(map.join("payload"), name.as_bytes()).unwrap();
}

fn make_packed_mod(root: &Path, name: &str, payload: &[u8]) {
    std::fs::create_dir_all(root.join("Mods")).unwrap();
    std::fs::write(root.join(format!("Mods/{name}.SC2Mod")), payload).unwrap();
}

fn make_unpacked_mod(root: &Path, name: &str, payload: &[u8]) {
    let container = root.join(format!("Mods/{name}.SC2Mod"));
    std::fs::create_dir_all(&container).unwrap();
    std::fs::write(container.join("member"), payload).unwrap();
}

fn ingest(store: &Store, source: &Path, id: &str) -> PackageManifest {
    let id = PackageId::parse(id).unwrap();
    let plan = plan_from_extracted(source).unwrap();
    store
        .ingest(&id, svccm_core::layout::SlotId::LotV, &plan)
        .unwrap();
    store.load_manifest(&id).unwrap()
}

fn deploy(
    store: &Store,
    mods_root: &Path,
    previous: &[ManagedMod],
    target: &PackageManifest,
    operation_id: &str,
) -> Vec<ManagedMod> {
    let transition =
        PreparedModsTransition::prepare(store, mods_root, previous, Some(target), operation_id)
            .unwrap();
    transition.apply().unwrap();
    let rows = transition.target_rows().to_vec();
    transition.finalize().unwrap();
    rows
}

#[test]
fn fully_owned_unpacked_mod_is_moved_from_staging_and_rolls_back() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_unpacked_mod(&source, "Loose", b"member bytes");
    let manifest = ingest(&store, &source, "move-loose");
    let mods_root = temp.path().join("Mods");

    let transition =
        PreparedModsTransition::prepare(&store, &mods_root, &[], Some(&manifest), "move-loose")
            .unwrap();
    let staged = transition.staging_path().join("Loose.SC2Mod");
    assert!(staged.join("member").is_file());

    transition.apply().unwrap();

    assert!(!staged.exists(), "the complete container should be renamed");
    assert_eq!(
        std::fs::read(mods_root.join("Loose.SC2Mod/member")).unwrap(),
        b"member bytes"
    );
    transition.rollback().unwrap();
    assert!(!mods_root.join("Loose.SC2Mod").exists());
}

#[test]
fn fully_owned_packed_mod_is_moved_from_staging_and_finalizes() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Packed", b"archive bytes");
    let manifest = ingest(&store, &source, "move-packed");
    let mods_root = temp.path().join("Mods");

    let transition =
        PreparedModsTransition::prepare(&store, &mods_root, &[], Some(&manifest), "move-packed")
            .unwrap();
    let staged = transition.staging_path().join("Packed.SC2Mod");
    assert!(staged.is_file());

    transition.apply().unwrap();

    assert!(!staged.exists(), "the packed Mod should be renamed");
    assert_eq!(
        std::fs::read(mods_root.join("Packed.SC2Mod")).unwrap(),
        b"archive bytes"
    );
    transition.finalize().unwrap();
}

#[test]
fn multiple_owned_mod_containers_are_each_moved_from_staging() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_unpacked_mod(&source, "First", b"first bytes");
    make_unpacked_mod(&source, "Second", b"second bytes");
    let manifest = ingest(&store, &source, "move-multiple");
    let mods_root = temp.path().join("Mods");

    let transition =
        PreparedModsTransition::prepare(&store, &mods_root, &[], Some(&manifest), "move-multiple")
            .unwrap();

    transition.apply().unwrap();

    assert_eq!(
        std::fs::read(mods_root.join("First.SC2Mod/member")).unwrap(),
        b"first bytes"
    );
    assert_eq!(
        std::fs::read(mods_root.join("Second.SC2Mod/member")).unwrap(),
        b"second bytes"
    );
    transition.rollback().unwrap();
}

#[test]
fn fully_owned_unpacked_mod_moves_to_backup_only_when_restore_applies() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_unpacked_mod(&source, "Loose", b"member bytes");
    let manifest = ingest(&store, &source, "restore-move-loose");
    let mods_root = temp.path().join("Mods");
    let rows = deploy(&store, &mods_root, &[], &manifest, "deploy-loose");

    let transition =
        PreparedModsTransition::prepare(&store, &mods_root, &rows, None, "restore-loose").unwrap();
    let backup = transition.backup_path().join("files/Loose.SC2Mod");
    assert!(mods_root.join("Loose.SC2Mod/member").is_file());
    assert!(
        !backup.exists(),
        "prepare must not copy a complete owned container"
    );

    transition.apply().unwrap();

    assert!(!mods_root.join("Loose.SC2Mod").exists());
    assert_eq!(
        std::fs::read(backup.join("member")).unwrap(),
        b"member bytes"
    );
    transition.rollback().unwrap();
    assert_eq!(
        std::fs::read(mods_root.join("Loose.SC2Mod/member")).unwrap(),
        b"member bytes"
    );
}

#[test]
fn fully_owned_same_name_container_swaps_by_rename_and_rolls_back() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let first_source = temp.path().join("first-source");
    make_map(&first_source, "first");
    make_unpacked_mod(&first_source, "Shared", b"first bytes");
    let first = ingest(&store, &first_source, "first-container");
    let second_source = temp.path().join("second-source");
    make_map(&second_source, "second");
    make_unpacked_mod(&second_source, "Shared", b"second bytes");
    let second = ingest(&store, &second_source, "second-container");
    let mods_root = temp.path().join("Mods");
    let first_rows = deploy(&store, &mods_root, &[], &first, "deploy-first");

    let transition = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &first_rows,
        Some(&second),
        "swap-container",
    )
    .unwrap();
    let backup = transition.backup_path().join("files/Shared.SC2Mod");
    let staged = transition.staging_path().join("Shared.SC2Mod");
    assert!(!backup.exists());
    assert_eq!(
        std::fs::read(staged.join("member")).unwrap(),
        b"second bytes"
    );

    transition.apply().unwrap();

    assert_eq!(
        std::fs::read(mods_root.join("Shared.SC2Mod/member")).unwrap(),
        b"second bytes"
    );
    assert_eq!(
        std::fs::read(backup.join("member")).unwrap(),
        b"first bytes"
    );
    assert!(!staged.exists());
    transition.rollback().unwrap();
    assert_eq!(
        std::fs::read(mods_root.join("Shared.SC2Mod/member")).unwrap(),
        b"first bytes"
    );
}

#[test]
fn extra_external_file_disables_container_move_and_survives_restore() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_unpacked_mod(&source, "Loose", b"member bytes");
    let manifest = ingest(&store, &source, "extra-file");
    let mods_root = temp.path().join("Mods");
    let rows = deploy(&store, &mods_root, &[], &manifest, "deploy-extra");
    let external = mods_root.join("Loose.SC2Mod/user-file");
    std::fs::write(&external, b"keep me").unwrap();

    let transition =
        PreparedModsTransition::prepare(&store, &mods_root, &rows, None, "restore-extra").unwrap();
    assert!(
        transition
            .backup_path()
            .join("files/Loose.SC2Mod/member")
            .is_file(),
        "mixed container must retain the file-level fallback"
    );

    transition.apply().unwrap();
    transition.finalize().unwrap();

    assert_eq!(std::fs::read(external).unwrap(), b"keep me");
    assert!(!mods_root.join("Loose.SC2Mod/member").exists());
}

#[test]
fn mixed_borrowed_container_uses_per_file_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    let container = source.join("Mods/Mixed.SC2Mod");
    std::fs::create_dir_all(&container).unwrap();
    std::fs::write(container.join("borrowed"), b"external bytes").unwrap();
    std::fs::write(container.join("created"), b"package bytes").unwrap();
    let manifest = ingest(&store, &source, "mixed-container");
    let mods_root = temp.path().join("Mods");
    let external = mods_root.join("Mixed.SC2Mod/borrowed");
    std::fs::create_dir_all(external.parent().unwrap()).unwrap();
    std::fs::write(&external, b"external bytes").unwrap();

    let transition = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &[],
        Some(&manifest),
        "mixed-container",
    )
    .unwrap();
    let staged = transition.staging_path().join("Mixed.SC2Mod");

    transition.apply().unwrap();

    assert!(
        staged.join("created").is_file(),
        "mixed ownership must keep the per-file copy fallback"
    );
    assert_eq!(std::fs::read(&external).unwrap(), b"external bytes");
    assert_eq!(
        std::fs::read(mods_root.join("Mixed.SC2Mod/created")).unwrap(),
        b"package bytes"
    );
    transition.rollback().unwrap();
    assert_eq!(std::fs::read(external).unwrap(), b"external bytes");
    assert!(!mods_root.join("Mixed.SC2Mod/created").exists());
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

#[test]
fn identical_external_mod_is_borrowed_and_survives_restore() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Shared", b"external dependency");
    let manifest = ingest(&store, &source, "borrowed");
    let mods_root = temp.path().join("Mods");
    std::fs::create_dir_all(&mods_root).unwrap();
    let external = mods_root.join("Shared.SC2Mod");
    std::fs::write(&external, b"external dependency").unwrap();

    let rows = deploy(&store, &mods_root, &[], &manifest, "borrow-deploy");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].disposition, ManagedModDisposition::Borrowed);

    let restore =
        PreparedModsTransition::prepare(&store, &mods_root, &rows, None, "borrow-restore").unwrap();
    restore.apply().unwrap();
    assert!(restore.target_rows().is_empty());
    restore.finalize().unwrap();
    assert_eq!(std::fs::read(external).unwrap(), b"external dependency");
}

#[test]
fn different_unowned_mod_is_rejected_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Shared", b"package bytes");
    let manifest = ingest(&store, &source, "conflict");
    let mods_root = temp.path().join("Mods");
    std::fs::create_dir_all(&mods_root).unwrap();
    let external = mods_root.join("Shared.SC2Mod");
    std::fs::write(&external, b"user bytes").unwrap();

    let error = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &[],
        Some(&manifest),
        "conflict-deploy",
    )
    .unwrap_err();
    assert_eq!(error.code(), "external_mods_conflict");
    assert_eq!(std::fs::read(external).unwrap(), b"user bytes");
}

#[test]
fn external_conflict_is_reported_before_target_mods_are_materialized() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Golden", b"package bytes");
    let manifest = ingest(&store, &source, "conflict-order");
    let mods_root = temp.path().join("Mods");
    std::fs::create_dir_all(&mods_root).unwrap();
    std::fs::write(mods_root.join("Golden.SC2Mod"), b"external bytes").unwrap();

    let mod_file = manifest
        .files
        .iter()
        .find(|file| file.path == "mods/Golden.SC2Mod")
        .unwrap();
    std::fs::remove_file(
        store
            .root()
            .join("blobs")
            .join(&mod_file.sha256[..2])
            .join(&mod_file.sha256),
    )
    .unwrap();

    let error = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &[],
        Some(&manifest),
        "conflict-before-staging",
    )
    .unwrap_err();

    assert_eq!(error.code(), "external_mods_conflict");
    assert_eq!(
        std::fs::read(mods_root.join("Golden.SC2Mod")).unwrap(),
        b"external bytes"
    );
}

#[test]
fn explicit_external_mod_permission_replaces_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Golden", b"package bytes");
    let manifest = ingest(&store, &source, "replace-external");
    let mods_root = temp.path().join("Mods");
    std::fs::create_dir_all(&mods_root).unwrap();
    let external = mods_root.join("Golden.SC2Mod");
    std::fs::write(&external, b"external bytes").unwrap();

    let transition = PreparedModsTransition::prepare_with_policy(
        &store,
        &mods_root,
        &[],
        Some(&manifest),
        "replace-external",
        ExternalModsPolicy::Replace,
    )
    .unwrap();
    transition.apply().unwrap();
    assert_eq!(std::fs::read(&external).unwrap(), b"package bytes");
    transition.finalize().unwrap();
}

#[test]
fn failed_external_mod_replacement_rolls_the_original_file_back() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Golden", b"package bytes");
    let manifest = ingest(&store, &source, "rollback-external");
    let mods_root = temp.path().join("Mods");
    std::fs::create_dir_all(&mods_root).unwrap();
    let external = mods_root.join("Golden.SC2Mod");
    std::fs::write(&external, b"external bytes").unwrap();

    let transition = PreparedModsTransition::prepare_with_policy(
        &store,
        &mods_root,
        &[],
        Some(&manifest),
        "rollback-external",
        ExternalModsPolicy::Replace,
    )
    .unwrap();
    transition.apply().unwrap();
    transition.rollback().unwrap();

    assert_eq!(std::fs::read(external).unwrap(), b"external bytes");
}

#[test]
fn deployment_never_reuses_or_deletes_a_user_file_named_like_an_old_temporary() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Owned", b"package bytes");
    let manifest = ingest(&store, &source, "temporary-safety");
    let mods_root = temp.path().join("Mods");
    std::fs::create_dir_all(&mods_root).unwrap();
    let external = mods_root.join(format!(
        ".Owned.SC2Mod.starvault-tmp-{}",
        std::process::id()
    ));
    std::fs::write(&external, b"user bytes").unwrap();

    deploy(&store, &mods_root, &[], &manifest, "safe-temporary");

    assert_eq!(std::fs::read(&external).unwrap(), b"user bytes");
    assert_eq!(
        std::fs::read(mods_root.join("Owned.SC2Mod")).unwrap(),
        b"package bytes"
    );
}

#[test]
fn changed_created_mod_is_discarded_on_restore_and_rollback_safe() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "Owned", b"managed bytes");
    let manifest = ingest(&store, &source, "owned");
    let mods_root = temp.path().join("Mods");
    let rows = deploy(&store, &mods_root, &[], &manifest, "owned-deploy");
    assert_eq!(rows[0].disposition, ManagedModDisposition::Created);
    let managed = mods_root.join("Owned.SC2Mod");
    std::fs::write(&managed, b"user changed this").unwrap();

    let transition =
        PreparedModsTransition::prepare(&store, &mods_root, &rows, None, "owned-restore").unwrap();
    transition.apply().unwrap();
    assert!(!managed.exists());
    transition.rollback().unwrap();
    assert_eq!(std::fs::read(managed).unwrap(), b"user changed this");
}

#[cfg(any(unix, windows))]
#[test]
fn restore_rejects_a_linked_managed_ancestor_without_touching_or_following_it() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_unpacked_mod(&source, "Shape", b"managed member");
    let manifest = ingest(&store, &source, "linked-ancestor");
    let mods_root = temp.path().join("Mods");
    let rows = deploy(&store, &mods_root, &[], &manifest, "ancestor-deploy");
    let restore =
        PreparedModsTransition::prepare(&store, &mods_root, &rows, None, "ancestor-restore")
            .unwrap();

    let container = mods_root.join("Shape.SC2Mod");
    std::fs::remove_dir_all(&container).unwrap();
    let external = temp.path().join("external-mod");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("member"), b"managed member").unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external, &container);

    let error = restore.apply().unwrap_err();

    assert_eq!(error.code(), "managed_file_changed");
    assert_eq!(
        std::fs::read(external.join("member")).unwrap(),
        b"managed member"
    );
    assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");

    remove_directory_link(&container);
    let rollback_error = restore.rollback().unwrap_err();
    assert_eq!(rollback_error.code(), "recovery_required");
    assert!(!mods_root.join("Shape.SC2Mod").exists());
    assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
}

#[test]
fn transitions_replace_file_with_directory_and_roll_back() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let packed_source = temp.path().join("packed-source");
    make_map(&packed_source, "packed");
    make_packed_mod(&packed_source, "Shape", b"packed bytes");
    let packed = ingest(&store, &packed_source, "packed");
    let unpacked_source = temp.path().join("unpacked-source");
    make_map(&unpacked_source, "unpacked");
    make_unpacked_mod(&unpacked_source, "Shape", b"directory bytes");
    let unpacked = ingest(&store, &unpacked_source, "unpacked");
    let mods_root = temp.path().join("Mods");
    let packed_rows = deploy(&store, &mods_root, &[], &packed, "packed-deploy");

    let transition = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &packed_rows,
        Some(&unpacked),
        "file-to-directory",
    )
    .unwrap();
    transition.apply().unwrap();
    assert_eq!(
        std::fs::read(mods_root.join("Shape.SC2Mod/member")).unwrap(),
        b"directory bytes"
    );
    transition.rollback().unwrap();
    assert_eq!(
        std::fs::read(mods_root.join("Shape.SC2Mod")).unwrap(),
        b"packed bytes"
    );
}

#[test]
fn transitions_replace_directory_with_file_and_roll_back() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let unpacked_source = temp.path().join("unpacked-source");
    make_map(&unpacked_source, "unpacked");
    make_unpacked_mod(&unpacked_source, "Shape", b"directory bytes");
    let unpacked = ingest(&store, &unpacked_source, "unpacked");
    let packed_source = temp.path().join("packed-source");
    make_map(&packed_source, "packed");
    make_packed_mod(&packed_source, "Shape", b"packed bytes");
    let packed = ingest(&store, &packed_source, "packed");
    let mods_root = temp.path().join("Mods");
    let unpacked_rows = deploy(&store, &mods_root, &[], &unpacked, "unpacked-deploy");

    let transition = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &unpacked_rows,
        Some(&packed),
        "directory-to-file",
    )
    .unwrap();
    transition.apply().unwrap();
    assert_eq!(
        std::fs::read(mods_root.join("Shape.SC2Mod")).unwrap(),
        b"packed bytes"
    );
    transition.rollback().unwrap();
    assert_eq!(
        std::fs::read(mods_root.join("Shape.SC2Mod/member")).unwrap(),
        b"directory bytes"
    );
}

#[cfg(unix)]
#[test]
fn linked_mods_root_is_rejected_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "External", b"must not deploy");
    let manifest = ingest(&store, &source, "campaign");
    let external = temp.path().join("external");
    let mods_root = temp.path().join("SC2/Mods");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::create_dir_all(mods_root.parent().unwrap()).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    symlink(&external, &mods_root).unwrap();

    let error = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &[],
        Some(&manifest),
        "linked-mods-root",
    )
    .unwrap_err();

    assert_eq!(error.code(), "unsafe_mods_root");
    assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
    assert!(!external.join("External.SC2Mod").exists());
}

#[cfg(windows)]
#[test]
fn junction_mods_root_is_rejected_without_touching_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(temp.path().join("store")).unwrap();
    let source = temp.path().join("source");
    make_map(&source, "campaign");
    make_packed_mod(&source, "External", b"must not deploy");
    let manifest = ingest(&store, &source, "campaign");
    let external = temp.path().join("external");
    let mods_root = temp.path().join("SC2/Mods");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::create_dir_all(mods_root.parent().unwrap()).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    junction::create(&external, &mods_root).unwrap();

    let error = PreparedModsTransition::prepare(
        &store,
        &mods_root,
        &[],
        Some(&manifest),
        "junction-mods-root",
    )
    .unwrap_err();

    assert_eq!(error.code(), "unsafe_mods_root");
    assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
    assert!(!external.join("External.SC2Mod").exists());
}
