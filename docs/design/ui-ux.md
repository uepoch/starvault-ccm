# UI / UX

Frontend: React + Vite + TypeScript + Mantine, talking to the core through
typed Tauri commands. The frontend holds no domain logic; it renders core
state and forwards intents.

## Screens

1. **Library.** Grid of installed packages: cover art (when present), title,
   author, version, slot badge, status (active where, warnings count). Actions:
   activate, replace/re-import, remove, show files. Drag-drop zips anywhere →
   import wizard.
2. **Campaigns.** Four slot cards (WoL / HotS / LotV / NCO) mirroring the
   mental model every CCM user already has. Each card: current content ("Plain
   campaign" or package title+version), Activate (package picker filtered by
   slot), Restore to plain, warning icon with explanatory tooltip.
3. **Log.** Chronological operation log (imported X, switched Y, repaired Z),
   filterable, copyable — the support artifact.
4. **Settings.** Game exe path (validated live), store location, strategy
   override (junction/copy), telemetry opt-in toggle, about + naming policy.

## Flows

### Import wizard (K2)

```
drop/browse ─▶ analyze (progress per file)
            ─▶ confirm: detected title/author/slot guess [editable]
                        warnings list (unresolved deps, dedup notes)
                        replace-existing prompt when identity matches (K3)
            ─▶ ingest (progress, cancellable)
            ─▶ done: "Activate now?" shortcut
```

Slot guess shows its basis ("matched 'lotv' in campaign=Legacy of the Void").
Unknown slot is an explicit choice the user makes from four buttons — nothing
is ever silently bucketed.

### Conflict dialog (M5)

Names both packages, the conflicting `Mods\` path, and both content hashes'
owners. Options: deactivate other slot / reset other slot to plain / cancel.
No "overwrite" option exists anywhere in the product.

### Migration (P2)

On first run, detect `%APPDATA%\SC2CCM\SC2CCM.txt` and a populated
`CustomCampaigns\`: offer "Import your existing campaigns?" Per-campaign list
with detected metadata, import runs the normal pipeline (so legacy packages
get normalized, hashed, and given campaign.toml). Old CCM files are left in
place; cleanup is manual and documented.

## Progress and cancellation

Every long operation emits structured progress events (operation id, phase,
bytes done/total, current file). Cancel is honored at file boundaries; partial
imports reclaim orphan blobs at next startup GC.

## Error presentation

Typed errors map to human sentences plus a "details" expander with the raw
chain. `Internal` errors additionally get a report-id when telemetry is opted
in. The log screen records everything regardless of telemetry settings.

## Visual direction

Dark-first, dense but calm; Mantine components with a restrained SC2-flavored
accent palette. Slot cards are the visual anchor — the four-slot grid should be
recognizable to CCM users within one glance. No decorative animation beyond
functional transitions (staging progress, swap completion).
