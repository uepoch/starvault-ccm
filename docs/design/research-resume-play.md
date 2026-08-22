# Research: "Play → resume last save" — can we launch SC2 directly into the newest campaign save?

## Summary

Yes — **easy**. `SC2Switcher_x64.exe -run <path\to\save.SC2Save>` loads a saved game directly (reverse-engineered engine behavior: the `run` CVariable dispatches on extension, `.SC2Save` → mode 3 "load saved game"). Inspection is trivial without any parsing: newest mtime under `Saves\Campaign\*.SC2Save`, where the filename *is* the mission name. The single biggest caveat: the save-mode of `-run` is not yet A/B-tested on this machine (5-minute test settles it).

## Verdict matrix

| Task | Verdict | Mechanism | Confidence |
| --- | --- | --- | --- |
| Inspect bank | **trivial** (but skip it) | `.SC2Bank` = plain XML (`<Bank><Key name=…>`); std/quick-xml parse. Not needed for Play-resume | high (format), n/a (need) |
| Inspect save ("latest for this campaign/mission/time") | **trivial** (no parse at all) | glob `Saves\Campaign\*.SC2Save` + max mtime; filename = mission display name; campaign = our own slot ledger | high — **verified locally** |
| Deep-parse save (SaveInfo etc.) | **easy, optional** | wow-mpq 0.7.0 `Archive::open` (handles `MPQ\x1b` transparently) → `user_data()`, `list()`, `read_file()` | med — API verified on docs.rs; entry names unconfirmed |
| Launch into save | **easy** | `SC2Switcher_x64.exe -run "<absolute save path>"` | med-high — engine-documented, untested here |
| Launch into save, fallback | **easy** | shell association `Blizzard.SC2Save\shell\open\command` = `…SC2Switcher.exe "%1"` (positional; verified working for replays) | med |
| battlenet:// deep link | **blocked** | only `starcraft`, `map/<region>/<id>`, `profile/...` exist; no save URI | high (absence) |

## Findings

1. **[verified, local]** `Saves\Campaign\Liberation Day.SC2Save` exists — in-mission saves are literally named after the mission. Newest-mtime glob = "last save, which mission". Flat `Saves\*.SC2Save` (LibertyCampaignSave…) are campaign-*progress* snapshots, not mission resumes — do NOT target those for resume.
2. **[verified, local]** Both save kinds start `4D 50 51 1B` (`MPQ\x1b`) with `(StarCraft II save` in the user-data region; payload sectors are bzip2 — `strings`-style probing stops at the marker. Any richer metadata needs a real MPQ open.
3. **[verified, docs.rs]** wow-mpq 0.7.0 `Archive` has `user_data()`, `list()` (via `(listfile)`), `list_all()`, `read_file()` — the header is handled transparently. Only needed if we want in-save timestamps/mission names beyond filename+mtime.
4. **[verified, primary source]** sc2-arcade-watcher RE docs (`archive-system.md`): the `run` **CVariable** is "a file path to launch directly into a game, replay, or saved game", parsed by extension: `.SC2Map`→map, `.SC2Replay`→replay, `.SC2Save`→**"Load saved game"**. Being a CVar, any `variables.txt` key works as a CLI flag (matches TL.net: "anything in variables.txt can be run with -command notation").
5. **[verified, fetch]** Blizzard's s2 product config registers `.sc2save` → ProgID `Blizzard.SC2Save` (and `.sc2replay` → `Blizzard.SC2Replay`) via `program_associations`; executable = `%binarypath%`. User-verified registry value (replays): `…\Support\SC2Switcher.exe" "%1"` — positional arg, boots SC2 and loads the file after login.
6. **[verified, primary]** sc2mapster's full SC2Switcher switch list (`-run -testmod -displaymode -trigdebug -preload -NoUserCheats -reloadcheck -meleeMod -difficulty -speed`) documents **no** `-loadsave`/`-loadfile` switch — `-run` with a save path (per #4) is the whole mechanism. A `--saveas` string exists in SC2.exe strings (TL.net) but is unverified/irrelevant.
7. **[verified, prior art]** Mass Recall's "launcher" is a map (`SCMR Campaign Launcher.SC2Map`) spawned via `-run` — prior art never launches into a save; the launcher-map pattern is the industry fallback if save-mode misbehaves (our junction swap already supplies campaign content).
8. **[verified, product config]** Battle.net passes URIs to the game via `-link`; only map/profile URI forms are known — no save deep link. Dead end, correctly.
9. **[inferred]** Banks dir not enumerated locally (this session has no shell; `read` refuses dirs). XML format is community-documented and banks are irrelevant to picking the newest save — only useful later for custom-campaign progress UI.

## Minimal "Play → resume" in svccm

```
pick():  resolves Saves dir (existing discover: Documents → Accounts\<acct>\<profile>\Saves)
         newest = argmax(mtime) over Saves\Campaign\*.SC2Save   (+ Saves\Unsaved\* if we honor autosaves)
         label  = file_stem (mission name) + relative mtime      → "Resume: Liberation Day · 2 h ago"
launch(): preflight (no running SC2 — existing check), junction swap already active (slot pinned)
         spawn: SC2Switcher_x64.exe  -run  "<absolute path to newest save>"
         [alt]  if -run save-mode fails on this build: spawn the Blizzard.SC2Save shell-open command,
                i.e. SC2Switcher with the path as positional arg (replay-verified form)
         [fallback] -run the campaign's launcher map (Mass Recall pattern)
```

No MPQ parsing, no bank parsing, no new deps — mtime + filename + one spawn. (ponytail: add wow-mpq SaveInfo parsing only if filename/mtime proves insufficient for UI labels.)

## Top risk

**`-run <save>` save-mode is engine-RE-documented but not A/B-tested here**; plus saves are account-scoped (`Accounts\<id>\<profile>\Saves`) — loading may require login/offline-profile context matching that account. Mitigation: one manual test (launch, confirm boots into mission, confirm zero menus); positional-arg and launcher-map fallbacks both keep the feature shippable.

## Sources

- Kept: sc2-arcade-watcher/sc2-file-format-docs `archive-system.md` (github.com/sc2-arcade-watcher/sc2-file-format-docs) — RE of the `run` CVar, `.SC2Save`→load-saved-game; the launch verdict rests on this.
- Kept: sc2mapster.github.io "Testing map without the editor" — authoritative SC2Switcher switch list.
- Kept: Blizzard s2 product config via blizztrack.com — `.sc2save`→`Blizzard.SC2Save` installer registration; `-link` URI arg.
- Kept: tl.net threads 408294 (association registry value, verified replay load) & 209815 (variables.txt keys as CLI flags).
- Kept: CurseForge Mass Recall 7.3.1 shortcuts — `-run <launcher map>` prior art.
- Kept: docs.rs wow-mpq/0.7.0 `Archive` — user_data/list/list_all/read_file.
- Dropped: fileinfo.co/openthefile "double-click loads save" — SEO lore, superseded by #4/#5.

## Gaps

- End-to-end `-run save.SC2Save` on this machine: **pending manual test** (top risk above). Also: does SC2 require the save to sit under the live profile's Saves tree (our junction satisfies this by design — confirm during the test)?
- wow-mpq extraction of `SaveInfo`/`BankData` member names: community-sourced, unverified — irrelevant unless deep parsing is ever wanted.
- Local bank file contents: not read (no shell/dir-list in this session) — parent can `cat` one under `Banks\` if ever needed.

---

## Postscript: launch path resolution (live-tested 2026-08-22)

The full matrix, after hands-on testing on the target machine:

| Method | Result |
| --- | --- |
| `Battle.net.exe --exec="launch S2"` | **WORKS** — full SSO (`-sso=1 -launch -uid s2`), zero clicks. THE launch path. |
| SC2Switcher + `-sso=1 -launch -uid s2` | Partial — works only with a warm agent session; cold = legacy login page. Fallback only. |
| SC2Switcher + `-portal 127.0.0.1` (offline trick) | Ignored on current build. |
| SC2Switcher + `-gnoLoginToken -unauthenticated -login offline_user` | Still shows login. |
| `battlenet://starcraft` / `battlenet://STC` deep links | Received by the app (UriController logs them) but silently dropped — no launch. |
| `-run <save.SC2Save>` | Confirmed dead for resume (engine dispatch exists but the game never reaches it without the SSO context). |

Critical operational detail: the SSO token is minted per-launch inside
Battle.net's process and handed to the game out-of-band (registry shows
`ACCOUNT=(hidden) ACCOUNT_TS=...`) — it is not injectable, ever, from an
external process. Only `--exec` delegation gives us authenticated launches.

**Race guard (required):** the Agent keeps tracking a closed game session for
a few seconds; launching inside that window crashes the game at boot with
"an error occurred starting StarCraft II". Wait for the process to exit,
then ~6s more, before delegating.
