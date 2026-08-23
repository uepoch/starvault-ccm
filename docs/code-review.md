# Code quality review — 2026-08-23

Sources: three parallel reviewer agents (core, app, UI), full project
diagnostics (LSP, ast-grep, jscpd, zizmor, opengrep, gitleaks), manual
triage. Every finding is triaged: real issue → action point; false positive
→ accepted convention with rationale. No repetitions: one line per point.

## Conventions accepted (non-issues, do not re-report)

- `.lock().expect("… poisoned")` for every mutex: documented Rust convention
  for poisoned-lock guards (panicking on poison is correct — state is
  untrustworthy afterwards).
- `unwrap()` inside `#[cfg(test)]` modules (container, mpq, docinfo, header,
  config, normalize tests): tests fail loudly on purpose.
- `tsconfig.*.json` "comments not permitted": tsconfig is JSONC; the JSON
  language server is wrong, the files are valid.
- jscpd duplicates across container/mpq/docinfo tests: `#[cfg(test)]`
  fixture helpers. (Consolidation tracked as B15 below.)
- gitleaks hits on `target/**/*.rmeta` / `node_modules`: build artifacts of
  public libs, gitignored, never committed. Silenced via `.gitleaksignore`.
- Sentry DSN embedded in telemetry.rs: public identifier by design.
- ESM imports without file extension in .tsx: standard bundler resolution.

## Action points

One commit per point. Status: ☐ todo / ☑ done (commit).

### A. CI / release hardening

- ☑ A1 (4b32f9d) — release.yml: pass `inputs.version` through `env:`, not
  template interpolation (shell injection).
- ☑ A2 (e506917) — pin every action to a commit SHA. `dtolnay/rust-toolchain`
  stays on its moving `stable` branch by design (it installs the current
  stable compiler; pinning would freeze the toolchain forever).
- ☑ A3 (341f3af) — `persist-credentials: false` on all checkouts.
- ☑ A4 (f3ff197) — least-privilege permissions: none at top level;
  `contents: write` only on the release build job.

### B. Rust core (from reviewer + lens)

- ☐ B1 **HIGH** — slots.rs:250/226/378, launch.rs:85 — `is_symlink()` is
  false for NTFS junctions; `reconcile` never detects dangling junctions on
  Windows (crash recovery dead on the only v1 platform). Fix: cfg(windows)
  `FileTypeExt::is_symlink_dir` helper at the 3 sites + a Windows test.
- ☐ B2 **HIGH** — slots.rs:217-236 — `restore` never redeploys the mods
  union of remaining active slots: the restored package's `mods/**` stay in
  the game's `Mods\` forever (global namespace, stale mods keep loading).
  Fix: recompute union after clearing, remove paths absent from the new
  union, then `apply_mods_union`. Needs a "union can shrink" step
  (`apply_mods_union` only adds/overwrites). Add core test.
- ☐ B3 **MED** — launch.rs:285 — `GAME_SHUTDOWN_GRACE` sleeps 6 s
  unconditionally; sleep only when a running instance was observed.
- ☐ B4 **MED** — launch.rs:282 — unbounded `while sc2_running()` wait; bound
  (~60 s) and return a UserError.
- ☐ B5 **MED** — tracing gaps: spans missing on `remove_package`,
  `set_metadata`, `saves::swap` (highest-stakes data), `remove_sets`,
  `reconcile`; `launch` span lacks `exe` field; `swap` lacks slot/rev.
- ☐ B6 **MED** — saves.rs:200-239 — `sweep_into` uses `fs::rename`; EXDEV
  on relocated-Documents multi-volume setups; fall back to copy on
  `CrossesDevices`.
- ☐ B7 **MED** — store.rs:137 — "orphans reclaimed by GC" claim is false
  (only GC lives inside remove_package); extract `Store::gc()` or fix the
  comment.
- ☐ B8 **MED** — normalize.rs:203-207 — directory maps flatten to
  `slot/<basename>` while packed maps keep their subfolder: same package
  packed vs unpacked yields different canonical manifests. Preserve the
  wrapper-relative path for map containers, or document the asymmetry.
- ☐ B9 LOW — consolidate slot-owned sibling list (slots.rs:388,
  launch.rs:136, library.rs:101) into one const in layout.rs; drop the dead
  `swarm\\evolution` entry.
- ☐ B10 LOW — store.rs:219 — "Load a stored manifest" doc line sits on
  `set_metadata`; move it to `load_manifest`.
- ☐ B11 LOW — layout.rs:44-59 — `GameLayout` trait single-impl and
  `slot_dirs` dead; delete unless a second layout is imminent.
- ☐ B12 LOW — import.rs:132 — `slug("")` yields an empty package id when
  `metadata.txt` has `title=`; treat empty values as None.
- ☐ B13 LOW — copy_with_retry retries are silent (`tracing::warn!` per
  attempt); slots.rs:84 discards reclaim_leftovers notes.
- ☐ B14 LOW — error taxonomy drift: ledger errors and "ingest cancelled"
  flow through `pkg_err`; use UserError/Environment respectively.
- ☐ B15 LOW — test fixture helpers duplicated across end_to_end/normalize/
  container tests; move to tests/common/mod.rs. (The 4 near-identical dir
  walkers are optional consolidation, skip.)
- ☐ B16 LOW — test gaps: `remove_package` and `set_metadata` have zero core
  tests; add the restore/union invariant test (fails until B2 lands);
  empty-slug edge (B12).

### C. Tauri app layer

- ☐ C1 **BLOCKER** — commands.rs:78-87 — `legacy_roaming_dir` strips two
  parents; the app identifier is one segment (`dev.starvault.ccm`) so
  `detect_legacy_ccm` always returns None — migration detection is dead.
  Fix: drop one `.parent()`, fix the comment.
- ☐ C2 **HIGH** — commands.rs:692/1003/1131 — "latest revision" picked by
  lexicographic hash order; with 2+ revisions activate/launch/reveal act on
  a stale revision. Fix: core helper `Store::latest_rev` (max by
  `imported_at`), collapse the three duplicated blocks.
- ☐ C3 **MED** — campaigns_cache is write-only (dead cache); add the
  list_library-style read short-circuit or delete it.
- ☐ C4 **MED** — telemetry.rs — `set_enabled(true)` re-runs `sentry::init`
  on every save_config; leaked client + transport thread each time. Fix:
  early-return when the flag did not change.
- ☐ C5 **MED** — error-path `log_op` missing on launch_package,
  launch_game, launch_battlenet, remove_package, edit_package_metadata,
  migrate_candidate, save_config — failures invisible in support log AND
  Sentry (capture rides on log_op). Fix: a small `fail()` helper.
- ☐ C6 **MED** — commands.rs:283 — `op_id` joined into a filesystem path
  unvalidated (webview-supplied, csp null). Validate charset
  (ascii-alphanumeric + dash) before use.
- ☐ C7 LOW — import_ops never pruned; clear_all_data leaves stale map
  entries pointing at deleted scratch dirs.
- ☐ C8 LOW — tracing spans on only 4 of 27 commands; add `#[tracing::
  instrument(skip_all)]` to remaining mutating/IO commands.
- ☐ C9 LOW — updater: check() errors unlogged (comment claims otherwise);
  success message says "restarting" but the restart comes from NSIS itself.
- ☐ C10 LOW — split commands.rs along its section seams: state.rs
  (AppState/caches/invalidate), logging.rs (log_op/rotation/levels); fold
  the 8 invalidate_library+invalidate_campaigns pairs into one helper.
- ☐ C11 LOW — reveal_package hardcodes core's `deploy/{slot}-{rev}` naming;
  add a Store accessor.
- ☐ C12 LOW — reconcile returns `e.to_string()` after logging `{e:#}`;
  return the full chain to the UI too.

### D. UI

- ☐ D1 **MED** — Settings.tsx:266-273 — after clear_all_data, stale
  logLevel/saveIsolation/savesProfile are re-persisted by the debounced
  auto-save: "Clear all data" resurrects wiped settings. Set all six
  fields from the refetched config.
- ☐ D2 **MED** — Library.tsx — a successful refresh never clears `error`
  and the Alert has no close button: one transient failure pins a permanent
  red banner. Clear error in `.then`.
- ☐ D3 **MED** — Library.tsx:399-410 — sortable headers are click-only divs
  (no keyboard, no aria-sort). Use UnstyledButton inside the th + aria-sort.
- ☐ D4 **MED** — Campaigns.tsx:283-297 — pick-modal rows are clickable
  Group divs without role/tabIndex/keydown; UnstyledButton per row.
- ☐ D5 **MED** — ImportWizard.tsx — on ingest failure the wizard stays on
  step 3 with a no-op Cancel; `setStep(2)` in the catch.
- ☐ D6 LOW — dead `analyzeRef` (ImportWizard.tsx:72,135); delete.
- ☐ D7 LOW — Library faction Select hardcodes the four entries that
  factions.ts exports as `SLOTS`; use `data={SLOTS}`.
- ☐ D8 LOW — ConfigDto and LibraryEntry interfaces duplicated across
  components (ConfigDto already drifted 3 vs 6 fields); consolidate in
  types.ts.
- ☐ D9 LOW — 10 catch sites use `String(e)` instead of `errMessage(e)`;
  future catches on CommandError commands would render "[object Object]".
- ☐ D10 LOW — activate flow copy-pasted three times (Library, Campaigns,
  ImportWizard); extract a `useActivate()` hook.
- ☐ D11 LOW — Log.tsx refresh has no `.catch`; add one.
- ☐ D12 LOW — Campaigns inline `<style>` injects a global rule per mount;
  move to stylesheet.
- ☐ D13 LOW — keepMounted={false} resets Library search/sort on tab
  switch; acceptable if intentional, revisit.
- ☐ D14 LOW — MigrationBanner: sanitized migration id can be empty; guard
  `!id` too.

### E. Docs

- ☐ E1 — research-save-followups.md typos ("Fo" ×2).
- ☑ E2 — this document.
