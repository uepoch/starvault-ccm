# Slot manager

Campaign slots are the four fixed locations the game reads:

| Slot | Directory |
|---|---|
| WoL | `<SC2>\Maps\Campaign` |
| HotS | `<SC2>\Maps\Campaign\swarm` (+ `\evolution`) |
| LotV | `<SC2>\Maps\Campaign\void` (+ prologue dir handled via `Mods`/layout rules) |
| NCO | `<SC2>\Maps\Campaign\nova` |

Exact layout constants live exclusively in `core::layout` (architecture rule 2).
A slot holds either the plain Blizzard campaign (empty/default) or exactly one
package revision's `slot/` subtree.

## Transaction state machine

Every switch — activate, restore-to-plain, replace-on-reimport — runs the same
transaction:

```
Idle ──begin──▶ Staging ──verify ok──▶ Verified ──swap──▶ Committed
                   │                      │
                   └──failure──▶ RolledBack ◀──swap failure──┘
```

- **Staging.** Copy strategy: materialize the package's `slot/` tree into
  `<slot>.staging-<n>` inside the same parent directory (same volume ⇒ atomic
  rename later). Junction strategy: create a junction at a temp name pointing
  at `store/packages/<id>/<rev>/slot/`.
- **Verified.** Copy: spot-check file count + sizes against the manifest, full
  hash check only on demand. Junction: verify target exists and is readable.
- **Swap.** Rename current slot contents aside to `<slot>.backup-<n>` (or drop
  the old junction), rename staged entry into place. Same-volume renames give
  near-atomicity; the window where the slot has no contents is microseconds.
- **Committed.** Delete backup. Ledger records the new active revision.
- **RolledBack.** Restore backup / remove temp junction; ledger unchanged;
  error surfaces with the exact failing path.

Crash recovery: leftover `.staging-*` and `.backup-*` directories are detected
at startup; backups older than the last committed ledger state are restored,
staging dirs reclaimed.

## Strategies

```rust
pub trait SlotStrategy {
    fn stage(&self, tx: &SlotTransaction) -> Result<Staged>;
    fn verify(&self, tx: &SlotTransaction, staged: &Staged) -> Result<()>;
    fn swap(&self, tx: &SlotTransaction, staged: Staged) -> Result<()>;
}
```

- **Junction (default).** NTFS directory junctions need no admin rights; the
  game reads through them. Switching is instant and near-atomic; disk usage
  halves. Risks managed explicitly:
  - dangling junctions → startup guard re-points or flags;
  - unsupported volumes/filesystems → automatic fallback to copy at
    activation time, recorded in config;
  - user deletes through the junction path → integrity reconciliation catches
    missing store content and reports which package is affected.
- **Copy (fallback).** Always available; slower and doubles disk usage; chosen
  automatically when junction creation fails, or manually in Settings.

Both strategies sit behind the identical trait; the choice is per-install, not
per-code-path.

## Cross-slot conflicts (M5)

The four slots are simultaneously active and share one runtime namespace:
`Mods\`. Activation therefore computes the union of `mods/**` across all would-
be-active revisions (dependency-store.md §deploy):

- no divergence → proceed;
- same path, different bytes → **block this activation**, name both packages
  and the path, and offer:
  1. deactivate the conflicting slot (its campaign goes back to plain),
  2. reset the other slot to plain campaign and retry,
  3. cancel.

The UI never allows a deployment whose reported state would differ from reality.

## Operations surface

- `activate(slot, rev)` — transaction above.
- `restore(slot)` — return slot to plain Blizzard state (clears to default
  layout contents; layout module defines what "plain" means per slot).
- `deactivate(slot)` — restore + clear ledger row + release dep refs.
- `verify_all()` — full manifest hash pass over slots and deployed mods.
