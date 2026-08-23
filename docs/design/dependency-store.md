# Dependency store

The store keeps immutable file blobs, one manifest per package ID, the fresh
SQLite ledger, and the pending-operation journal.

## Layout

```text
%APPDATA%\StarVault\CCM\
  config.toml
  store\
    pending-operation.json
    blobs\<prefix>\<sha256>
    packages\<package-id>\manifest.json
    ledger.db
    staging\...
    backups\...
```

Blobs are immutable. Package and config writes use a temporary file, flush the
contents, then atomically rename it into place.

## Package manifest

Each package ID has one current `manifest.json`. The manifest records the
validated package ID, revision, faction, metadata, import time, and canonical
file records.

The revision hash covers only:

- faction;
- sorted canonical path;
- SHA-256;
- size.

Metadata, package ID, and import time do not change the revision. An identical
reimport therefore produces the same revision. Metadata edits replace the
manifest atomically without changing it.

Reimporting an inactive package replaces its manifest atomically. Reimporting
or removing the active package returns `active_package_requires_restore`.

## Fresh ledger schema

```sql
CREATE TABLE active_campaign (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    package_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    faction TEXT NOT NULL
);

CREATE TABLE managed_mods (
    path TEXT PRIMARY KEY COLLATE NOCASE,
    sha256 TEXT NOT NULL,
    disposition TEXT NOT NULL
        CHECK (disposition IN ('created', 'borrowed'))
);
```

There is no deployments table or active-slot table. `active_campaign` has zero
or one row. `managed_mods` records which paths StarVault created and which
matching external paths it borrowed.

## Inventory and garbage collection

Inventory returns valid packages and explicit corrupt-package records. It does
not hide an unreadable manifest.

Garbage collection first reads every manifest and computes the reachable blob
set. If any manifest is unreadable, collection aborts without deleting a blob.
This rule trades leaked disk space for recoverable package data.

## Import limits

Archive analysis enforces these limits before commit:

- 20,000 entries;
- 2 GiB per file;
- 8 GiB total declared uncompressed content;
- 512 bytes per entry path;
- free space of declared content plus 1 GiB.

Extraction and ingestion run on blocking workers. Cancellation checks occur no
more than 4 MiB apart, including within one large file. Each operation uses a
unique scratch directory and removes it on every terminal state.

## Unsupported old stores

The fresh schema has no compatibility layer for the previous alpha store.
Opening a legacy schema returns an unsupported-format error. Follow
[`../alpha-reset.md`](../alpha-reset.md) instead of adding automatic
conversion code.
