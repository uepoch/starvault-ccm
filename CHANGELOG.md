# Changelog

## [0.2.5] - 2026-08-25

- Translator jobs can open `starvault://install/translator/<id>` to confirm,
  download, and import a campaign. Reopening the link offers to activate the
  installed package without downloading it again.
- Startup and mutation recovery repair provably StarVault-owned orphaned
  campaign links while preserving their deployments and any saved vanilla tree.
- Clear all data keeps application data intact when the configured game path
  is invalid and directs the user to repair the path before retrying.

## [0.2.4] - 2026-08-25

- Fix startup and shutdown panics caused by Aptabase work running outside
  Tauri's Tokio runtime.

## [0.2.3] - 2026-08-24

- Return to vanilla now discards changed files created by StarVault while
  preserving borrowed external Mods. The separate Repair action is gone.
- Campaign recovery now stores the single campaign slot directly instead of
  carrying a one-element collection through every operation.
- Shared filesystem primitives replace duplicate link, operation-path, and
  Windows file-identity implementations. Dead UI and core dependencies were
  removed in the same simplification pass.

## [0.2.2] - 2026-08-24

- Large loose Mods now activate by renaming complete staged `.SC2Mod`
  containers into place instead of copying thousands of files into the live
  game directory.
- Return to vanilla moves fully owned Mod containers into the operation backup
  and can roll them back by rename. Mixed or externally owned containers keep
  the conservative per-file path.
- A healthy startup verification is reused for later activation, Play, and
  restore operations in the same process. Interrupted recovery still performs
  full verification before changing files.

## [0.2.1] - 2026-08-24

- Release packaging now gives the installer a fixed GitHub-safe filename and
  verifies that `latest.json` names the uploaded file exactly.
- Slightly larger default window; the Imported column no longer collapses
  below its date content.

## [0.2.0] - 2026-08-24

- Every faction now uses one synthetic `Maps\Campaign` tree and one junction.
  Official maps stay in the game archives, and Return to vanilla restores the
  exact loose override tree that existed before activation.
- Activation reuses complete campaign deployments and no longer rereads every
  package, map, and staged Mods file to compute hashes it already has.
- A conflicting external Mods file is reported before target Mods are staged.
  Library can replace it for one activation or remember the choice. Failed
  activations restore the external file from the operation backup.
- The importer accepts mirrored and nested map layouts while preserving useful
  subdirectories beneath the selected faction root.
- Public documentation now describes the single-campaign workflow, Mods
  replacement behavior, performance checks, and current Windows support.

## [0.1.7] — 2026-08-23

- Campaign activation is now global: zero or one custom campaign can be active,
  and Library is the only campaign-management screen. Activate, Play, and
  Return to vanilla are separate actions.
- Activation, restore, and repair now use a durable cross-resource journal.
  Interrupted operations recover to the exact previous or committed state;
  ambiguous recovery preserves its backups and blocks further mutations.
- Packages now have one current manifest and content-derived revision.
  Inactive reimports replace atomically; active packages must be returned to
  vanilla before replacement or removal.
- Managed `Mods\` files distinguish StarVault-created content from identical
  borrowed files. Changed or external content is never silently deleted.
- Save isolation is rebuilt around one global owner, remains opt-in Beta, and
  creates a verified `Saves` and `Banks` recovery backup before enablement or
  profile changes.
- Import now enforces entry, path, per-file, total-size, and free-space limits,
  supports mid-file cancellation, and cleans operation scratch data on every
  terminal path.
- Commands return stable structured errors. Opt-in telemetry sends only panics
  and internal failures with redacted payloads and safe operation/error tags;
  complete diagnostics remain local.
- Clear-all first restores and verifies vanilla, refuses linked/junctioned
  application data, cancels imports, then removes only owned data.
- Release signing is gated on version consistency, formatting, clippy, Rust
  and frontend tests, production builds, and Rust/JavaScript dependency audits.

## [0.1.6] — 2026-08-23

- Fixed imports of flat-layout packages (The Swarm Reborn and friends):
  packed `.SC2Mod` files shipping next to the maps landed in the campaign
  slot instead of the game's `Mods\` folder, so every map failed with a
  missing-mod error. They now deploy where the game looks for them.

## [0.1.5] — 2026-08-23

- Library table fills the window and scrolls itself with a sticky header,
  instead of growing the page.
- Library action buttons share one background; tooltips wait 300ms before
  showing.
- New landing page with download links (uepoch.github.io/starvault-ccm) and a
  rewritten README.

## [0.1.4] — 2026-08-23

- Crash reports (opt-in) now carry the surrounding operation: the archive,
  package, slot, and revision in flight ship with every captured error and
  panic.
- Taller default window so Settings fits without scrolling.
- Internal cleanup: ~170 lines of dead code removed, duplicated logic merged.

## [0.1.3] — 2026-08-23

- Single-instance: launching the app again focuses the existing window.
- Log view: wider Level column; Settings cards fill the window; smaller
  default window size.

## [0.1.2] — 2026-08-22

No app changes: release pipeline now builds with a warm dependency cache.

## [0.1.1] — 2026-08-22

- Automatic updates: the app checks for a new release at startup and
  installs it in the background; it takes effect on next launch.
- Metadata editor: edit a package's title, author, version, and description
  from the Library.
- Import view: editable version field; Title above Version.
- Action buttons unified to the background-less style (Play keeps emphasis).
- Fixed shared-mods deployment failing with "os error 183" when a leftover
  packed mod file blocked an unpacked one.

## [0.1.0] — 2026-08-22

First public pre-alpha. Windows 10+.

- Import any community campaign zip — via file dialog or by dropping it
  anywhere on the window. Detected title, author, and faction are shown for
  confirmation; both are editable.
- Library table with search (title/author/description), faction filter, and
  sortable columns. Actions: Activate (icon, green when active), open in
  Explorer, remove (with disk reclaim).
- One-click **Play**: resets every faction to plain, activates the chosen
  campaign on its faction, and launches the game — a clean `Mods\` union
  every time.
- Faction cards mirror the in-game campaign menu. Pre-flight check verifies
  the install, running instances, and deployed content; repairs crash
  leftovers with one click.
- Campaign switches are transactions: stage, verify, swap, roll back on
  failure. Dedicated factions use NTFS junctions (instant swaps) with an
  automatic copy fallback; startup reconciliation repairs interrupted
  switches.
- Cross-campaign dependency conflicts block activation with a dialog naming
  both campaigns, the clashing file, and an option to disable the other
  campaign and proceed.
- Old SC2CCM installs are detected; each old campaign imports through the
  same checked pipeline, originals untouched.
- Operation log with severity levels, rotation, and a per-level filter.
  Opt-in error reporting (Settings) sends crashes and failures only.
- Game install is auto-detected (registry or well-known folders); settings
  save automatically.
- Launch pre-flight, detached game launch, and a Battle.net fallback.
