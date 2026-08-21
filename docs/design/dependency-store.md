# Dependency store

The store is the app's database of record. It is content-addressed, refcounted,
and the sole source of truth for what is deployed into the game directory.

## Layout

```
%APPDATA%\StarVault\CCM\
  config.toml
  store\
    blobs\<ab>\<sha256…>                 # every unique file content, once
    packages\<id>\<rev>\manifest.json    # package revision → ordered file list
    ledger.db                            # SQLite, single writer
```

- `<ab>` = first two hex chars of the SHA-256; standard content-addressed fan-out.
- `<rev>` = content revision id (hash of manifest.json). Re-importing identical
  content is a no-op at the storage layer.
- Blobs are immutable. Nothing ever edits a file in `blobs/`.

## Package manifests

`manifest.json` for a revision:

```json
{
  "id": "tarcade",
  "rev": "b3da…",
  "slot": "lotv",
  "files": [
    { "path": "slot/tarcade.SC2Map/Triggers", "sha256": "…", "size": 4681944 },
    { "path": "mods/RaynorRogue.SC2Mod/DocumentHeader", "sha256": "…", "size": 834 }
  ],
  "dependencies": [
    { "logical_path": "Mods/RaynorRogue.SC2Mod", "sha256": "<container digest>" }
  ]
}
```

Two subtrees by convention, enforced at ingest (see package-model.md):

- `slot/**` — what a campaign slot receives (junction target or copy source).
- `mods/**` — what mirrors into the game's `Mods\`, structure preserved.

## Ledger schema

```sql
CREATE TABLE active_slots (
  slot      TEXT PRIMARY KEY,   -- wol | hots | lotv | nco
  rev       TEXT,               -- NULL = plain/default campaign
  pkg_id    TEXT
);

CREATE TABLE deployments (
  game_path TEXT NOT NULL,      -- Mods\SCORE\SCORE-Other.SC2Mod/... normalized
  sha256    TEXT NOT NULL,
  rev       TEXT NOT NULL,      -- owning package revision
  PRIMARY KEY (game_path, rev)
);

CREATE TABLE blob_refs (
  sha256    TEXT PRIMARY KEY,
  refcount  INTEGER NOT NULL DEFAULT 0
);
```

Every filesystem mutation happens inside a ledger transaction: write blobs
first (harmless if orphaned), commit ledger rows, then mutate the game
directory, then mark the transaction committed. Crash between steps leaves
recoverable state; startup reconciliation detects and repairs drift.

## Deploy algorithm (M2: dumb-and-faithful)

1. Read `active_slots`. For each non-null slot, load its manifest.
2. Union all `mods/**` entries across active manifests.
3. Group by target game path:
   - same hash from multiple packages → deploy once, refcount per contributing
     revision;
   - **different hashes for one path** → genuine conflict → block per M5 (see
     slot-manager.md), never silently overwrite.
4. Materialize: copy/link blobs into `Mods\` under their preserved relative
   paths.
5. Record deployments + bump blob refs in one transaction.

Uninstall/deactivate decrements refs; zero-ref paths are removed from the game
directory and zero-ref blobs are GC'd (deferred, batched).

Same-name-different-content mods coexist in the store by construction (M1);
they can only collide at *deploy* time, where M5 policy applies.

## Import pipeline

```
zip ─▶ stream-hash files (never whole-archive in memory)
    ─▶ normalize (package-model.md)
    ─▶ identity check vs installed packages (K3 prompt)
    ─▶ interactive confirm (UI shows title/author/slot guess/warnings)
    ─▶ ingest: write blobs, write manifest, ledger tx
```

Progress events fire per-file with byte counts; cancellation leaves orphan
blobs that startup GC reclaims.

## Integrity

- Startup reconciliation: verify each active slot's junction/copy target
  against its manifest (spot-check hashes on copy strategy; existence checks on
  junction strategy); verify deployed `Mods\` paths exist with right sizes.
  Drift is repaired when possible, reported otherwise.
- Full hash verification runs on demand ("Verify installation" action), not on
  every startup — half-gigabyte packages make full verification a deliberate
  act.

## Future: registry (v2)

Package identity (`[package] id` + `source.url`) is registry-ready today. A
future registry serves signed manifests; the store gains a `sources` table and
the importer gains a download backend. No schema change to blobs or manifests
is anticipated.
