# Research: save isolation follow-ups (four streams)

Parent brief: `research-save-isolation.md`. These four follow-ups were run
against the real machine (WSL //mnt/c) plus web sources.

---

# Research: Detecting Battle.net account ID for SC2 saves tree (no game running)

## Summary
No registry key or config value is documented to carry the numeric SC2 account ID; the numeric ID exists offline only as the `Accounts\<id>` directory name itself. Login emails are recoverable but don't map to numeric IDs. Recommendation: select the saves tree by directory enumeration + recency heuristic, not by "detecting the logged-in account".

## What was found on the test machine (anonymized)
- `%APPDATA%\Battle.net\Battle.net.config` (read in full): `Client.SavedAccountNames = "<email-a>,<email-b> [redacted]"` — TWO saved logins; `Client.AutoLogin = "true"`; exactly one per-account key `"<opaque-hash>"` (opaque local hash, not a documented account UID) with `Services.LastLoginRegion = "EU"`; `Games.s2.LastPlayed = 1787398938` (~Aug 2026, recent) and `s2_ptr` also present.
- `Documents\StarCraft II\Variables.txt`: `LastAccountName=<EMAIL-A> [redacted]`, `accountCountry=<redacted>`, campaign progress (`completedCampaignMask=27`). Email only — no numeric ID anywhere in the file.
- `C:\ProgramData\Battle.net\Agent\product.db` (SQLite, read as text): Agent 2.40.3.9700, region `eu`, geoip `FR`. No account ID in readable strings. `Agent.log` is NOT at `ProgramData\Battle.net\Agent\Agent.log`; real layout is `Agent\Agent.<build>\Logs\Agent-<timestamp>.log` — unguessable without dir listing.
- `Documents\StarCraft II\Accounts\` exists (probe confirms dir) but this subagent has no shell/glob tool, so numeric dirs could NOT be enumerated. Registry via `reg.exe` interop: NOT RUN, same reason — parent should run: `"/mnt/c/Windows/System32/reg.exe" query "HKCU\Software\Blizzard Entertainment" /s` and `ls "~/Documents/StarCraft II/Accounts"`.
- Web: no documented Blizzard registry key carries the account ID (`HKCU\...\Battle.net\S2` historically held license data only). `SavedAccountNames` is what account-switcher tooling (TcNo-Acc-Switcher) treats as the local account identity. LAST_USED-style values are stale-able implementation details; Blizzard's supported ID source is OAuth, not local files.

## Ranked detection methods
1. **Enumerate `Accounts\*` dirs having `*\Saves` — RECOMMENDED.** Exactly one → use it. Several → newest mtime under `Saves`, user-overridable. Works with game and Battle.net fully closed; zero dependency on Blizzard internals.
2. **`Variables.txt` `LastAccountName`** — identifies which email last ran SC2 (here: <email-a>). Good for labeling/confirming a pick; cannot produce the numeric ID.
3. **`Battle.net.config` `SavedAccountNames`** — count of saved logins (2 here) warns multiple account dirs may exist. No numeric IDs.
4. **Agent/bnet client logs** — contain account IDs but timestamped filenames, undocumented format, brittle. Skip.
5. **Registry** — nothing documented carries the ID. Skip.
6. **Battle.net OAuth** — authoritative account ID, absurd for choosing a local saves dir. Skip.

## Is "exactly one dir, else dormant" sane?
Mostly. Single account + single region is the common case, and 0 dirs correctly means dormant. But multiple dirs are routine: region switches create additional regional IDs, family/shared PCs have second accounts — this machine has 2 saved logins (though only one per-account client key, so the second login likely never ran SC2 logged in here — unverified). Better gate: dormant on 0; on >1 auto-pick newest `Saves` mtime with a one-time user override. Never merge/delete account dirs (confirmed normal Blizzard behavior, separate saves/banks per region).

## Gaps
- Numeric dirs on this machine not enumerated (no shell tool in this subagent); registry unverified for the same reason.
- Whether SC2 PTR writes into the same Accounts tree: unconfirmed.

## Sources
- TcNo-Acc-Switcher Platforms.json (github.com/TCNOco/TcNo-Acc-Switcher) — SavedAccountNames is the field switchers rely on.
- Blizzard SC2 forums: "New windows account" / "Cannot load saved games" (us.forums.blizzard.com) — Accounts\<id>\<profile>\Saves layout, multiple account folders normal on region switch.
- lutris/services/battlenet.py — Agent product.db region reading; Agent log layout.
- Blizzard forums "Launcher cannot remember username" — config fields are implementation details, not authoritative session identity.


---

# COPY save-swap under OneDrive-synced Documents — verdict & mitigations

## Verdict
VIABLE WITH MITIGATIONS. COPY uses no reparse points, so the KFM blocker is gone;
the remaining risks (Files-On-Demand placeholders, transient locks, mass-delete optics, churn)
are all cheaply mitigable. Best case is still Blizzard's own advice: exclude
`Documents\StarCraft II` from OneDrive entirely.

## Findings
1. Base case confirmed: OneDrive does not support junctions/symlinks/reparse points
   inside KFM-backed folders — abandoning the junction strategy was correct. [MS Q&A 5831080]
2. [HIGH] Files-On-Demand: cloud-only placeholders hydrate on first open; SC2 expects
   plain local files and can fail/hang offline; Storage Sense/"Free up space" can
   dehydrate the Saves tree between sessions. Copy-out (read) also forces hydration,
   so a swap needs network. [MS Support: Files On-Demand]
3. [LOW typical / HIGH pathological] Churn: .SC2Save ≈ 0.6–5 MB each; a campaign set is
   typically a few–tens of MB → per-swap re-upload is trivial. Autosave hoards under
   Saves\Unsaved can reach ~700 MB–1 GB → guard sweep size / prune Unsaved.
4. [MED] Locking: os error 32 (ERROR_SHARING_VIOLATION) from OneDrive/AV/Explorer is
   transient; bounded exponential backoff (1–16 s + jitter, 5–8 tries) per file. MS also
   suggests staging outside the synced folder, then moving in once tray shows "Up to date".
5. Blizzard stance: SC2 support/forum guidance is to exclude `Documents\StarCraft II`
   from OneDrive backup/sync; uninstalling OneDrive alone does not undo redirection.
   [Blizzard forum threads 7040, 4876, 21843]
6. [MED] Mass deletion: a local delete inside a synced folder propagates to cloud AND
   all devices; recycle bin holds 30 d (personal) / 93 d (work/school); M365 users get
   30-day "Restore your OneDrive" rewind. A delete-based sweep = repeated mass-delete
   events; a MOVE into a store inside the same synced tree emits no deletes (renames only).
   [MS Support: delete files; restore your OneDrive]

## Local check (this machine)
- ~/Documents/StarCraft II/variables.txt — EXISTS, live SC2 data.
- ~/OneDrive/Documents/StarCraft II/variables.txt — ENOENT.
→ Documents is NOT OneDrive-redirected here (KFM off); OneDrive risk is currently
  theoretical for this user. No shell in session: ls/du not run; existence probes used.

## Mitigation sequence (ordered)
1. Detect KFM at startup: does resolved Documents live under OneDrive? Files-On-Demand attrs on Saves?
2. Pin Saves "Always keep on this device" (attrib +P -U) before any swap → kills (2).
3. Sweep = MOVE (rename) into a store inside the SAME synced tree → kills (6), no churn.
4. Materialize via write-temp + rename; bounded backoff retry on os err 32/33 per file;
   on exhausted budget mid-swap, roll back (move swept set back) — never half-swap.
5. Offer optional OneDrive exclusion of Documents\StarCraft II (Blizzard-recommended).

## Gaps
- Save-set sizes not measured locally (no shell tool); community figures substituted.
- Blizzard guidance is forum/CS threads, not a formal KB article.


---

# Research: .SC2Save internal structure (campaign vs mission saves)

## Summary
A campaign progress save is a **single self-contained MPQ file** (no nested per-mission saves inside — mission saves are sibling files in `Saves\Campaign\`). Locally confirmed on real saves: first bytes of `LibertyCampaignSave.SC2Save` are `4D 50 51 1B` = `MPQ\x1b` user-data header — same MPQ container family as maps/replays, readable by the already-depended `wow-mpq` crate.

## Findings
1. Single file per campaign, no nesting. Local: `Saves\LibertyCampaignSave.SC2Save` (2.8 MB), `SwarmCampaignSave` + `SwarmCampaignCompletedSave`, `VoidCampaignSave`, `VoidEpilogueCampaignSave`, `VoidPrologueCampaignSave`, plus 4 `*PublishArchive.SC2Save`. Per-mission saves are separate sibling files in `Saves\Campaign\` named after the mission map (incl. custom-campaign missions like "REVOLUTION OVERDRIVE").
2. Container = MPQ. `MPQ\x1b` user-data header precedes the real `MPQ\x1a` archive header at a 512-aligned offset (zezula.net MPQ format). SC2 registers `.SC2Save` as an engine archive type (sc2-arcade-watcher docs).
3. Inside the MPQ: `(listfile)`, `SaveInfo` (metadata), `BankData` (embedded campaign banks). `SC2Data.xml` lists `CampaignBanks` per campaign which persist inside the progress save.
4. Filenames are game-defined: per campaign `SaveName`, `CompletedSaveName`, `PublishArchiveName` — matches local files 1:1. Custom campaigns have no `SaveName`; their progress banks live in `Banks\<author-account-id>\`, only in-mission saves land in `Saves\Campaign\`.
5. Split confirmed: `Saves\*.SC2Save` = campaign progress (fixed names); `Saves\Campaign\` = in-mission saves; also `Unsaved\` (autosaves), `Challenge\`, `Multiplayer\`.
6. wow-mpq 0.7.0 (already a dependency) handles both header variants.

## Implications for save-set isolation
- Vanilla campaigns: fixed filename set -> file-level swap of SaveName/CompletedSaveName/PublishArchiveName trio is surgical.
- Custom campaigns: progress is in `Banks\<author-id>\` (outside Saves) + arbitrary mission names -> whole-dir swap (`Saves\Campaign\` + that campaign's Banks dir) is the robust unit.
- Include `Saves\Unsaved\` in any whole-dir swap, or stale autosaves survive a rollback.

## Gaps
- Entry names (SaveInfo/BankData) community-sourced; verify once via a wow-mpq probe before relying on them.
- Battle.net cloud sync may reconcile campaign progress server-side after a local swap — offline behavior assumed fine, unverified.


---

## Direct launch

Research complete. Deliverable written to `/root/.pi/agent/sessions/--root-work-tries-sc2-campaigner--/subagent-artifacts/outputs/8f7c2ed7-bb56-4762-bf84-9b1e5de70a8a/direct-launch.md` (57 lines, within the 60-line cap).

Key takeaways:
- **`-run <map>` on `Support64\SC2Switcher_x64.exe` is the whole answer** — boots straight into a specific map, no Battle.net, no SC2 menu. Plus `-testmod`, `-displaymode`, `-difficulty`, `-speed`, and any `variables.txt` key as a CLI flag.
- **Offline works** via guest/offline; achievements and co-op break, campaign saves locally.
- **Best UX**: copy the Mass Recall pattern (it's the proven prior art) — `-run "<launcher-map>.SC2Map"`; Agent doesn't interfere with direct spawns.