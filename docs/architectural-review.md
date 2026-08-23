# Archived architectural review

The original review described SC2CCM and the first StarVault alpha. Its useful
findings are now requirements in the current design documents:

- path construction belongs in one layout module;
- game-file changes need staging, verification, rollback, and recovery;
- the domain core must remain independent of Tauri;
- the frontend receives stable errors instead of diagnostic chains;
- long imports need progress and cancellation.

See [`design/architecture.md`](design/architecture.md) and
[`design/slot-manager.md`](design/slot-manager.md) for the current system.
