# Game integration

How the app finds, understands, and launches StarCraft II. All constants here
are owned by `core::layout`; this document is their spec.

## Install discovery

In order:

1. Config file: `%APPDATA%\StarVault\CCM\config.toml` → `game.exe_path`.
2. Registry probe (Windows): `HKLM\Software\Classes\Blizzard.SC2Save\
   shell\open\command` — strip `" \"%1\""`, then two path segments
   (`Support\SC2Switcher.exe` in the common case) to reach the install root;
   candidate exe is `<root>\StarCraft II.exe`. Validated by existence.
3. Common locations: `C:\Program Files (x86)\StarCraft II\StarCraft II.exe`.
4. File picker.

The old tool's known limitation stands: multiple SC2 installs resolve to one
path, possibly not the launched one. Mitigation: Settings shows the resolved
path prominently and launch pre-flight verifies it; a per-install override
exists from day one.

Known install layouts are recorded as data in the layout module (slot paths,
plain-campaign contents per slot, prologue special cases), never scattered.

## Non-Windows / broken targets

If the resolved target fails validation (missing exe, non-Windows layout), the
app shows a polite setup screen — never a crash, never a silent wrong-path
mode. Re-picking re-runs validation immediately.

## Launch flow (X1)

```
preflight():
  1. exe exists and is executable
  2. no running instance (named mutex / process scan)
  3. active slots reconcile: junctions resolve, copy trees match manifests
     (spot-check level)
  4. deployed Mods\ paths exist (existence-level check)
spawn:
  detached process: <exe>            # no mutating arguments
  on failure: typed error with OS message
fallback:
  if exe invalid → offer battlenet:// deep link (requires Battle.net app)
```

Launching **never mutates** slots or deployments. If pre-flight finds drift,
it offers repair first; it does not silently fix and launch in one step.

## Plain-campaign state

"Restore to plain" means the slot directory contains what a fresh install
provides for that slot (recorded per-slot in the layout module; HotS includes
`evolution`, LotV accounts for the prologue directory). The installer does not
ship Blizzard files; restore clears custom content and leaves the game's own
resolution to apply. Where the original CCM left slots empty, we do the same —
the game tolerates empty slot directories as "default."

## Version/patch interactions

Battle.net repairs and patches can touch the game directory. Startup
reconciliation treats unexpected foreign files inside slot directories as
drift: reported, not auto-deleted. `Mods\` entries owned by us are verified
against the ledger; unowned entries are ignored (we are a guest in Blizzard's
directory).
