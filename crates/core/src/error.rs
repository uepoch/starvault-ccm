//! Typed error taxonomy for the whole core.
//!
//! The UI maps variants to human sentences; only [`Error::Internal`] reaches
//! crash reporting. See `docs/design/architecture.md`.

use thiserror::Error;

/// Something the user can fix: locked files, disk full, wrong exe picked.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct UserError {
    pub message: String,
    /// Path the user can act on, when there is one.
    pub path: Option<std::path::PathBuf>,
}

/// The package (zip/containers/manifests) is malformed or unsatisfiable.
/// Always carries the offending path so import failures point at the exact
/// file inside the archive.
#[derive(Debug, Error)]
#[error("{context}: {detail}")]
pub struct PackageError {
    /// Where in the package this happened, e.g. `Mods/RaynorRogue.SC2Mod`.
    pub context: String,
    pub detail: String,
}

/// The environment cannot support the requested operation.
#[derive(Debug, Error)]
pub enum EnvironmentError {
    #[error("StarCraft II installation not found or invalid")]
    GameNotFound,
    #[error("operation requires Windows")]
    UnsupportedPlatform,
    #[error("volume does not support junctions and copy fallback is disabled")]
    JunctionsUnsupported,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    User(#[from] UserError),
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(transparent)]
    Environment(#[from] EnvironmentError),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::User(UserError {
            message: e.to_string(),
            path: None,
        })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Convenience constructor for package errors.
pub fn pkg_err(context: impl Into<String>, detail: impl Into<String>) -> Error {
    Error::Package(PackageError {
        context: context.into(),
        detail: detail.into(),
    })
}
