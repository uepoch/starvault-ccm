//! Windows-only coverage for real NTFS directory junctions in save data.

#![cfg(windows)]

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use svccm_core::identity::PackageId;
use svccm_core::layout::SlotId;
use svccm_core::saves::{SaveOwner, SaveTransition, SavesManager};

fn touch(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn package(id: &str) -> SaveOwner {
    SaveOwner::Package(PackageId::parse(id).unwrap())
}

fn junction_target(path: &Path) -> PathBuf {
    std::fs::read_link(path).unwrap()
}

#[test]
fn campaign_junction_is_swapped_and_rolled_back_without_traversing_its_target() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let external = temporary.path().join("external-campaign");
    let campaign = live.join("Campaign");
    let store = temporary.path().join("store");
    std::fs::create_dir_all(&live).unwrap();
    touch(&external.join("sentinel.SC2Save"), b"outside");
    junction::create(&external, &campaign).unwrap();
    let original_target = junction_target(&campaign);

    let manager = SavesManager::new(live.clone(), &store);
    let prepared = manager
        .prepare(
            SaveTransition {
                previous_owner: SaveOwner::Plain,
                previous_faction: None,
                target_owner: package("campaign-a"),
                target_faction: Some(SlotId::LotV),
            },
            "junction-swap",
        )
        .unwrap();

    prepared.apply().unwrap();
    assert!(std::fs::symlink_metadata(&campaign).is_err());
    assert_eq!(
        std::fs::read(external.join("sentinel.SC2Save")).unwrap(),
        b"outside"
    );

    prepared.rollback().unwrap();
    let metadata = std::fs::symlink_metadata(&campaign).unwrap();
    assert!(metadata.file_type().is_symlink());
    assert_eq!(junction_target(&campaign), original_target);
    assert_eq!(
        std::fs::read(external.join("sentinel.SC2Save")).unwrap(),
        b"outside"
    );
}

#[test]
fn target_file_replaces_live_campaign_junction_without_touching_external_data() {
    let temporary = tempfile::tempdir().unwrap();
    let live = temporary.path().join("profile/Saves");
    let external = temporary.path().join("external-campaign");
    let campaign = live.join("Campaign");
    let store = temporary.path().join("store");
    std::fs::create_dir_all(&live).unwrap();
    touch(&external.join("sentinel.SC2Save"), b"outside");
    junction::create(&external, &campaign).unwrap();
    touch(
        &store.join("saves/v2/packages/campaign-b/global/Campaign"),
        b"target-file",
    );

    let manager = SavesManager::new(live.clone(), &store);
    let prepared = manager
        .prepare(
            SaveTransition {
                previous_owner: package("campaign-a"),
                previous_faction: Some(SlotId::LotV),
                target_owner: package("campaign-b"),
                target_faction: Some(SlotId::LotV),
            },
            "junction-collision",
        )
        .unwrap();

    prepared.apply().unwrap();
    assert_eq!(std::fs::read(&campaign).unwrap(), b"target-file");
    assert_eq!(
        std::fs::read(external.join("sentinel.SC2Save")).unwrap(),
        b"outside"
    );
    prepared.rollback().unwrap();
    assert!(std::fs::symlink_metadata(&campaign)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read(external.join("sentinel.SC2Save")).unwrap(),
        b"outside"
    );
}

#[test]
fn exclusive_save_locks_exhaust_retries_without_losing_saves_or_banks() {
    for lock_banks in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let live = temporary.path().join("profile/Saves");
        let banks = temporary.path().join("profile/Banks");
        let store = temporary.path().join("store");
        let live_root = live.join("VoidCampaignSave.SC2Save");
        let live_mission = live.join("Campaign/a.SC2Save");
        let live_bank = banks.join("author/a.SC2Bank");
        touch(&live_root, b"a-root");
        touch(&live_mission, b"a-mission");
        touch(&live_bank, b"a-bank");
        touch(
            &store.join("saves/v2/packages/campaign-b/roots/lotv/VoidCampaignSave.SC2Save"),
            b"b-root",
        );
        touch(
            &store.join("saves/v2/packages/campaign-b/global/Campaign/b.SC2Save"),
            b"b-mission",
        );
        touch(
            &store.join("saves/v2/packages/campaign-b/global-banks/author/b.SC2Bank"),
            b"b-bank",
        );
        let manager = SavesManager::new(live.clone(), &store);
        let prepared = manager
            .prepare(
                SaveTransition {
                    previous_owner: package("campaign-a"),
                    previous_faction: Some(SlotId::LotV),
                    target_owner: package("campaign-b"),
                    target_faction: Some(SlotId::LotV),
                },
                if lock_banks {
                    "locked-bank-transition"
                } else {
                    "locked-save-transition"
                },
            )
            .unwrap();
        let locked_path = if lock_banks { &live_bank } else { &live_root };
        let expected_locked = if lock_banks {
            b"a-bank".as_slice()
        } else {
            b"a-root".as_slice()
        };
        let mut locked = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(locked_path)
            .unwrap();

        let started = Instant::now();
        let error = prepared.apply().unwrap_err();

        assert_eq!(error.code(), "hash_save_entry");
        assert!(error.retryable());
        assert_eq!(error.path(), Some(locked_path.as_path()));
        assert!(started.elapsed() >= Duration::from_millis(700));
        let mut locked_bytes = Vec::new();
        locked.seek(SeekFrom::Start(0)).unwrap();
        locked.read_to_end(&mut locked_bytes).unwrap();
        assert_eq!(locked_bytes, expected_locked);
        assert_eq!(std::fs::read(&live_mission).unwrap(), b"a-mission");
        if !lock_banks {
            assert_eq!(std::fs::read(&live_bank).unwrap(), b"a-bank");
        }
        assert!(prepared.paths().saves_backup.is_dir());
        assert!(prepared.paths().banks_backup.is_dir());

        let cleanup_error = prepared.rollback().unwrap_err();
        assert_eq!(cleanup_error.code(), "hash_save_entry");
        assert!(cleanup_error.retryable());
        assert!(prepared.paths().saves_backup.is_dir());
        assert!(prepared.paths().banks_backup.is_dir());
        drop(locked);

        assert_eq!(std::fs::read(&live_root).unwrap(), b"a-root");
        assert_eq!(std::fs::read(&live_mission).unwrap(), b"a-mission");
        assert_eq!(std::fs::read(&live_bank).unwrap(), b"a-bank");
        assert!(!live.join("Campaign/b.SC2Save").exists());
        assert!(!banks.join("author/b.SC2Bank").exists());
        prepared.rollback().unwrap();
        assert!(!prepared.paths().saves_staging.exists());
        assert!(!prepared.paths().saves_backup.exists());
        assert!(!prepared.paths().banks_staging.exists());
        assert!(!prepared.paths().banks_backup.exists());
        assert_eq!(std::fs::read(&live_root).unwrap(), b"a-root");
        assert_eq!(std::fs::read(&live_mission).unwrap(), b"a-mission");
        assert_eq!(std::fs::read(&live_bank).unwrap(), b"a-bank");
    }
}
