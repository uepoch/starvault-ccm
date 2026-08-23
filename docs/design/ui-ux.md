# UI and UX

The frontend presents core state. It does not plan deployment or merge package
state on its own.

## Navigation

The app has Library, Log, and Settings tabs. There is no Campaigns tab.

Library shows the global active state above the package table. An active
summary includes faction, revision, Play, and Return to vanilla. An inactive
row has separate Activate and Play actions. The active row disables Activate
and keeps Play available.

Removing or reimporting the active package explains that the user must return
to vanilla first.

## Health and repair

Library renders `Health.state` as ready, drifted, or recovery required. It
lists each issue returned by the core. Recovery-required state disables normal
mutations.

For health issues, the frontend offers Repair only when the backend sets the
issue's `repairable` flag. For command failures, where the wire type has no
repair flag, a small stable error-code allowlist controls the same affordance.
Retryability alone never turns an unrelated failure into a Repair prompt.

## Import

The import reducer mirrors the backend states:

```text
Analyzing | Ready | Ingesting | Cancelled | Failed | Completed
```

Closing the wizard cancels any nonterminal operation and lets backend cleanup
finish. A failed operation can be retried after the backend has released its
scratch state. Reimporting the active package is blocked with a Return to
vanilla explanation.

## Migration

Migration discovery returns opaque candidate IDs and display labels. The
frontend sends the candidate ID, destination package ID, and faction. It never
sends a source path selected or copied from page state.

## Settings

While a campaign is active, Settings disables:

- game executable path and discovery;
- deployment strategy;
- save isolation;
- save profile selection.

Save isolation is labeled Beta. Profiles use opaque IDs and human-readable
labels returned by discovery.

## Errors

The frontend receives `CommandError`, shows its stable message, and may show a
safe path supplied for user action. It never renders a raw diagnostic chain.
Internal errors may include a report ID. Full diagnostics remain in the Log
tab.

## Accessibility and visual language

The UI keeps the existing dark Mantine theme and faction colors. The active
campaign summary is the page's visual anchor.

Sortable table headers contain keyboard-operable buttons. The header cell
updates `aria-sort` to `none`, `ascending`, or `descending`. Icon-only actions
have package-specific accessible labels.
