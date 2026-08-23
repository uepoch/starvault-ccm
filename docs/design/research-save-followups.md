# Save isolation follow-up results

These findings are folded into the Beta contract:

- Directory enumeration is the only dependable offline profile-discovery
  method. Login email and Battle.net configuration do not identify the local
  numeric profile safely.
- Multiple profiles are normal after region changes or shared-PC use. The app
  must ask instead of selecting by path order or recent modification time.
- Campaign progress, in-mission saves, autosaves, and banks use different
  locations. A safe transition needs both the previous and target faction.
- Large `Unsaved` trees make mid-file cancellation and free-space checks
  necessary.
- MPQ parsing is not required for isolation. Ownership follows the active
  package and known save locations, not the internal contents of a save file.
- Direct resume through game arguments remains outside the Play contract.

See [`research-save-isolation.md`](research-save-isolation.md) for the evidence
summary and [`game-integration.md`](game-integration.md) for required behavior.
