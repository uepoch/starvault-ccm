# StarVault CCM

**Install, switch, and launch StarCraft II custom campaigns — without breaking
your install.**

StarVault CCM is a modern successor to the SC2 Custom Campaign Manager
(SC2CCM). It keeps a local library of campaign packages, deploys them into the
game's campaign slots as reversible transactions, resolves `.SC2Mod`
dependencies without file soup in `Mods\`, isolates saves per campaign, and
launches the game with one click.

[**Download the alpha**](https://github.com/uepoch/starvault-ccm/releases/latest)
· Windows 10 or newer.

## Why another campaign manager?

The original SC2CCM copied files around and hoped for the best. StarVault CCM
treats a campaign switch as a transaction:

- **Nothing is overwritten.** Packages live in a content-addressed store;
  activating a campaign stages, verifies, then swaps. A failed switch rolls
  back; a crash mid-switch is repaired on next launch.
- **Dependencies stay clean.** Shared `.SC2Mod` files are deduplicated across
  campaigns. Two campaigns shipping different bytes for the same `Mods\` path
  block activation with a dialog that names both — instead of one silently
  winning and the other corrupting.
- **Saves don't bleed.** Every campaign gets its own save set (banks
  included); switching a faction swaps its saves in and out. Your vanilla
  progress is never touched.
- **One click to play.** Play resets every faction to plain, activates the
  chosen campaign, and launches the game through Battle.net — signed in, no
  clicks.

## Features

- Import any community campaign zip — file dialog or drag-and-drop. Detected
  title, author, version, and faction are confirmed before anything lands;
  all fields are editable, and metadata stays editable later.
- Library with search, faction filter, and sortable columns.
- Faction cards mirroring the in-game campaign menu (WoL, HotS, LotV, NCO),
  with per-faction activate/replace/restore.
- Pre-flight check before launch: install validity, running instances,
  leftover content — with one-click repair.
- Migration from an existing SC2CCM install: old campaigns import through the
  same checked pipeline, originals untouched.
- Operation log with severity filter (the support artifact), and opt-in crash
  reporting that carries the operation in flight — never personal data.
- Automatic, silent updates: the app refreshes itself in the background and
  restarts when a new release ships.

## Alpha status

This is an alpha: the core flows are tested (on real installs, not just CI),
but the variety of community campaign packages out there will surface edge
cases we haven't met. If an import fails or a campaign misbehaves, please
[open an issue](https://github.com/uepoch/starvault-ccm/issues) with the
operation log (Settings → Log) — that file usually contains the answer.

Known limitations:

- **Battle.net must be installed** (and signed in) for the one-click launch.
  Direct `StarCraft II.exe` launching does not support authenticated
  campaign play — this is a game constraint, not a StarVault one.
- **Resuming straight into a save** is not possible: the game's save tokens
  are private to the game process. StarVault launches the campaign; picking
  the save is the one click you keep.
- **Drag-and-drop needs the app to run non-elevated** (a Windows UI rule).
  The file dialog works either way.

## Support

- [Issue tracker](https://github.com/uepoch/starvault-ccm/issues) for bugs
  and broken imports.
- [Discord](https://discord.com/users/440833687257481227) — come talk.

## Development

Rust workspace (`crates/core` domain, `crates/app` Tauri 2 shell) with a
React + Mantine UI. The design documents and decision record live in
[`docs/`](docs/).

```sh
cargo test -p svccm-core          # domain tests
cargo clippy -p svccm-core --all-targets -- -D warnings
```

## Naming policy

The official releases of StarVault CCM are the ones published from this
repository. Modified or renamed builds must state clearly that they are
unofficial. This project is not affiliated with or endorsed by Blizzard
Entertainment; "StarCraft II" is used descriptively only.

## License

MIT — see [LICENSE](LICENSE).
