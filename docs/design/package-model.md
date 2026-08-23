# Package model

A package is an imported custom campaign identified by a validated
`PackageId`. The importer accepts community archive layouts and normalizes them
to campaign and Mods records in the store manifest.

## Package identity

`PackageId` contains 1 to 64 lowercase ASCII characters. Alphanumeric segments
use one dash as a separator. Validation rejects:

- empty segments or repeated dashes;
- path separators and rooted paths;
- `plain`;
- Windows device names;
- trailing dots or spaces;
- an ID that differs from an installed ID only by case.

Every store entry point accepts the validated type rather than an unchecked
string.

## Container discovery

The importer finds `.SC2Map` and `.SC2Mod` containers anywhere in the archive.
It supports loose directory containers and packed MPQ files. Extension and
path matching follows Windows case-insensitive behavior.

Dependency declarations in `DocumentHeader` and `DocumentInfo` are inspected
for local Mod references. Blizzard-installed and Battle.net references remain
external. An unresolved local reference is an import warning because some
campaigns intentionally depend on separately installed content.

## Metadata and faction

Legacy `metadata.txt` values provide title, author, description, version, and
a faction guess. The import wizard shows the guess and requires a user choice
when no faction can be inferred.

Metadata is not package identity and does not affect the revision. Editing it
atomically replaces the manifest without rewriting blobs.

## Canonical files and revision

Normalization produces canonical campaign and Mods paths. It preserves nested
Mods paths and never rewrites an MPQ container. Two source entries that map to
one canonical path are accepted only when their content matches.

The revision is the hash of faction and sorted records containing path,
SHA-256, and size. Package ID, metadata, and import time are excluded.

The stored form is:

```text
packages/<package-id>/manifest.json
```

There is no revision directory and no `latest_rev` selection. A Library row
always refers to the package's sole current revision.

## Replacement

An identical inactive reimport is a content no-op. A different inactive
reimport writes needed blobs, creates a complete temporary manifest, flushes
it, then atomically replaces `manifest.json`.

An active package cannot be replaced or removed. The command returns
`active_package_requires_restore`, and the UI directs the user to Return to
vanilla.

## Import operation lifecycle

The backend keeps an operation and its cancellation token addressable until
terminal cleanup. Its state is one of:

```text
Analyzing | Ready | Ingesting | Cancelled | Failed | Completed
```

Analysis and ingestion run on blocking workers. Cancellation checks occur
within large files, not only between files. Failure and cancellation remove
the operation scratch directory before a retry can reuse the package ID.
