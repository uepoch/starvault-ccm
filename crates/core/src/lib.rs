//! StarVault CCM domain core.
//!
//! Pure Rust, zero shell dependencies. Everything in this crate is testable
//! against temporary directory trees without a display server.
//!
//! See `docs/design/architecture.md` for the structural rules this crate
//! enforces.

pub mod atomic_file;
pub mod config;
pub mod contracts;
pub mod error;
pub mod filesystem;
pub mod identity;
pub mod launch;
pub mod layout;
pub mod library;
pub mod mods;
pub mod mpq;
pub mod operation;
pub mod package;
pub mod saves;
pub mod slots;
pub mod store;
pub mod workflow;

pub use error::{CommandError, Error, ErrorKind, InternalError, PackageError, UserError};
pub use identity::{PackageId, ProfileId};
