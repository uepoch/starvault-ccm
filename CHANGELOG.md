# Changelog

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
