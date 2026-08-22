# Research: save-set isolation for SC2 custom campaigns (junction / VFS feasibility)

## Summary

Feasible with our existing junction machinery, but the design-doc premise is **wrong in one load-bearing way**: SC2 saves are **not** subfoldered per campaign. Campaign progress lives as flat, per-campaign-*ID* files (`LibertyCampaignSave.SC2Save`, `SwarmCampaignSave.SC2Save`, …) directly under `...\Accounts\<acct>\<profile>\Saves\`, with in-mission saves under `Saves\Campaign\`. Isolation therefore means junctioning the **whole `Saves` directory** per faction slot — not "per-campaign save directories." NTFS junction is the right mechanism (reuse `swap_junction`); OneDrive Known Folder Move and multi-account/profile discovery are the two real hazards.

## (a) Verified save-path facts

1. **Path structure** — `Documents\StarCraft II\Accounts\<numeric>\<profile>\Saves\` and `Saves\Campaign\`, where `<numeric>` is the local Battle.net account ID and `<profile>` is `R-S2-1-<toon>` (R = region digit, e.g. 1=US, 2=EU). Multiple pairs appear after region switches or different Battle.net logins. [Blizzard forums — one-possible-fix-for-lost-campaign-saves](https://us.forums.blizzard.com/en/sc2/t/one-possible-fix-for-lost-campaign-saves/7431), [staredit bank-path thread](https://staredit.net/topic/13323/52/). **Unverified**: exact field semantics of each component (community inference, no official doc); whether `<numeric>` changes on game-version updates (no evidence it does).
2. **Flat layout, named per campaign ID** — from the game's own data catalog: `SaveName` per campaign is `LibertyCampaignSave` / `SwarmCampaignSave` / `VoidPrologueCampaignSave` / `VoidCampaignSave` / `VoidEpilogueCampaignSave` / `NovaCampaign01Save` (`.SC2Save`), plus `*CompletedSave` variants. All Nova mission packs share one file. [SC2Data.xml (game data extract)](https://github.com/Talv/sc2-data/blob/master/mods/core.sc2mod/base.sc2data/GameData/SC2Data.xml) — verified by direct fetch. Community confirms the progress files sit directly in `Saves\`, in-mission saves in `Saves\Campaign\`. [Hive Workshop](https://www.hiveworkshop.com/threads/i-want-to-change-smth-in-a-bank-file.182250/). **Consequence**: a custom campaign replacing a faction's entrypoint inherits that faction's `SaveName` → two campaigns on one slot write the *same file*. That is the collision we're isolating.
3. **Not configurable** — no launch arg or `Variables.txt` key redirects saves; SC2 writes under the shell-resolved Documents known folder, and Blizzard's fix for OneDrive interference is to *exclude* `Documents\StarCraft II` from sync, not to relocate. [Blizzard forums — cant-change-settings](https://us.forums.blizzard.com/en/sc2/t/cant-change-settings/9262). Resolve via `SHGetKnownFolderPath(FOLDERID_Documents, 0)` (KFM-aware); `%USERPROFILE%\Documents` is wrong on KFM machines. [KNOWN_FOLDER_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/ne-shlobj_core-known_folder_flag).
4. **Recreation** — the game rebuilds missing `Documents\StarCraft II` structure (documented workaround: rename folder, relaunch, restore saves). A missing `Saves` dir is treated as absent, not fatal. [Blizzard forums — cannot-select-any-campaign](https://us.forums.blizzard.com/en/sc2/t/cannot-select-any-campaign/9376). Dangling-junction behavior specifically: **unverified** (no community report found); our reconcile must not rely on game tolerance.
5. **OneDrive × junctions** — reparse points are officially unsupported for sync; KFM can refuse/repair with "folder contains a reparse point," and Microsoft's remediation is removing the link. [OneDrive restrictions](https://support.microsoft.com/en-us/onedrive/restrictions-and-limitations-in-onedrive-and-sharepoint), [KFM doc](https://support.microsoft.com/en-US/onedrive/back-up-your-folders-with-onedrive).
6. **Prior art** — none found for per-campaign save isolation. Old CCM does not touch saves at all ([7thAce/SC2CCM](https://github.com/7thAce/SC2CCM)); Mass Recall/AIO guides tell users to back up saves manually. Closest prior art is community junctioning of the whole `Documents\StarCraft II` for disk relocation — proves the game reads through a junction fine. **Unverified** whether any tool ever junctioned `Saves` specifically.

## (b) Mechanism ranking

| Rank | Mechanism | Verdict |
| --- | --- | --- |
| 1 | **NTFS junction on `Saves`** | Reuses `make_junction`/`remove_junction` + stage→verify→swap→reconcile as-is. No privilege needed. Local-only targets (our store is local). Over OneDrive it is *technically* read-fine by the game but unsupported by OneDrive sync — needs the detection/warning below. |
| 2 | Dir symlink | No advantage; needs Developer Mode/admin to create. Reject. |
| 3 | Copy strategy | Fallback when Documents is OneDrive-managed or junction create fails — mirrors our existing auto-fallback. Save-sets are small (a few MB), so copy cost is trivial. |
| 4 | VHD / ProjFS | Attach/detach lifecycle and a resident provider process while the game runs. Massive overkill for a pointer swap. Reject. |

Note the handle caveat applies to all links: a swap changes future path resolution only; already-open handles stay on the old tree. That argues for pre-launch-only swaps, which we already enforce ("launching never mutates" + preflight `no running instance`).

## (c) Swap / recovery protocol (fits existing machinery)

```
discover():
  docs = SHGetKnownFolderPath(FOLDERID_Documents)           # KFM-aware
  candidates = glob(docs/"StarCraft II"/Accounts/*/*/Saves)
  one hit  -> persist (account, profile) in config
  many     -> picker UI; none -> feature dormant
  on later runs: if persisted pair missing from candidates => drift note, re-pick

activate(slot, pkg):                    # save-set IS user data, not package data
  set = store/saves/<slot>-<pkg>/
  first activation: import live Saves/ content into set (this "seeds" the plain
                    campaign's saves into the store as the slot's plain set)
  stage  : ensure set exists (no manifest verify — file-count spot check only)
  swap   : rename Saves -> Saves.backup-<pid>; make_junction(Saves, set)   # swap_junction shape
  commit : update ledger; on failure restore backup (existing Err arm)
  cleanup: reclaim .staging-*/.backup-* via existing sibling conventions

reconcile():                            # extend existing per-slot loop with save slots
  dangling junction -> remove_junction, clear ledger, report "activate again"
  .backup-* with no live Saves -> rename back (crash mid-swap)
  persisted account/profile no longer present -> report drift, do not auto-pick
```

Differences from game-dir slots worth encoding: (1) the store tree is *imported from live data*, so it must be exempt from any future blob GC — losing it loses saves; (2) `Saves` sits in Documents, so the path must be re-resolved every run, never cached as absolute; (3) `Documents` under OneDrive detection: if the resolved Documents path contains `\OneDrive\`, either warn + offer to exclude the subtree, or force copy strategy.

## (d) Top 3 risks & mitigations

1. **Flat save layout breaks the "per-campaign directory" premise** — no such directories exist; isolation must swap the whole `Saves` per *faction slot*. A campaign sharing a faction inherits its `SaveName`. Mitigation: junction the whole `Saves`; document that isolation granularity = slot (WoL/HotS/LotV/NCO), which matches how our slots already work. Update `docs/design/game-integration.md` deferred-idea wording before building.
2. **OneDrive KFM Documents** — junctions unsupported there: sync errors, broken KFM repair, plausible save loss. Mitigation: detect OneDrive-owned Documents at discover time; warn and offer (a) exclude `Accounts\...\Saves` subtree from sync or (b) copy strategy. Never silently place a junction under KFM.
3. **Multi-account / profile discovery picking wrong tree** — wrong `Accounts\<x>\<y>` = saves appear to vanish (they're isolated under another profile's path). Mitigation: enumerate all, persist choice, drift-check at every reconcile, picker when ambiguous; never auto-select silently.

Residual: game behavior on a *dangling* `Saves` junction is unverified — treat reconcile's dangling-junction removal as mandatory before launch rather than relying on game tolerance. `junction` crate behavior and our helpers need no change.
