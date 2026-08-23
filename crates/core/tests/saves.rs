use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use svccm_core::identity::{PackageId, ProfileId};
use svccm_core::layout::SlotId;
use svccm_core::saves::{
    create_recovery_backup, create_recovery_backup_with, discover, is_onedrive,
    is_onedrive_with_roots, resolve_profile, SaveIo, SaveOwner, SaveTransition, SavesManager,
    SystemSaveIo,
};

fn touch(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn read(path: impl AsRef<Path>) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn package(id: &str) -> SaveOwner {
    SaveOwner::Package(PackageId::parse(id).unwrap())
}

fn transition(
    previous_owner: SaveOwner,
    previous_faction: Option<SlotId>,
    target_owner: SaveOwner,
    target_faction: Option<SlotId>,
) -> SaveTransition {
    SaveTransition {
        previous_owner,
        previous_faction,
        target_owner,
        target_faction,
    }
}

fn execute(manager: &SavesManager, change: SaveTransition, operation: &str) {
    let prepared = manager.prepare(change, operation).unwrap();
    prepared.apply().unwrap();
    prepared.finalize().unwrap();
}

fn profile_tree(documents: &Path) -> (PathBuf, PathBuf) {
    let profile = documents.join("StarCraft II/Accounts/120927238/2-S2-1-3475134");
    (profile.join("Saves"), profile.join("Banks"))
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
fn assert_external_sentinel_is_untouched(external: &Path) {
    assert_eq!(read(external.join("sentinel")), b"keep");
    assert_eq!(std::fs::read_dir(external).unwrap().count(), 1);
}

#[test]
fn discovery_returns_opaque_ids_and_resolution_is_fresh() {
    let temporary = tempfile::tempdir().unwrap();
    let documents = temporary.path();
    let (saves, _) = profile_tree(documents);
    touch(&saves.join("LibertyCampaignSave.SC2Save"), b"save");

    let profiles = discover(documents).unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].display_label, "120927238/2-S2-1-3475134");
    assert_eq!(profiles[0].id.as_str().len(), 64);
    assert!(!profiles[0].id.as_str().contains("120927238"));
    assert_eq!(
        resolve_profile(documents, &profiles[0].id)
            .unwrap()
            .saves_dir(),
        saves
    );

    std::fs::remove_dir_all(saves).unwrap();
    let error = resolve_profile(documents, &profiles[0].id).unwrap_err();
    assert_eq!(error.code(), "save_profile_not_found");

    let unknown: ProfileId = serde_json::from_str(&format!("\"{}\"", "0".repeat(64))).unwrap();
    assert_eq!(
        resolve_profile(documents, &unknown).unwrap_err().code(),
        "save_profile_not_found"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn linked_profile_identity_roots_are_rejected_without_traversal() {
    for component in ["account", "profile", "Saves", "Banks"] {
        let temporary = tempfile::tempdir().unwrap();
        let documents = temporary.path();
        let (saves, banks) = profile_tree(documents);
        touch(&saves.join("LibertyCampaignSave.SC2Save"), b"save");
        touch(&banks.join("author/custom.SC2Bank"), b"bank");
        let profile_id = discover(documents).unwrap().remove(0).id;
        let profile = saves.parent().unwrap();
        let linked = match component {
            "account" => profile.parent().unwrap(),
            "profile" => profile,
            "Saves" => saves.as_path(),
            "Banks" => banks.as_path(),
            _ => unreachable!(),
        };
        let original = linked.with_file_name(format!(
            "{}-original",
            linked.file_name().unwrap().to_string_lossy()
        ));
        std::fs::rename(linked, &original).unwrap();
        let external = temporary.path().join(format!("external-{component}"));
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        create_directory_link(&external, linked);

        let error = resolve_profile(documents, &profile_id).unwrap_err();

        assert_eq!(error.code(), "unsafe_save_profile", "{component}");
        assert_external_sentinel_is_untouched(&external);
        assert!(linked.symlink_metadata().is_ok());

        remove_directory_link(linked);
        std::fs::rename(original, linked).unwrap();
    }
}

#[cfg(any(unix, windows))]
#[test]
fn linked_recovery_root_is_rejected_without_writing_external_data() {
    let temporary = tempfile::tempdir().unwrap();
    let documents = temporary.path();
    let (saves, banks) = profile_tree(documents);
    touch(&saves.join("Campaign/save.SC2Save"), b"save");
    touch(&banks.join("author/custom.SC2Bank"), b"bank");
    let profile_id = discover(documents).unwrap().remove(0).id;
    let external = temporary.path().join("external-recovery");
    let recovery = documents.join("StarVault CCM Recovery");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external, &recovery);

    let error = create_recovery_backup(documents, &profile_id, 42).unwrap_err();

    assert_eq!(error.code(), "unsafe_recovery_backup_path");
    assert_external_sentinel_is_untouched(&external);
    assert!(recovery.symlink_metadata().is_ok());

    remove_directory_link(&recovery);
}

#[cfg(any(unix, windows))]
#[test]
fn linked_internal_save_roots_are_rejected_without_external_mutation() {
    for root_name in ["save-operations", "saves"] {
        let temporary = tempfile::tempdir().unwrap();
        let live = temporary.path().join("profile/Saves");
        let store = temporary.path().join("store");
        let external = temporary.path().join(format!("external-{root_name}"));
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        create_directory_link(&external, &store.join(root_name));
        let manager = SavesManager::new(live, &store);

        let error = manager
            .prepare(
                transition(
                    SaveOwner::Plain,
                    None,
                    package("campaign-a"),
                    Some(SlotId::LotV),
                ),
                "linked-internal-root",
            )
            .unwrap_err();

        assert_eq!(error.code(), "unsafe_store_path", "{root_name}");
        assert_external_sentinel_is_untouched(&external);
        assert!(store.join(root_name).symlink_metadata().is_ok());

        remove_directory_link(&store.join(root_name));
    }
}

#[cfg(any(unix, windows))]
#[test]
fn finalize_rejects_a_linked_owner_directory_without_external_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let store = temporary.path().join("store");
    touch(&live.join("VoidCampaignSave.SC2Save"), b"active");
    let manager = SavesManager::new(live, &store);
    let prepared = manager
        .prepare(
            transition(
                package("campaign-a"),
                Some(SlotId::LotV),
                package("campaign-b"),
                Some(SlotId::LotV),
            ),
            "linked-owner-finalize",
        )
        .unwrap();
    prepared.apply().unwrap();
    let owner = store.join("saves/v2/packages/campaign-a");
    let original = store.join("saves/v2/packages/campaign-a-original");
    std::fs::rename(&owner, &original).unwrap();
    let external = temporary.path().join("external-owner");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    create_directory_link(&external, &owner);

    let error = prepared.finalize().unwrap_err();

    assert_eq!(error.code(), "unsafe_store_path");
    assert_external_sentinel_is_untouched(&external);
    assert!(owner.symlink_metadata().is_ok());

    remove_directory_link(&owner);
    std::fs::rename(&original, &owner).unwrap();
    prepared.rollback().unwrap();
}

#[test]
fn onedrive_detection_uses_names_and_shell_roots() {
    assert!(is_onedrive(Path::new(
        "C:/Users/test/OneDrive/Documents/StarCraft II"
    )));
    assert!(is_onedrive(Path::new(
        "C:/Users/test/OneDrive - Acme/Documents"
    )));
    assert!(!is_onedrive(Path::new(
        "C:/Users/test/CloudFiles/Documents"
    )));
    assert!(is_onedrive_with_roots(
        Path::new("C:/Users/test/CloudFiles/Documents"),
        &[PathBuf::from("c:/users/TEST/cloudfiles")]
    ));
    assert!(!is_onedrive_with_roots(
        Path::new("C:/Users/test/CloudFiles2/Documents"),
        &[PathBuf::from("C:/Users/test/CloudFiles")]
    ));
}

#[test]
fn same_faction_a_to_b_round_trips_global_and_root_data() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let banks = temporary.path().join("profile/Banks");
    let store = temporary.path().join("store");
    let manager = SavesManager::new(live.clone(), &store);

    touch(&live.join("VoidCampaignSave.SC2Save"), b"plain-lotv");
    touch(&live.join("SwarmCampaignSave.SC2Save"), b"plain-hots");
    touch(&live.join("Campaign/plain.SC2Save"), b"plain-mission");
    touch(&live.join("Unsaved/plain.SC2Save"), b"plain-auto");
    touch(&live.join("Multiplayer/ladder.SC2Save"), b"shared");
    touch(&banks.join("author/plain.SC2Bank"), b"plain-bank");
    touch(&banks.join("ZCampaignStats.SC2Bank"), b"vanilla-bank");

    execute(
        &manager,
        transition(
            SaveOwner::Plain,
            None,
            package("campaign-a"),
            Some(SlotId::LotV),
        ),
        "plain-to-a",
    );
    assert!(!live.join("VoidCampaignSave.SC2Save").exists());
    assert!(!live.join("Campaign").exists());
    assert_eq!(read(live.join("SwarmCampaignSave.SC2Save")), b"plain-hots");
    assert_eq!(read(live.join("Multiplayer/ladder.SC2Save")), b"shared");
    assert_eq!(read(banks.join("ZCampaignStats.SC2Bank")), b"vanilla-bank");

    touch(&live.join("VoidCampaignSave.SC2Save"), b"a-root");
    touch(&live.join("Campaign/a.SC2Save"), b"a-mission");
    touch(&live.join("Unsaved/a.SC2Save"), b"a-auto");
    touch(&banks.join("author/a.SC2Bank"), b"a-bank");
    execute(
        &manager,
        transition(
            package("campaign-a"),
            Some(SlotId::LotV),
            package("campaign-b"),
            Some(SlotId::LotV),
        ),
        "a-to-b",
    );
    assert!(!live.join("VoidCampaignSave.SC2Save").exists());
    assert!(!live.join("Campaign").exists());

    touch(&live.join("VoidCampaignSave.SC2Save"), b"b-root");
    touch(&live.join("Campaign/b.SC2Save"), b"b-mission");
    touch(&banks.join("author/b.SC2Bank"), b"b-bank");
    execute(
        &manager,
        transition(
            package("campaign-b"),
            Some(SlotId::LotV),
            package("campaign-a"),
            Some(SlotId::LotV),
        ),
        "b-to-a",
    );

    assert_eq!(read(live.join("VoidCampaignSave.SC2Save")), b"a-root");
    assert_eq!(read(live.join("Campaign/a.SC2Save")), b"a-mission");
    assert_eq!(read(live.join("Unsaved/a.SC2Save")), b"a-auto");
    assert_eq!(read(banks.join("author/a.SC2Bank")), b"a-bank");
    assert!(!live.join("Campaign/b.SC2Save").exists());
    assert_eq!(read(live.join("SwarmCampaignSave.SC2Save")), b"plain-hots");
    assert_eq!(read(banks.join("ZCampaignStats.SC2Bank")), b"vanilla-bank");

    execute(
        &manager,
        transition(
            package("campaign-a"),
            Some(SlotId::LotV),
            SaveOwner::Plain,
            None,
        ),
        "a-to-plain",
    );
    assert_eq!(read(live.join("VoidCampaignSave.SC2Save")), b"plain-lotv");
    assert_eq!(read(live.join("Campaign/plain.SC2Save")), b"plain-mission");
    assert_eq!(read(live.join("Unsaved/plain.SC2Save")), b"plain-auto");
    assert_eq!(read(banks.join("author/plain.SC2Bank")), b"plain-bank");
}

#[test]
fn cross_faction_transition_restores_old_plain_roots_and_seeds_new_plain_roots() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let banks = temporary.path().join("profile/Banks");
    let store = temporary.path().join("store");
    let manager = SavesManager::new(live.clone(), &store);

    touch(&live.join("LibertyCampaignSave.SC2Save"), b"plain-wol");
    touch(&live.join("SwarmCampaignSave.SC2Save"), b"plain-hots");
    touch(&live.join("VoidCampaignSave.SC2Save"), b"plain-lotv");
    touch(&live.join("NovaCampaign01Save.SC2Save"), b"plain-nco");
    touch(&live.join("Campaign/plain.SC2Save"), b"plain-global");
    touch(&banks.join("author/plain.SC2Bank"), b"plain-bank");
    touch(&banks.join("WCampaign.SC2Bank"), b"vanilla-bank");

    execute(
        &manager,
        transition(
            SaveOwner::Plain,
            None,
            package("campaign-a"),
            Some(SlotId::LotV),
        ),
        "seed-a",
    );
    touch(&live.join("VoidCampaignSave.SC2Save"), b"a-root");
    touch(&live.join("Campaign/a.SC2Save"), b"a-global");
    touch(&banks.join("author/a.SC2Bank"), b"a-bank");

    execute(
        &manager,
        transition(
            package("campaign-a"),
            Some(SlotId::LotV),
            package("campaign-b"),
            Some(SlotId::HotS),
        ),
        "a-lotv-to-b-hots",
    );
    assert_eq!(read(live.join("VoidCampaignSave.SC2Save")), b"plain-lotv");
    assert!(!live.join("SwarmCampaignSave.SC2Save").exists());
    assert!(!live.join("Campaign").exists());
    assert_eq!(read(live.join("LibertyCampaignSave.SC2Save")), b"plain-wol");
    assert_eq!(read(live.join("NovaCampaign01Save.SC2Save")), b"plain-nco");
    assert_eq!(read(banks.join("WCampaign.SC2Bank")), b"vanilla-bank");

    touch(&live.join("SwarmCampaignSave.SC2Save"), b"b-root");
    touch(&live.join("Campaign/b.SC2Save"), b"b-global");
    touch(&banks.join("author/b.SC2Bank"), b"b-bank");
    execute(
        &manager,
        transition(
            package("campaign-b"),
            Some(SlotId::HotS),
            package("campaign-a"),
            Some(SlotId::LotV),
        ),
        "b-hots-to-a-lotv",
    );

    assert_eq!(read(live.join("SwarmCampaignSave.SC2Save")), b"plain-hots");
    assert_eq!(read(live.join("VoidCampaignSave.SC2Save")), b"a-root");
    assert_eq!(read(live.join("Campaign/a.SC2Save")), b"a-global");
    assert_eq!(read(banks.join("author/a.SC2Bank")), b"a-bank");
}

#[test]
fn already_active_play_is_a_true_noop() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let banks = temporary.path().join("profile/Banks");
    let manager = SavesManager::new(live.clone(), &temporary.path().join("store"));
    touch(&live.join("VoidCampaignSave.SC2Save"), b"active-root");
    touch(&live.join("Campaign/current.SC2Save"), b"active-mission");
    touch(&banks.join("author/current.SC2Bank"), b"active-bank");

    let no_change = transition(
        package("campaign-a"),
        Some(SlotId::LotV),
        package("campaign-a"),
        Some(SlotId::LotV),
    );
    execute(&manager, no_change, "play-active-a");

    assert_eq!(read(live.join("VoidCampaignSave.SC2Save")), b"active-root");
    assert_eq!(
        read(live.join("Campaign/current.SC2Save")),
        b"active-mission"
    );
    assert_eq!(read(banks.join("author/current.SC2Bank")), b"active-bank");
}

struct FailTargetCopies;

impl SaveIo for FailTargetCopies {
    fn rename(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        SystemSaveIo.rename(source, destination)
    }

    fn copy_file(&self, source: &Path, destination: &Path) -> std::io::Result<u64> {
        if source
            .components()
            .any(|component| component.as_os_str() == "saves-staging")
        {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        } else {
            SystemSaveIo.copy_file(source, destination)
        }
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        SystemSaveIo.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> std::io::Result<()> {
        SystemSaveIo.remove_dir(path)
    }

    fn wait(&self, _duration: Duration) {}
}

#[test]
fn apply_failure_rolls_back_the_exact_live_state() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let banks = temporary.path().join("profile/Banks");
    let store = temporary.path().join("store");
    let manager = SavesManager::new(live.clone(), &store);
    touch(&live.join("VoidCampaignSave.SC2Save"), b"original-root");
    touch(&live.join("Campaign/original.SC2Save"), b"original-global");
    touch(&banks.join("author/original.SC2Bank"), b"original-bank");
    touch(
        &store.join("saves/v2/packages/campaign-b/roots/lotv/VoidCampaignSave.SC2Save"),
        b"target-root",
    );

    let prepared = manager
        .prepare(
            transition(
                package("campaign-a"),
                Some(SlotId::LotV),
                package("campaign-b"),
                Some(SlotId::LotV),
            ),
            "rollback-copy-failure",
        )
        .unwrap();
    assert!(prepared.apply_with(&FailTargetCopies).is_err());

    assert_eq!(
        read(live.join("VoidCampaignSave.SC2Save")),
        b"original-root"
    );
    assert_eq!(
        read(live.join("Campaign/original.SC2Save")),
        b"original-global"
    );
    assert_eq!(
        read(banks.join("author/original.SC2Bank")),
        b"original-bank"
    );
}

#[test]
fn live_drift_after_prepare_aborts_before_any_destructive_change() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let store = temporary.path().join("store");
    let manager = SavesManager::new(live.clone(), &store);
    touch(&live.join("VoidCampaignSave.SC2Save"), b"prepared-version");
    touch(
        &store.join("saves/v2/packages/campaign-b/roots/lotv/VoidCampaignSave.SC2Save"),
        b"target-version",
    );

    let prepared = manager
        .prepare(
            transition(
                package("campaign-a"),
                Some(SlotId::LotV),
                package("campaign-b"),
                Some(SlotId::LotV),
            ),
            "detect-live-drift",
        )
        .unwrap();
    touch(&live.join("VoidCampaignSave.SC2Save"), b"new-user-version");

    let error = prepared.apply().unwrap_err();
    assert_eq!(error.code(), "save_verification_failed");
    assert_eq!(
        read(live.join("VoidCampaignSave.SC2Save")),
        b"new-user-version"
    );
    let rollback = prepared.rollback().unwrap_err();
    assert_eq!(rollback.code(), "save_verification_failed");
    assert!(prepared.paths().saves_backup.exists());
}

#[test]
fn target_staging_tamper_is_detected_before_apply_changes_live_saves() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let banks = temporary.path().join("profile/Banks");
    let store = temporary.path().join("store");
    let manager = SavesManager::new(live.clone(), &store);
    touch(&live.join("VoidCampaignSave.SC2Save"), b"previous-root");
    touch(&banks.join("author/previous.SC2Bank"), b"previous-bank");
    touch(
        &store.join("saves/v2/packages/campaign-b/roots/lotv/VoidCampaignSave.SC2Save"),
        b"target-root",
    );
    touch(
        &store.join("saves/v2/packages/campaign-b/global-banks/author/target.SC2Bank"),
        b"target-bank",
    );
    let prepared = manager
        .prepare(
            transition(
                package("campaign-a"),
                Some(SlotId::LotV),
                package("campaign-b"),
                Some(SlotId::LotV),
            ),
            "target-staging-tamper",
        )
        .unwrap();
    touch(
        &prepared
            .paths()
            .saves_staging
            .join("live/VoidCampaignSave.SC2Save"),
        b"substituted-target",
    );

    let error = prepared.apply().unwrap_err();

    assert_eq!(error.code(), "save_recovery_proof_mismatch");
    assert_eq!(
        read(live.join("VoidCampaignSave.SC2Save")),
        b"previous-root"
    );
    assert_eq!(
        read(banks.join("author/previous.SC2Bank")),
        b"previous-bank"
    );
    assert!(prepared.paths().saves_backup.exists());
}

#[test]
fn restart_after_save_finalize_proves_the_committed_target_from_sets() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let banks = temporary.path().join("profile/Banks");
    let store = temporary.path().join("store");
    let manager = SavesManager::new(live.clone(), &store);
    touch(&live.join("VoidCampaignSave.SC2Save"), b"a-root");
    touch(&live.join("Campaign/a.SC2Save"), b"a-global");
    touch(&banks.join("author/a.SC2Bank"), b"a-bank");
    touch(
        &store.join("saves/v2/packages/campaign-b/roots/lotv/VoidCampaignSave.SC2Save"),
        b"b-root",
    );
    touch(
        &store.join("saves/v2/packages/campaign-b/global/Campaign/b.SC2Save"),
        b"b-global",
    );
    touch(
        &store.join("saves/v2/packages/campaign-b/global-banks/author/b.SC2Bank"),
        b"b-bank",
    );
    let change = transition(
        package("campaign-a"),
        Some(SlotId::LotV),
        package("campaign-b"),
        Some(SlotId::LotV),
    );

    let prepared = manager
        .prepare(change.clone(), "crash-after-save-finalize")
        .unwrap();
    let recovery_proof = prepared.recovery_proof().unwrap().clone();
    prepared.apply().unwrap();
    prepared.finalize().unwrap();
    assert!(!prepared.paths().saves_backup.exists());

    let recovered = manager
        .prepared(change, "crash-after-save-finalize", recovery_proof)
        .unwrap();
    recovered.finalize().unwrap();
    recovered.verify_committed().unwrap();
    assert_eq!(read(live.join("VoidCampaignSave.SC2Save")), b"b-root");
    assert_eq!(read(live.join("Campaign/b.SC2Save")), b"b-global");
    assert_eq!(read(banks.join("author/b.SC2Bank")), b"b-bank");

    touch(&live.join("VoidCampaignSave.SC2Save"), b"drifted");
    let error = recovered.finalize().unwrap_err();
    assert_eq!(error.code(), "committed_saves_drifted");
}

#[test]
fn file_and_directory_collisions_replace_by_actual_kind() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let banks = temporary.path().join("profile/Banks");
    let store = temporary.path().join("store");
    let manager = SavesManager::new(live.clone(), &store);
    touch(&live.join("Campaign/old.SC2Save"), b"old-dir");
    touch(&banks.join("author"), b"old-file");
    touch(
        &store.join("saves/v2/packages/campaign-b/global/Campaign"),
        b"target-file",
    );
    touch(
        &store.join("saves/v2/packages/campaign-b/global-banks/author/new.SC2Bank"),
        b"target-dir",
    );

    let prepared = manager
        .prepare(
            transition(
                package("campaign-a"),
                Some(SlotId::LotV),
                package("campaign-b"),
                Some(SlotId::LotV),
            ),
            "kind-collisions",
        )
        .unwrap();
    prepared.apply().unwrap();

    assert_eq!(read(live.join("Campaign")), b"target-file");
    assert_eq!(read(banks.join("author/new.SC2Bank")), b"target-dir");
    prepared.rollback().unwrap();
    assert_eq!(read(live.join("Campaign/old.SC2Save")), b"old-dir");
    assert_eq!(read(banks.join("author")), b"old-file");
}

struct CrossDeviceRename {
    calls: AtomicUsize,
}

impl SaveIo for CrossDeviceRename {
    fn rename(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(std::io::Error::from(std::io::ErrorKind::CrossesDevices))
        } else {
            SystemSaveIo.rename(source, destination)
        }
    }

    fn copy_file(&self, source: &Path, destination: &Path) -> std::io::Result<u64> {
        SystemSaveIo.copy_file(source, destination)
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        SystemSaveIo.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> std::io::Result<()> {
        SystemSaveIo.remove_dir(path)
    }

    fn wait(&self, _duration: Duration) {}
}

#[test]
fn cross_device_move_falls_back_to_verified_copy_then_remove() {
    let temporary = tempfile::tempdir().unwrap();
    let documents = temporary.path();
    let (saves, banks) = profile_tree(documents);
    touch(&saves.join("Campaign/save.SC2Save"), b"save");
    touch(&banks.join("author/bank.SC2Bank"), b"bank");
    let profile = discover(documents).unwrap().remove(0);
    let io = CrossDeviceRename {
        calls: AtomicUsize::new(0),
    };

    let backup = create_recovery_backup_with(documents, &profile.id, 42, &io).unwrap();
    assert_eq!(
        read(backup.path.join("Saves/Campaign/save.SC2Save")),
        b"save"
    );
    assert_eq!(read(backup.path.join("Banks/author/bank.SC2Bank")), b"bank");
    assert!(io.calls.load(Ordering::SeqCst) >= 1);
    assert!(!backup
        .path
        .parent()
        .unwrap()
        .join(format!(
            ".{}.staging",
            backup.path.file_name().unwrap().to_string_lossy()
        ))
        .exists());
}

struct RetryCopies {
    failures_left: AtomicUsize,
    waits: AtomicUsize,
}

impl SaveIo for RetryCopies {
    fn rename(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        SystemSaveIo.rename(source, destination)
    }

    fn copy_file(&self, source: &Path, destination: &Path) -> std::io::Result<u64> {
        if self
            .failures_left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        } else {
            SystemSaveIo.copy_file(source, destination)
        }
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        SystemSaveIo.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> std::io::Result<()> {
        SystemSaveIo.remove_dir(path)
    }

    fn wait(&self, _duration: Duration) {
        self.waits.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn sharing_violations_are_retried_with_a_bound() {
    let temporary = tempfile::tempdir().unwrap();
    let documents = temporary.path();
    let (saves, banks) = profile_tree(documents);
    touch(&saves.join("Campaign/save.SC2Save"), b"save");
    touch(&banks.join("author/bank.SC2Bank"), b"bank");
    let profile = discover(documents).unwrap().remove(0);
    let io = RetryCopies {
        failures_left: AtomicUsize::new(3),
        waits: AtomicUsize::new(0),
    };

    create_recovery_backup_with(documents, &profile.id, 43, &io).unwrap();
    assert_eq!(io.waits.load(Ordering::SeqCst), 3);
}

#[test]
fn recovery_backup_copies_and_verifies_complete_saves_and_banks() {
    let temporary = tempfile::tempdir().unwrap();
    let documents = temporary.path();
    let (saves, banks) = profile_tree(documents);
    touch(&saves.join("Campaign/save.SC2Save"), b"mission");
    touch(&saves.join("Multiplayer/shared.SC2Save"), b"shared");
    touch(&banks.join("ZCampaignStats.SC2Bank"), b"vanilla");
    touch(&banks.join("author/custom.SC2Bank"), b"custom");
    let profile = discover(documents).unwrap().remove(0);

    let first = create_recovery_backup(documents, &profile.id, 1_787_500_000).unwrap();
    let second = create_recovery_backup(documents, &profile.id, 1_787_500_000).unwrap();
    assert_ne!(first.path, second.path);
    assert!(first
        .path
        .starts_with(documents.join("StarVault CCM Recovery")));
    assert_eq!(
        read(first.path.join("Saves/Campaign/save.SC2Save")),
        b"mission"
    );
    assert_eq!(
        read(first.path.join("Saves/Multiplayer/shared.SC2Save")),
        b"shared"
    );
    assert_eq!(
        read(first.path.join("Banks/ZCampaignStats.SC2Bank")),
        b"vanilla"
    );
    assert_eq!(
        read(first.path.join("Banks/author/custom.SC2Bank")),
        b"custom"
    );
}

#[cfg(unix)]
#[test]
fn save_links_are_copied_and_removed_as_links_without_touching_targets() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let target = temporary.path().join("external-campaign");
    let store = temporary.path().join("store");
    std::fs::create_dir_all(&live).unwrap();
    touch(&target.join("sentinel.SC2Save"), b"sentinel");
    symlink(&target, live.join("Campaign")).unwrap();

    let manager = SavesManager::new(live.clone(), &store);
    let prepared = manager
        .prepare(
            transition(
                SaveOwner::Plain,
                None,
                package("campaign-a"),
                Some(SlotId::LotV),
            ),
            "linked-campaign",
        )
        .unwrap();
    prepared.apply().unwrap();
    assert!(!live.join("Campaign").exists());
    assert_eq!(read(target.join("sentinel.SC2Save")), b"sentinel");
    prepared.rollback().unwrap();
    assert!(std::fs::symlink_metadata(live.join("Campaign"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(read(target.join("sentinel.SC2Save")), b"sentinel");
}
