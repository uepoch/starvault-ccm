//! Save-set isolation: discovery, faction-scoped sweeps, round trips.

use svccm_core::layout::SlotId;
use svccm_core::saves::{discover, is_onedrive, saves_dir, SavesManager};

fn touch(p: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, bytes).unwrap();
}

fn fake_documents(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("StarCraft II").join("Accounts")
}

#[test]
fn discovers_profiles_and_requires_saves_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = fake_documents(tmp.path());

    // A real profile tree.
    touch(
        &accounts.join("120927238/2-S2-1-3475134/Saves/LibertyCampaignSave.SC2Save"),
        b"x",
    );
    // Account dir without a Saves dir (never ran SC2) — skipped.
    touch(&accounts.join("999888/1-S2-1-1/Banks/keep.txt"), b"y");

    let profiles = discover(tmp.path());
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].id, "120927238/2-S2-1-3475134");

    assert!(saves_dir(tmp.path(), "120927238/2-S2-1-3475134").is_some());
    assert!(saves_dir(tmp.path(), "missing/1-S2-1-2").is_none());
}

#[test]
fn onedrive_paths_are_detected() {
    assert!(is_onedrive(std::path::Path::new(
        "C:/Users/x/OneDrive/Documents/StarCraft II"
    )));
    assert!(is_onedrive(std::path::Path::new(
        "C:/Users/x/OneDrive - Acme/Documents"
    )));
    assert!(!is_onedrive(std::path::Path::new(
        "C:/Users/x/Documents/StarCraft II"
    )));
}

#[test]
fn swap_isolates_saves_between_owners_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp
        .path()
        .join("Documents/StarCraft II/Accounts/1/1-S2-1-1/Saves");
    let store = tmp.path().join("store");
    let mgr = SavesManager::new(live.clone(), &store);

    // Plain LotV progress + mission saves; HotS files must stay untouched.
    touch(&live.join("VoidCampaignSave.SC2Save"), b"plain-progress");
    touch(&live.join("Campaign/For the Swarm.SC2Save"), b"mission");
    touch(&live.join("Unsaved/autosave.SC2Save"), b"auto");
    touch(&live.join("SwarmCampaignSave.SC2Save"), b"hots-stays");
    touch(&live.join("Multiplayer/1v1.SC2Save"), b"mp-stays");
    // Campaign progress banks live beside Saves (profile dir), not inside.
    let banks = live.parent().unwrap().join("Banks");
    touch(&banks.join("2-S2-1-777/plain-bank.SC2Bank"), b"plain-bank");
    // A vanilla bank already sitting in the plain set (previous sweep):
    // the collision removal must handle file-shaped destinations (os error
    // 267 when remove_dir_all hits a file).
    touch(
        &store.join("saves/lotv-plain/Banks/ZCampaignStats.SC2Bank"),
        b"older",
    );

    // Activate custom campaign: plain saves archived, campaign set fresh.
    mgr.swap(SlotId::LotV, "kerrigan", "plain").unwrap();
    assert!(!live.join("VoidCampaignSave.SC2Save").exists());
    assert!(!live.join("Campaign/For the Swarm.SC2Save").exists());
    assert!(live.join("SwarmCampaignSave.SC2Save").exists());
    assert!(live.join("Multiplayer/1v1.SC2Save").exists());
    assert!(store
        .join("saves/lotv-plain/VoidCampaignSave.SC2Save")
        .is_file());
    assert!(store
        .join("saves/lotv-plain/Campaign/For the Swarm.SC2Save")
        .is_file());

    // The game writes new progress while the campaign is live.
    touch(&live.join("VoidCampaignSave.SC2Save"), b"campaign-progress");
    touch(&live.join("Campaign/Kerrigan Mission.SC2Save"), b"km");
    touch(
        &banks.join("2-S2-1-777/plain-bank.SC2Bank"),
        b"campaign-bank",
    );
    // A vanilla-named bank left in live (e.g. cloud sync) must ride with
    // plain, never with the displaced campaign.
    touch(&banks.join("ZCampaignStats.SC2Bank"), b"vanilla");

    // Restore to plain: campaign saves archived, plain ones back.
    mgr.swap(SlotId::LotV, "plain", "kerrigan").unwrap();
    assert!(store
        .join("saves/lotv-plain/Banks/ZCampaignStats.SC2Bank")
        .is_file());
    assert!(!store
        .join("saves/lotv-kerrigan/Banks/ZCampaignStats.SC2Bank")
        .exists());
    assert!(live.join("VoidCampaignSave.SC2Save").is_file());
    assert!(live.join("Campaign/For the Swarm.SC2Save").is_file());
    assert!(!live.join("Campaign/Kerrigan Mission.SC2Save").exists());
    // Root saves are vanilla-owned: whatever they held while the campaign
    // ran rides with the plain set (live wins), never the campaign's —
    // a campaign set cannot grow a Continue state it did not earn.
    assert_eq!(
        std::fs::read(store.join("saves/lotv-plain/VoidCampaignSave.SC2Save")).unwrap(),
        b"campaign-progress"
    );
    assert!(!store
        .join("saves/lotv-kerrigan/VoidCampaignSave.SC2Save")
        .exists());
    assert!(store
        .join("saves/lotv-kerrigan/Campaign/Kerrigan Mission.SC2Save")
        .is_file());
    // Banks ride with the set and come back on restore.
    assert_eq!(
        std::fs::read(banks.join("2-S2-1-777/plain-bank.SC2Bank")).unwrap(),
        b"plain-bank"
    );
    assert_eq!(
        std::fs::read(store.join("saves/lotv-kerrigan/Banks/2-S2-1-777/plain-bank.SC2Bank"))
            .unwrap(),
        b"campaign-bank"
    );

    // Shared saves were never touched.
    assert_eq!(
        std::fs::read(live.join("SwarmCampaignSave.SC2Save")).unwrap(),
        b"hots-stays"
    );
}

#[test]
fn sweep_is_faction_scoped_at_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live/Saves");
    touch(&live.join("LibertyCampaignSave.SC2Save"), b"wol");
    touch(&live.join("NovaCampaign01Save.SC2Save"), b"nco");
    let store = tmp.path().join("store");

    // Swapping NCO must not archive the WoL file.
    SavesManager::new(live.clone(), &store)
        .swap(SlotId::Nco, "nova-mod", "plain")
        .unwrap();
    assert!(live.join("LibertyCampaignSave.SC2Save").is_file());
    assert!(!live.join("NovaCampaign01Save.SC2Save").exists());
    assert!(store
        .join("saves/nco-plain/NovaCampaign01Save.SC2Save")
        .is_file());
}

#[test]
fn remove_sets_deletes_only_that_package() {
    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("live/Saves");
    let store = tmp.path().join("store");
    let mgr = SavesManager::new(live.clone(), &store);

    touch(
        &store.join("saves/lotv-kerrigan/VoidCampaignSave.SC2Save"),
        b"k",
    );
    touch(
        &store.join("saves/lotv-plain/VoidCampaignSave.SC2Save"),
        b"p",
    );

    assert_eq!(mgr.remove_sets("kerrigan"), 1);
    assert!(!store.join("saves/lotv-kerrigan").exists());
    assert!(store.join("saves/lotv-plain").is_dir());
}
