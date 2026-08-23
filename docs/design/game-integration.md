# Game integration

`core::layout` owns install discovery and every StarCraft II path used by the
workflow.

## Install discovery

StarVault checks the configured executable, Windows registry, known install
locations, then the user-selected file. The chosen executable must exist and
its parent must match a supported StarCraft II layout.

The game path is locked while a custom campaign is active. Changing it
requires vanilla state so the ledger cannot refer to one install while files
remain deployed in another.

## Preflight and Play

Preflight checks:

1. no unresolved operation journal exists;
2. the executable and game layout are valid;
3. StarCraft II is not running;
4. the active campaign and managed Mods match the ledger;
5. save ownership matches the active package when isolation is enabled;
6. the requested package manifest is readable and its referenced blob metadata
   is valid.

Play acquires the mutation lock before preflight. It activates the requested
package only when it is not already active, then launches the game while still
holding the lock.

If process launch fails after activation commits, the package remains active
and the command returns `launch_failed_after_activation`. Play never repeats a
save or campaign swap after successful activation.

## Vanilla state

Vanilla means no StarVault package is active, no unchanged created Mod remains,
and isolated saves have the plain owner. Borrowed Mods remain because StarVault
does not own them.

Return to vanilla and clear-all both refuse to run while StarCraft II is
running. Clear-all calls restore first and deletes app data only after restore,
verification, and journal cleanup succeed.

## Save isolation Beta

Save isolation is off by default. Enabling it requires vanilla state, a profile
ID from fresh discovery, and a timestamped recovery backup of both `Saves` and
`Banks`.

The save owner is:

```text
plain | package(package_id)
```

A transition receives the previous and target factions. It archives the
previous faction's root campaign progress and materializes the target
faction's set. `Saves\Campaign`, `Saves\Unsaved`, and non-vanilla banks move
with the active package. Other faction root saves remain untouched.

Profile selection and disabling isolation require vanilla state. Discovery
returns opaque IDs and labels. Commands resolve an ID only against the latest
discovery result.

Documents managed by OneDrive are rejected for isolation. Cross-volume moves
use copy-then-remove. Sharing violations receive bounded retries, and failure
rolls the whole workflow back.

## External changes

Battle.net repair, antivirus, synchronization tools, and users can change game
files. Verification reports unexpected or changed managed paths as drift.
StarVault does not delete an unowned file merely because it appears under a
campaign or Mods directory.
