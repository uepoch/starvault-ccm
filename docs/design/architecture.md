# Architecture

StarVault CCM is a Tauri 2 desktop application with a pure-Rust domain core.
The core is the product; the shell is its mouthpiece.

## Workspace layout

```
crates/
  core/            pure Rust, zero Tauri deps, builds and tests on Linux
    src/
      layout/      GameLayout trait + WindowsLayout impl.
                   ALL knowledge of the SC2 directory contract lives here.
      config/      TOML config under %APPDATA%\StarVault\CCM\
      mpq/         MPQ archive reader (read-only in v1)
      package/     container discovery, DocumentHeader/DocumentInfo parsing,
                   legacy metadata.txt parser, campaign.toml model, normalizer
      store/       content-addressed blob store + SQLite ledger
      library/     installed-package scan, statuses, old-CCM migration
      slots/       SlotManager + SlotTransaction state machine
      launch/      pre-flight verification + game process spawn
      report/      report_error() seam (no-op backend until release)
  app/             Tauri shell: commands, events, tray, updater wiring (M2)
webui/             React + Vite + TypeScript + Mantine frontend (M2)
```

## Structural rules

1. **The core never imports `tauri`.** Every crate in `core/` compiles and is
   unit-tested on Linux against temporary directory trees. The shell calls the
   core; the core never knows the shell exists.
2. **One module owns path knowledge.** All game-directory paths are produced by
   `layout::GameLayout`. No other module concatenates game path strings. This
   is what keeps macOS a porting task instead of a rewrite.
3. **Paths enter as parameters.** Functions take `&Path` / `PathBuf`; nothing
   reconstructs absolute paths from string fragments (the original tool's
   `path.Replace` bug class is structurally impossible here).
4. **Typed error taxonomy.** See below. The UI maps variants to human
   sentences; only `Internal` reaches crash reporting.
5. **Async at the edges.** Long operations (import, switch) run on background
   threads inside the shell and emit progress events; core functions are
   synchronous and cancellation-aware where meaningful.

## Error taxonomy

```rust
pub enum Error {
    User(UserError),            // locked files, disk full, user picked wrong exe
    Package(PackageError),      // malformed zip, unreadable container,
                                // unresolved dependency, schema violation
    Environment(EnvironmentError), // no game install, non-Windows target,
                                   // non-NTFS volume when junctions required
    Internal(InternalError),    // bugs. reported via report_error(), never
                                // blamed on the user
}
```

Every `PackageError` carries the offending container path so import failures
point at the exact file inside the zip.

## Data flow

```
zip file ──▶ package::normalize ──▶ interactive confirm (UI)
                                        │
                                        ▼
                              store::ingest
                     (blobs + manifest + ledger tx)
                                        │
              ┌─────────────────────────┤
              ▼                         ▼
      slots::activate              library::scan
   (stage→verify→commit)          (reads store + slots)
              │
              ▼
   game dir: Maps\Campaign\<slot>  +  Mods\ mirror
              │
              ▼
        launch::preflight ──▶ StarCraft II.exe
```

## Concurrency and state ownership

- The SQLite ledger is the single writer for deployment state. All mutations go
  through `store::Ledger::transaction()`; the filesystem is only ever mutated
  inside a ledger transaction that can be rolled back.
- Active-slot state is *derived* (which package revision each slot points at)
  but cached in the ledger for fast UI reads; startup reconciliation compares
  cache against reality and repairs or reports drift.

## Testing strategy

- Core: unit tests per module plus integration tests that drive full flows
  against temp trees. Golden fixtures: real-world packages (`example.zip`,
  `example-fixed.zip`) reduced to committed test vectors.
- Junction-specific behavior: gated to the Windows CI job (`windows-latest`);
  everything else runs on Linux.
- Shell: thin command wrappers, covered by a smoke test that boots the app
  headless in CI where feasible.
