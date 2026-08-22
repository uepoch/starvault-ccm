//! Campaign save-set isolation.
//!
//! Saves are bound to the exact campaign content: swapping a faction's
//! campaign strands its saves (game refuses them). Isolation gives each
//! package (and the plain campaign) its own save-set under the store; the
//! live `Saves` tree always mirrors the active package for its faction.
//!
//! Scope (docs/design/research-save-followups.md):
//! - Vanilla progress files live flat in `Saves\`, named per campaign ID —
//!   matched by faction prefix (Liberty…, Swarm…, Void…, Nova…).
//! - Mission saves (`Saves\Campaign\`) and autosaves (`Saves\Unsaved\`)
//!   have arbitrary names — swept whole.
//! - Multiplayer/Challenge saves and `Banks\` are shared, untouched (v1:
//!   same-author custom campaigns may still share banks).
//!
//! Strategy is copy/move (no reparse points), so a crash mid-swap can never
//! leave a dangling link; worst case a half-moved set is repaired by the
//! next swap (live content always wins over the set copy).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Result;
use crate::layout::SlotId;

/// One discovered saves profile: `Accounts\<account>\<profile>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SavesProfile {
    pub account: String,
    pub profile: String,
    /// `<account>/<profile>` — the persisted config form.
    pub id: String,
}

/// Save-file name prefixes per faction (case-insensitive match). Covers
/// SaveName / CompletedSaveName / PublishArchiveName variants from the
/// game's SC2Data.xml.
pub fn save_prefixes(slot: SlotId) -> &'static [&'static str] {
    match slot {
        SlotId::Wol => &["LibertyCampaign"],
        SlotId::HotS => &["SwarmCampaign", "SwarmPublish"],
        SlotId::LotV => &[
            "VoidCampaign",
            "VoidPublish",
            "VoidPrologue",
            "VoidEpilogue",
        ],
        SlotId::Nco => &["NovaCampaign"],
    }
}

/// Directories inside `Saves\` that belong to the campaign (swept whole);
/// everything else (Multiplayer, Challenge) is shared.
const SWEPT_DIRS: [&str; 2] = ["Campaign", "Unsaved"];

/// True when the path lives under a OneDrive-managed folder — isolation is
/// not supported there yet (sync conflicts with swap churn).
pub fn is_onedrive(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        name.eq_ignore_ascii_case("onedrive") || name.to_lowercase().starts_with("onedrive - ")
    })
}

/// Enumerate `Documents\StarCraft II\Accounts\<acct>\<profile>\Saves` trees.
pub fn discover(documents: &Path) -> Vec<SavesProfile> {
    let mut out = Vec::new();
    let accounts = match documents.join("StarCraft II").join("Accounts").read_dir() {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for account in accounts.flatten() {
        if !account.path().is_dir() {
            continue;
        }
        let Ok(profiles) = account.path().read_dir() else {
            continue;
        };
        for profile in profiles.flatten() {
            let pname = profile.file_name().to_string_lossy().into_owned();
            // Profile dirs look like `<region>-S2-1-<toon>`; require the
            // Saves dir to exist so we only list real save trees.
            if pname.contains("-S2-") && profile.path().join("Saves").is_dir() {
                out.push(SavesProfile {
                    account: account.file_name().to_string_lossy().into_owned(),
                    profile: pname,
                    id: format!(
                        "{}/{}",
                        account.file_name().to_string_lossy(),
                        profile.file_name().to_string_lossy()
                    ),
                });
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Live saves dir for a persisted profile id (`<account>/<profile>`).
pub fn saves_dir(documents: &Path, profile_id: &str) -> Option<PathBuf> {
    let (account, profile) = profile_id.split_once('/')?;
    let dir = documents
        .join("StarCraft II")
        .join("Accounts")
        .join(account)
        .join(profile)
        .join("Saves");
    dir.is_dir().then_some(dir)
}

/// Swaps save-sets for one faction between owners ("plain" or a package id).
pub struct SavesManager {
    /// The live `…\Accounts\<acct>\<profile>\Saves` directory.
    live: PathBuf,
    /// `…\<profile>\Banks` — campaign progress banks live beside Saves,
    /// not inside it. For custom campaigns the bank *is* the campaign
    /// state (the launcher map reads it for "continue"); swept with the
    /// save-set so everything written while a campaign is active rides
    /// with it.
    banks: PathBuf,
    /// Store root; sets live under `<store>/saves/<slot>-<owner>/`.
    sets_root: PathBuf,
}

impl SavesManager {
    pub fn new(live_saves_dir: PathBuf, store_root: &Path) -> Self {
        let banks = live_saves_dir
            .parent()
            .map(|p| p.join("Banks"))
            .unwrap_or_else(|| live_saves_dir.join("Banks"));
        Self {
            live: live_saves_dir,
            banks,
            sets_root: store_root.join("saves"),
        }
    }

    fn set_dir(&self, slot: SlotId, owner: &str) -> PathBuf {
        self.sets_root.join(format!("{}-{}", slot.as_str(), owner))
    }

    /// Live root save files owned by this faction (prefix match).
    fn live_files(&self, slot: SlotId) -> Vec<PathBuf> {
        let prefixes = save_prefixes(slot);
        let Ok(entries) = self.live.read_dir() else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_lowercase();
                e.path().is_file()
                    && name.ends_with(".sc2save")
                    && prefixes.iter().any(|p| name.starts_with(&p.to_lowercase()))
            })
            .map(|e| e.path())
            .collect()
    }

    /// Make `new_owner`'s set live for `slot`, sweeping the current live
    /// content into `prev_owner`'s set first. Idempotent; live content
    /// always wins over the set copy. Returns repair notes.
    pub fn swap(&self, slot: SlotId, new_owner: &str, prev_owner: &str) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        std::fs::create_dir_all(&self.live)?;

        // --- sweep: live -> prev set ---------------------------------------
        let prev_dir = self.set_dir(slot, prev_owner);
        let swept = self.sweep_into(slot, &prev_dir)?;
        if swept > 0 {
            notes.push(format!(
                "{}: {} save file(s)/dir(s) archived to {prev_owner}",
                slot.as_str(),
                swept
            ));
        }

        // --- materialize: new set -> live ----------------------------------
        let new_dir = self.set_dir(slot, new_owner);
        if new_dir.is_dir() {
            let restored = self.materialize_from(&new_dir)?;
            if restored > 0 {
                notes.push(format!(
                    "{}: {restored} save file(s)/dir(s) restored from {new_owner}",
                    slot.as_str()
                ));
            }
        } else if prev_owner != new_owner {
            notes.push(format!(
                "{}: fresh save set for {new_owner} (nothing to restore)",
                slot.as_str()
            ));
        }
        Ok(notes)
    }

    /// Move the faction's live saves into `set_dir`. Live wins on collision.
    fn sweep_into(&self, slot: SlotId, set_dir: &Path) -> Result<usize> {
        let mut moved = 0;
        let mut touched = false;
        for file in self.live_files(slot) {
            if !touched {
                std::fs::create_dir_all(set_dir)?;
                touched = true;
            }
            let dest = set_dir.join(file.file_name().expect("save has a name"));
            if dest.symlink_metadata().is_ok() {
                std::fs::remove_file(&dest)?;
            }
            std::fs::rename(&file, &dest)?;
            moved += 1;
        }
        // Banks ride with the set: campaign progress (the "continue"
        // state) lives there for custom campaigns, and everything written
        // while this owner was active belongs to it.
        for dir in SWEPT_DIRS.map(|d| self.live.join(d)).into_iter().chain([self.banks.clone()]) {
            if !dir.is_dir() {
                continue;
            }
            if !touched {
                std::fs::create_dir_all(set_dir)?;
                touched = true;
            }
            let dest = set_dir.join(dir.file_name().expect("swept dir has a name"));
            if dest.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(&dest)?;
            }
            std::fs::rename(&dir, &dest)?;
            moved += 1;
        }
        Ok(moved)
    }

    /// Copy a set's root save files and swept dirs back into live. Banks
    /// restore beside Saves (their original home), not inside it.
    fn materialize_from(&self, set_dir: &Path) -> Result<usize> {
        let mut restored = 0;
        for entry in std::fs::read_dir(set_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let is_banks = name.eq_ignore_ascii_case("Banks");
            let dest_root = if is_banks {
                self.banks.parent().unwrap_or(&self.live).to_path_buf()
            } else {
                self.live.clone()
            };
            let dest = dest_root.join(&name);
            if path.is_file() {
                std::fs::copy(&path, &dest)?;
                restored += 1;
            } else if path.is_dir() {
                copy_tree(&path, &dest)?;
                restored += 1;
            }
        }
        Ok(restored)
    }

    /// Delete a package's save sets (called on package removal).
    pub fn remove_sets(&self, package: &str) -> usize {
        let mut removed = 0;
        for slot in SlotId::ALL {
            let dir = self.set_dir(slot, package);
            if dir.symlink_metadata().is_ok() && std::fs::remove_dir_all(&dir).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
