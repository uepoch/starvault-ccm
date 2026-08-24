# Large loose-Mod performance study (2026-08-24)

## Result

Activating Heart Of Eyeseer took **31.793 seconds** in an unprofiled Windows
release build. **28.122 seconds (88.5%)** were spent copying the already-staged
loose Mod files into the live StarCraft II `Mods` directory.

This is not a map, save-isolation, ledger, or content-hashing bottleneck. The
campaign has 3,027 loose Mod files, and the current rollback-safe workflow
copies every one twice: store blobs to an operation staging tree, then staging
to the live game directory. The second pass was 11.95 times slower than the
first and passed through the Windows filesystem-filter stack, including
Microsoft Defender.

## Test case

- Build: `svccm` 0.2.1, commit `137829b`, optimized native Windows build with
  Rust debug symbols
- Profiler: Superluminal Performance Pro, 8,190 Hz sampling with context-switch
  stacks
- Package: `heart-of-eyeseer`
- Revision: `5c87245c7a6abb61ca300db47748d6d1490f16b1067120ac1489fc5ff80cb289`
- Faction: Heart of the Swarm
- Save isolation: enabled
- External-Mod replacement policy: enabled
- Starting state for the measured activation: vanilla
- Heart map deployment cache: already materialized; activation only needed the
  top-level campaign junction swap

Package footprint:

| Content | Files | Bytes |
| --- | ---: | ---: |
| Total | 3,062 | 612,667,766 |
| Loose Mods | 3,027 | 578,329,103 |
| Slot maps | 35 | 34,338,663 |
| Model Mod | 2,981 | 564,731,297 |
| Data Mod | 46 | 13,597,806 |

The selected profile contained 15 save files and 15 bank files totaling about
9.7 MB. This is small enough to make save isolation a useful control rather
than a competing large workload.

## Unprofiled phase timings

The probe watched the atomically replaced `pending-operation.json` at roughly
1 ms resolution. The first `preparing` timestamp separates preflight from the
journaled preparation work.

| Phase | Seconds | Share of 31.793 s |
| --- | ---: | ---: |
| Preflight and initial journal write | 0.286 | 0.9% |
| Prepare saves, slot, and complete Mods staging tree | 2.354 | 7.4% |
| Apply save transition | 0.065 | 0.2% |
| Swap campaign map junction | 0.011 | 0.03% |
| Apply staged files to live `Mods` | **28.122** | **88.5%** |
| Commit ledger and journal phase | 0.030 | 0.1% |
| Verify, finalize, and remove journal | 0.918 | 2.9% |

The staging pass copied 578.3 MB at about **245.7 MB/s**. The live pass copied
the same bytes at about **20.6 MB/s**, or **9.29 ms per file** across 3,027
files. Both paths were on the same volume. The large difference is therefore
destination and per-file filter overhead, not source decompression or hashing.

The durable workflow boundaries correspond to
`Workflow::transition` in `crates/core/src/workflow.rs`:

1. `PreparedModsTransition::prepare_with_policy` calls
   `Store::materialize_mods`, copying every `mods/` blob to staging.
2. `PreparedModsTransition::apply_standard` calls `copy_atomic` for every
   created target file, copying staging into StarCraft II's live `Mods` tree.

## Superluminal attribution

The fully profiled activation process lived for 38.800 seconds. A separate
unprofiled repetition produced the 31.793-second figure above; the difference
includes profiler overhead and cache-state variation.

The activation workflow generated 78,385 context-switch events in the trace.
Their inclusive distribution was:

| Workflow function | Context-switch hits | Share of workflow hits |
| --- | ---: | ---: |
| `PreparedModsTransition::apply` | 51,220 | 65.34% |
| `PreparedModsTransition::prepare_with_policy` | 23,688 | 30.22% |
| `Workflow::finish_committed` | 2,721 | 3.47% |
| Save prepare and apply combined | 489 | 0.62% |
| `SlotManager::prepare` | 25 | 0.03% |

Within preparation, `Store::materialize_tree` accounted for 23,534 hits, or
99.4% of that phase. Within live application, `copy_atomic` accounted for
47,673 hits, or 93.1% of that phase.

Relevant modules on the wait stacks were:

| Module | Uni-inclusive context-switch share |
| --- | ---: |
| Windows filesystem filter manager (`FLTMGR.SYS`) | 65.54% |
| NTFS | 46.69% |
| Microsoft Defender (`WdFilter.sys`) | 13.52% |
| Gaming Services filter (`gameflt.sys`) | 7.66% |

These values overlap because a single stack can contain several modules. They
are stack attribution, not elapsed-time percentages. They show that Defender
contributes materially, but the broader cause is thousands of synchronous
file creations and replacements through the protected game directory's filter
stack.

Exclusive CPU samples reinforce that conclusion: only 2.8% of samples were in
the probe's Rust executable, while 97.2% were in Windows and filesystem/filter
modules. SHA-256 compression represented about 0.06% of all process CPU sample
weight. Replacing SHA-256 with BLAKE3 would not materially change this
activation.

## Return-to-vanilla finding

Heart's restore path was slower than activation in two repetitions:

| Repetition | Total | Verify active state before journal | Prepare rollback backup | Remove live Mods | Finalize |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 51.313 s | 18.687 s | 28.511 s | 2.578 s | 1.424 s |
| 2 | 52.764 s | 19.269 s | 29.176 s | 2.661 s | 1.548 s |

This follows directly from the safety model. `Workflow::transition` first
verifies the active managed files. Mods preparation then verifies them again,
copies changed created files into the rollback backup, and hashes the backups
before any live file is removed. This preserves crash recovery, but it makes a
large loose deployment expensive to remove as well as activate.

For comparison, restoring the smaller `ued-project` deployment took 4.409
seconds, and reactivating it took 1.627 seconds.

## MPQ packing experiment

The large model Mod was also packed separately to estimate the alternative of
paying a one-time import cost and deploying one archive instead of 2,981 loose
files.

- Input: 564,731,297 bytes in 2,981 files
- MPQ output: 249,542,223 bytes
- First scan/build/verify: 0.082 s / 42.546 s / 3.095 s
- Warm scan/build/verify: 0.054 s / 18.743 s / 3.114 s

The profiled MPQ build took 41.003 seconds wall and 13.672 seconds of process
CPU. About 27.3 seconds were off-CPU. Of in-process CPU samples, 80.71% were in
`miniz_oxide` deflate compression. Context-switch stacks showed the cold-build
penalty came mainly from per-file reads and opens through `WdFilter.sys`; the
warm floor was single-threaded zlib/miniz compression.

Packing therefore does not make import free. It moves the cost to a one-time
operation, shrinks this Mod by 55.8%, and removes 2,980 per-file operations from
every later activation and restore.

## Recommended next experiments

1. **Implemented and measured below:** stage each top-level `.SC2Mod` directory
   beside the live game directory and atomically rename the completed directory
   into place. Mixed-ownership containers retain the per-file fallback.
2. Verify that StarCraft II accepts a junction for each StarVault-owned
   top-level `.SC2Mod` directory. If it does, activation becomes proportional
   to the number of Mod containers rather than their file count.
3. Pack compatible loose `.SC2Mod` directories once during import. The MPQ
   experiment shows a credible trade: an 18.7-42.5 second one-time build for a
   much smaller, single-file deployment.
4. **Implemented and measured below:** verify managed contents once when the
   process establishes a healthy startup state. Later mutations in that
   process check filesystem shape under the global mutation lock. Interrupted
   recovery still performs full content verification before it mutates files.

Changing the hash algorithm is not a recommended activation optimization.

## Atomic-container implementation result

The first recommended experiment was implemented and measured on the same
machine and imported `heart-of-eyeseer` package. The journal-bound Mods plan
now identifies fully owned top-level `.SC2Mod` units. A complete unpacked Mod
directory is renamed from the sibling staging tree into the live `Mods` tree;
a packed `.SC2Mod` is renamed as one file. Containers with borrowed files,
external replacement, or previous ownership continue through the existing
per-file copy path.

The release-mode activation result was:

| Phase | Before | After | Change |
| --- | ---: | ---: | ---: |
| Total activation | 31.793 s | **6.263 s** | **80.3% faster** |
| Initial preflight and journal | 0.286 s | 0.751 s | cold-run variance |
| Prepare through complete Mods staging | 2.354 s | 4.381 s | cold-run variance |
| Apply save transition | 0.065 s | 0.079 s | effectively unchanged |
| Swap campaign map junction | 0.011 s | 0.017 s | effectively unchanged |
| Apply staged files to live `Mods` | 28.122 s | **0.482 s** | **98.3% faster** |
| Commit ledger and journal phase | 0.030 s | 0.033 s | effectively unchanged |
| Verify, finalize, and remove journal | 0.918 s | 0.520 s | 43.4% faster |

The second per-file copy was therefore the correct diagnosis. The new live
deployment phase is proportional to top-level Mod containers plus a shape-only
inventory of loose files, rather than another 578 MB copy through the game
directory. The remaining activation floor is the one store-to-staging
materialization pass; this sample was cold and took 4.381 seconds.

The recovery contract remains unchanged at the resource level. The atomic
operation plan records eligible containers, moved staging content is accepted
as a journal-bound subset during cleanup, and the existing restart tests still
prove either the previous or committed state after every workflow checkpoint.
Regression coverage also proves packed-file moves, unpacked-directory moves,
rollback, finalization, and the mixed borrowed/created fallback.

## Move-to-backup restore result

The restore path now uses the same container boundary in the other direction.
For a fully StarVault-owned `.SC2Mod`, applying a transition renames the live
container into the journal's backup directory. It does not copy or hash every
file immediately before removing it. Rollback renames that exact container
back. A committed transition deletes the journal-bound backup only after the
ledger commit and final shape verification.

Mixed containers remain on the conservative per-file path. A container is not
moved as a unit if it contains a borrowed file, an external replacement, or an
untracked extra file. That fallback preserves content StarVault does not own.

Managed contents are fully verified once when a process establishes its ready
startup health receipt. Subsequent activation, Play, and restore operations in
that process use shape checks while holding the global mutation lock. Recovery
after an interrupted operation does not trust the receipt: it verifies the
journal-bound live, staging, and backup contents before choosing rollback or
finalization.

Native Windows release measurements on the same imported package were:

| Flow | Before | After | Change |
| --- | ---: | ---: | ---: |
| UED to Heart activation | 31.793 s | **5.439 s** | **82.9% faster** |
| Heart to vanilla mutation | 51.313-66.551 s | **4.164 s** | **91.9-93.7% faster** |
| Vanilla to UED activation | 1.627 s | **1.259 s** | 22.6% faster |

The restore benchmark intentionally started its timer after `initialize()`.
In a fresh process, Heart's one-time startup integrity scan took approximately
27 seconds before the 4.164-second restore. The desktop app performs that scan
once and retains the ready receipt, so a restore initiated later in the same
session does not pay it again. Startup work should be surfaced separately in
the UI rather than hidden inside every campaign action.

This design removes both avoidable costs from the normal large-container
restore: the second full verification and the rollback copy. The live data is
still recoverable because a same-volume rename is the backup; no duplicate
578 MB tree is needed.

## Artifacts and cleanup

The resolved Superluminal activation session and small CSV reports are retained
at:

`C:\Users\Martin\AppData\Local\Temp\svccm-activation-profile-20260824`

The resolved session entry point is
`heart-activation\heart-activation.session`. The 544 MB raw ETL and the 20 MB
expanded xperf HTML report were removed after the measurements above were
extracted.

The earlier resolved MPQ session remains at:

`C:\Users\Martin\AppData\Local\Temp\svccm-mpq-profile\svccm-mpq-profile.session`

The benchmark began with `ued-project` active and ended with the same package,
revision, faction, 274 borrowed Mod rows, and 23 created Mod rows. No pending
operation journal remained. A diagnostic poller briefly caused a Windows
sharing violation while replacing the journal; the operation stopped before
live mutation, left no journal, and succeeded once the poller was removed.
