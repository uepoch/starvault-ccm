# Campaign deployment workflow

The filename remains for old links, but campaign deployment no longer manages
independent active slots. One workflow transitions the entire game between
vanilla and one package.

## Journal

`pending-operation.json` sits beside the store and contains:

```text
version
operation_id
kind: activate | restore | repair
phase:
  preparing
  prepared
  saves_swapped
  slots_swapped
  mods_swapped
  ledger_committed
  rollback_verified
previous_campaign
target_campaign
saves_participated
backup paths
staging paths
save recovery proof
previous and target campaign-slot object identities
Mods rollback-plan digest
repair backup identities
```

The workflow first writes a `preparing` journal that owns every deterministic
staging and backup path. It then stages the resources and atomically advances
the journal to `prepared`, including the save recovery proof, exact previous
and target slot identities, the Mods rollback-plan digest, and any repair
backup identity. The save proof binds the operation, ownership transition,
previous and target Saves and Banks trees, and every archived save-set update.
The campaign identity binds the single `Maps\Campaign` object to its exact kind
and content or junction target. Recovery therefore accepts only the prepared
previous or target state, rather than trusting a replacement that happens to
occupy an expected path. This makes a process exit during staging recoverable
without scanning for filename patterns. The journal is flushed before each
destructive phase, and backups remain until the ledger commits and the final
tree verifies.

## Activation

Activation runs in this order:

1. refuse the operation while StarCraft II is running;
2. recover or reject an existing journal;
3. load and hash-verify the target manifest;
4. verify the current owned campaign files and managed Mods;
5. write the `preparing` journal with every owned artifact path;
6. stage the complete synthetic `Maps\Campaign` view and Mods trees;
7. reject a different unowned file already present at a target Mods path;
8. classify an identical unowned file as borrowed and advance the journal to
   `prepared`;
9. transition saves when isolation is enabled;
10. swap the previous campaign-root object for the target;
11. replace the managed Mods set;
12. commit the active campaign and managed Mods rows;
13. verify the result, delete backups, and clear the journal.

Save, campaign, or Mods errors abort the operation. The workflow restores the
previous state and returns the error. It does not log and continue.

## Managed Mods

Every deployed path has one disposition:

- `created` means StarVault wrote the file;
- `borrowed` means the same bytes already existed outside StarVault ownership.

Restore removes a created file only when its current hash still matches the
ledger. It never removes a borrowed file. A changed managed file blocks the
operation and remains on disk for inspection or repair.

File-to-directory and directory-to-file replacements use staging and backups.
Filesystem code handles regular files, directories, symbolic links, and
Windows directory junctions according to their actual type.

## Restore and repair

`restore_vanilla()` transitions isolated saves to the plain owner, restores
the exact loose `Maps\Campaign` tree preserved at activation, removes unchanged
created Mods, preserves borrowed Mods, commits an empty `active_campaign`, then
verifies vanilla.

`repair_active()` uses the same journal and target manifest. The explicit
Repair action backs up and replaces drifted StarVault-created files. It never
overwrites a borrowed file; changed borrowed content returns a typed error.

## Recovery

On startup the workflow reads both the journal and the ledger. A `preparing`
operation has not touched live game data, so recovery removes all journal-owned
partial staging artifacts. Otherwise, if the ledger still names
`previous_campaign`, it rolls the filesystem back. If the ledger names
`target_campaign`, it verifies the target and finalizes cleanup. Reading the
ledger is required because a process can stop after the SQLite commit but
before it writes the `ledger_committed` checkpoint. Any other combination
produces `recovery_required`. Journal loading and removal also verify the
opened journal object's identity, size, schema, and exact expected contents so
a link or file substitution cannot redirect recovery or cleanup.

Recovery-required state preserves the journal, staging trees, and backups. All
mutations remain blocked until a repair or operator action can prove one state.

## Campaign-root junction and copy behavior

Official campaign maps live in the game archives; `Maps\Campaign` is only the
loose override tree. StarVault therefore builds one complete synthetic view and
normally exposes it through one junction at `Maps\Campaign` for every faction:

- Wings of Liberty package maps are placed at the synthetic root;
- Heart of the Swarm maps are placed below `swarm\`;
- Legacy of the Void maps are placed below `void\`;
- Nova Covert Ops maps are placed below `nova\`;
- the other faction directories, including `voidprologue\`, remain empty.

The pre-activation loose tree is renamed intact to
`Campaign.starvault-plain`; return to vanilla puts that exact object back.
StarVault never copies official maps out of the game archives. If junction
creation is unavailable, the same synthetic view can be copied as a fallback.

Archive normalization recursively discovers map containers, removes wrapper
and mirrored `Maps\Campaign\<faction>` prefixes, and preserves meaningful
logical subdirectories beneath that point. Mods use their separate managed-file
transaction and are not placed in the campaign-root junction.

Save moves across volumes detect `ErrorKind::CrossesDevices` and use
copy-then-remove. Sharing violations from antivirus or OneDrive receive a
bounded retry. Exhausted retries roll back.
