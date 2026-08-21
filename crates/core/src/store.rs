//! Content-addressed blob store and deployment ledger.
//!
//! Layout and algorithms per `docs/design/dependency-store.md`.
//! Implementation lands in M1.

/// Root of the app's private storage:
/// `%APPDATA%\StarVault\CCM\store` on Windows.
pub struct Store {
    _root: std::path::PathBuf,
}
