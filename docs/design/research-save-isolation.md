# Save isolation research notes

This file records the findings that shaped the current Beta design. The
normative contract is in [`game-integration.md`](game-integration.md).

## Profile discovery

StarCraft II stores local data below:

```text
Documents\StarCraft II\Accounts\<account>\<profile>\
```

The numeric account and regional profile IDs are filesystem identifiers. No
reliable offline registry value maps the signed-in Battle.net identity to one
profile. StarVault therefore enumerates valid profiles and returns opaque IDs
with display labels. A command must resolve the chosen ID against a fresh
enumeration.

## Save scope

Faction progress files live directly under `Saves`. In-mission saves live in
`Saves\Campaign`, autosaves in `Saves\Unsaved`, and custom campaign progress
may also live in `Banks`.

Swapping the complete `Saves` directory would hide unrelated vanilla faction
progress. The current design instead moves the previous and target faction's
root progress plus `Campaign`, `Unsaved`, and non-vanilla banks. Other faction
root saves remain live.

## OneDrive and cross-volume behavior

OneDrive Known Folder Move can leave placeholders, synchronization locks, and
cloud reconciliation races in Documents. StarVault rejects save isolation for
a OneDrive-managed profile. It does not offer a junction fallback there.

Documents may live on a different volume from app data. Rename reports
`ErrorKind::CrossesDevices` in that case. The save workflow copies to staging,
verifies the copy, then removes the source. It uses bounded retries for sharing
violations and rolls back after the retry limit.

## Recovery backup

Enabling isolation is the first point at which StarVault takes ownership of
save transitions. It requires vanilla state and creates a timestamped copy of
both `Saves` and `Banks` outside the working staging paths. Failure to copy or
verify either directory leaves isolation disabled.

## Remaining acceptance questions

- Verify save classification against packages that write several bank author
  directories.
- Verify Battle.net cloud reconciliation after offline and online sessions.
- Record the sharing-violation retry timing on antivirus and OneDrive-enabled
  Windows machines, even though OneDrive profiles remain rejected.
