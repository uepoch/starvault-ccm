# StarVault CCM

StarVault CCM installs, switches, and launches StarCraft II custom campaigns.
It is the successor to SC2CCM and is currently an alpha for Windows 10 or
newer.

[Download the latest alpha](https://github.com/uepoch/starvault-ccm/releases/latest)

## How campaign switching works

StarVault keeps zero or one custom campaign active for the whole game. The
Library is the only campaign-management screen.

- **Activate** deploys a package but does not launch the game.
- **Play** runs preflight, activates the package if needed, then launches the
  game.
- **Return to vanilla** restores the game before another active package can be
  replaced or removed.

Every package ID has one current manifest and one current revision. Reimporting
an inactive package replaces that manifest atomically. Metadata edits do not
change the revision. Reimporting or removing the active package is rejected
until the user returns to vanilla.

Activation, restore, and repair use a persistent operation journal. StarVault
writes the journal before each filesystem swap and keeps staging trees and
backups until the ledger commits and verification succeeds. On startup it
rolls an uncommitted operation back to the previous campaign; a ledger-committed
operation is verified and finalized. If it cannot prove either state, it
preserves the recovery files and blocks further mutations.

StarVault also tracks files it manages in `Mods\`. A matching external file is
borrowed and left in place on restore. A file created by StarVault is removed
only while its hash still matches. Changed managed files stop the operation and
remain available for Repair.

## Import and migration

The importer accepts community zip layouts with packed or loose `.SC2Map` and
`.SC2Mod` containers. It shows detected metadata and faction before ingestion,
enforces archive size and entry limits, and supports cancellation during large
files.

Old SC2CCM campaigns can be imported through the same checked pipeline. The
desktop backend issues opaque migration candidate IDs. The frontend never
sends a source filesystem path back to the migration command.

## Save isolation

Save isolation is an opt-in Beta feature and is off by default. Enabling it
first creates a timestamped recovery backup of the selected profile's `Saves`
and `Banks` directories. Profile selection, isolation changes, and deployment
strategy changes stay locked while a custom campaign is active.

The active package owns `Saves\Campaign`, `Saves\Unsaved`, and non-vanilla
banks. Saves belonging to other vanilla factions stay in the profile root.
OneDrive-managed profiles remain unsupported until their file semantics can be
made safe.

## Safety and privacy

- StarVault refuses filesystem mutations while StarCraft II is running.
- A process-wide mutation lock serializes activation, play, restore, repair,
  import commit, removal, save-related configuration, and clear-all.
- Clear-all restores and verifies vanilla before it closes the store or
  removes application data.
- Telemetry is strictly opt-in. Only panics and internal errors are sent.
  User, package, and environment failures stay local. Reports exclude absolute
  paths, usernames, profile IDs, archive names, and temporary paths.
- Full diagnostic chains stay in the local operation log.

## Alpha reset

The hardened store deliberately does not migrate the previous alpha schema.
Follow the [fresh alpha reset procedure](docs/alpha-reset.md) before installing
the first build that uses the single-campaign schema. Stop if restore or backup
verification fails.

## Current limitations

- Battle.net must be installed and signed in for authenticated campaign play.
- Drag and drop requires the app to run non-elevated. The file picker works in
  either mode.
- Resuming directly into a selected save is not part of the launch contract.

Report broken imports or recovery failures in the
[issue tracker](https://github.com/uepoch/starvault-ccm/issues). Include the
local operation log, but review it before posting because it contains full
local paths.

## Development

The Cargo workspace contains `crates/core`, the Tauri-independent domain, and
`crates/app`, the desktop shell. The React frontend lives in `crates/app/ui`.
Design documents live under [`docs/design`](docs/design).

Install `cargo-audit` and Vite+ before running the complete local gate:

```sh
cargo install cargo-audit --locked --version 0.22.2
scripts/check.sh
```

`scripts/check.sh` runs Rust formatting, clippy, core and desktop-adapter tests, Rust dependency
audit, frontend formatting and type checks, frontend tests, the production
JavaScript dependency audit, and the production web build.

The release helper also runs the full gate and verifies that the requested
version matches the Cargo workspace and `tauri.conf.json` before it dispatches
the signing workflow. See the
[release process](docs/design/release-process.md) for the signing order and the
one documented RustSec exception.

## Naming and license

Official releases are published from this repository. Modified or renamed
builds must identify themselves as unofficial. This project is not affiliated
with or endorsed by Blizzard Entertainment. "StarCraft II" is used only to
identify the supported game.

Licensed under the [MIT License](LICENSE).
