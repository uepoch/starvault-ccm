# Architecture

StarVault CCM is a Tauri desktop application with a Tauri-independent Rust
core. The central invariant is simple: the game has zero or one active custom
campaign.

## Layers

`crates/core` owns package identity, manifests, the content store, save
transitions, game-file deployment, preflight, operation journaling, and startup
recovery. It accepts paths and typed inputs. It does not import Tauri.

`crates/app` owns process state and IPC adapters. It validates each request,
resolves opaque IDs against current discovery, acquires the mutation mutex,
calls one core workflow, logs full errors locally, and returns a stable result
to the webview.

`crates/app/ui` owns presentation and frontend state transitions. It does not
sequence filesystem work or infer deployment state from cached package rows.

`core::layout` is the only module that knows StarCraft II paths. Other modules
receive paths from layout APIs.

## Process state

`AppState` holds one process-wide mutation mutex. These operations acquire it:

- activate and Play;
- restore and repair;
- import commit and package removal;
- configuration changes that affect saves or deployment;
- clear-all.

The lock covers preflight, any needed activation, and process launch during
Play. It prevents a second command from observing or changing an intermediate
state.

## Core workflow

The application workflow module exposes:

```text
activate(package_id)
restore_vanilla()
repair_active()
preflight(package_id | current)
initialize() / recover_pending()
```

Activation verifies the target manifest and current game state before it
writes the operation journal. It stages all target content before swapping
saves, campaign files, or Mods. The ledger commit occurs only after those
swaps succeed. Final verification occurs before backups and the journal are
removed.

Startup recovery reads the journal and the ledger before accepting mutations.
If the ledger still names `previous_campaign`, the filesystem rolls back. If
it names `target_campaign`, the target is verified and cleanup is finalized,
even when the last journal checkpoint predates the SQLite commit. If neither
state can be proven, startup reports `recovery_required` and preserves every
remaining backup and staging path. Journal-bound proofs cover the exact save
transition, archived save sets, and previous and target campaign-slot objects;
recovery rejects unknown edits or substituted files instead of inferring
ownership from path names.

## State and responses

The core state model is:

```text
active_campaign = none | { package_id, revision, faction }
live_save_owner = plain | package(package_id)
```

The main frontend responses are:

```text
ActiveCampaign { id, revision, faction }

LibrarySnapshot {
  entries,
  active_campaign,
  health
}

Health {
  state: ready | drifted | recovery_required,
  issues
}
```

`initialize()` returns a `StartupReport` after recovery. Campaign commands are
`list_library`, `activate_package`, `play_package`, `restore_vanilla`, and
`repair_active`. There is no separate Campaigns command set and no standalone
game-launch command.

## Errors and diagnostics

Core errors use four categories: `User`, `Package`, `Environment`, and
`Internal`. Tauri maps them to:

```text
CommandError {
  kind,
  code,
  message,
  path,
  retryable,
  report_id
}
```

The frontend never receives a raw Rust error chain. The local operation log
keeps the chain and full paths. Telemetry receives only panics and explicit
`Internal` errors after opt-in, with safe operation and error-code tags.

## Test boundaries

Core integration tests use temporary store, game, and profile trees. Test-only
failpoints stop activation after every journal phase. Restart tests must prove
the recovered tree matches either the complete previous state or the complete
committed target.

Windows tests cover junctions, directory links, locked files, sharing
violations, and cross-volume save moves. The frontend tests presentation and
its reducers with mocked typed adapters.
