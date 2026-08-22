//! StarVault CCM domain core.
//!
//! Pure Rust, zero shell dependencies. Everything in this crate is testable
//! against temporary directory trees without a display server.
//!
//! See `docs/design/architecture.md` for the structural rules this crate
//! enforces.

pub mod config;
pub mod error;
pub mod launch;
pub mod layout;
pub mod library;
pub mod mpq;
pub mod package;
pub mod saves;
pub mod slots;
pub mod store;

pub use error::{Error, PackageError, UserError};
