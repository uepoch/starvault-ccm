//! Stable error taxonomy shared by the core and Tauri boundary.
//!
//! Display text is safe for the local UI. Callers keep the full debug chain
//! in the local operation log and serialize only [`CommandError`].

use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

/// The four public failure classes. Telemetry may report only `Internal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    User,
    Package,
    Environment,
    Internal,
}

/// Something the user can fix, such as a locked file or invalid request.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct UserError {
    pub code: String,
    pub message: String,
    /// A local path the user can act on, when one is useful.
    pub path: Option<PathBuf>,
    pub retryable: bool,
}

/// The imported or installed package is malformed or cannot be satisfied.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct PackageError {
    pub code: String,
    pub message: String,
    /// Canonical package-relative context, never an archive source path.
    pub context: Option<String>,
    pub path: Option<PathBuf>,
    pub retryable: bool,
}

/// The host cannot support the operation in its current state.
#[derive(Debug, Error)]
pub enum EnvironmentError {
    #[error("StarCraft II installation not found or invalid")]
    GameNotFound,
    #[error("operation requires Windows")]
    UnsupportedPlatform,
    #[error("volume does not support directory junctions")]
    JunctionsUnsupported,
    #[error("StarCraft II is running; close it and retry")]
    GameRunning,
    #[error("not enough free space for this operation")]
    InsufficientSpace { path: Option<PathBuf> },
    #[error("Documents is managed by OneDrive; save isolation is unavailable")]
    OneDriveUnsupported,
    #[error("StarCraft II could not be launched")]
    LaunchFailed { detail: String },
    #[error("the campaign was activated, but StarCraft II could not be launched")]
    LaunchFailedAfterActivation { detail: String },
}

impl EnvironmentError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::GameNotFound => "game_not_found",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::JunctionsUnsupported => "junctions_unsupported",
            Self::GameRunning => "game_running",
            Self::InsufficientSpace { .. } => "insufficient_space",
            Self::OneDriveUnsupported => "onedrive_unsupported",
            Self::LaunchFailed { .. } => "launch_failed",
            Self::LaunchFailedAfterActivation { .. } => "launch_failed_after_activation",
        }
    }

    fn path(&self) -> Option<&std::path::Path> {
        match self {
            Self::InsufficientSpace { path } => path.as_deref(),
            _ => None,
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::GameRunning
                | Self::InsufficientSpace { .. }
                | Self::LaunchFailed { .. }
                | Self::LaunchFailedAfterActivation { .. }
        )
    }
}

/// An invariant violation or unexpected implementation failure.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct InternalError {
    pub code: String,
    /// Deliberately generic public text. Diagnostic detail belongs in logs.
    pub message: String,
    pub detail: Option<String>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    User(#[from] UserError),
    #[error(transparent)]
    Package(#[from] PackageError),
    #[error(transparent)]
    Environment(#[from] EnvironmentError),
    #[error(transparent)]
    Internal(#[from] InternalError),
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::User(_) => ErrorKind::User,
            Self::Package(_) => ErrorKind::Package,
            Self::Environment(_) => ErrorKind::Environment,
            Self::Internal(_) => ErrorKind::Internal,
        }
    }

    pub fn code(&self) -> &str {
        match self {
            Self::User(error) => &error.code,
            Self::Package(error) => &error.code,
            Self::Environment(error) => error.code(),
            Self::Internal(error) => &error.code,
        }
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            Self::User(error) => error.path.as_deref(),
            Self::Package(error) => error.path.as_deref(),
            Self::Environment(error) => error.path(),
            Self::Internal(_) => None,
        }
    }

    pub fn retryable(&self) -> bool {
        match self {
            Self::User(error) => error.retryable,
            Self::Package(error) => error.retryable,
            Self::Environment(error) => error.retryable(),
            Self::Internal(_) => false,
        }
    }

    /// Full local-only detail, including internal context.
    pub fn diagnostic(&self) -> String {
        match self {
            Self::Internal(error) => error
                .detail
                .as_ref()
                .map(|detail| format!("{}: {detail}", error.message))
                .unwrap_or_else(|| error.message.clone()),
            Self::Package(error) => error
                .context
                .as_ref()
                .map(|context| format!("{context}: {}", error.message))
                .unwrap_or_else(|| error.message.clone()),
            Self::Environment(
                EnvironmentError::LaunchFailed { detail }
                | EnvironmentError::LaunchFailedAfterActivation { detail },
            ) => format!("{}: {detail}", self),
            _ => self.to_string(),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::User(UserError {
            code: "filesystem_error".into(),
            message: error.to_string(),
            path: None,
            retryable: matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::Interrupted
            ),
        })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn user_err(code: impl Into<String>, message: impl Into<String>) -> Error {
    Error::User(UserError {
        code: code.into(),
        message: message.into(),
        path: None,
        retryable: false,
    })
}

pub fn user_path_err(
    code: impl Into<String>,
    message: impl Into<String>,
    path: impl Into<PathBuf>,
    retryable: bool,
) -> Error {
    Error::User(UserError {
        code: code.into(),
        message: message.into(),
        path: Some(path.into()),
        retryable,
    })
}

/// Convenience constructor for package errors.
pub fn pkg_err(context: impl Into<String>, detail: impl Into<String>) -> Error {
    Error::Package(PackageError {
        code: "invalid_package".into(),
        message: detail.into(),
        context: Some(context.into()),
        path: None,
        retryable: false,
    })
}

pub fn package_err(code: impl Into<String>, message: impl Into<String>) -> Error {
    Error::Package(PackageError {
        code: code.into(),
        message: message.into(),
        context: None,
        path: None,
        retryable: false,
    })
}

pub fn internal_err(
    code: impl Into<String>,
    message: impl Into<String>,
    detail: impl Into<String>,
) -> Error {
    Error::Internal(InternalError {
        code: code.into(),
        message: message.into(),
        detail: Some(detail.into()),
    })
}

/// Stable IPC error. It never contains a Rust error chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandError {
    pub kind: ErrorKind,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_id: Option<String>,
}

impl CommandError {
    pub fn from_core(error: &Error, report_id: Option<String>) -> Self {
        Self {
            kind: error.kind(),
            code: error.code().to_string(),
            message: error.to_string(),
            path: error.path().map(|path| path.display().to_string()),
            retryable: error.retryable(),
            report_id,
        }
    }
}

impl From<Error> for CommandError {
    fn from(error: Error) -> Self {
        Self::from_core(&error, None)
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_error_drops_internal_detail() {
        let error = internal_err(
            "ledger_invariant",
            "StarVault could not complete the operation",
            "sqlite at C:/Users/Alice/private/store/ledger.db returned 11",
        );
        let dto = CommandError::from_core(&error, Some("report-1".into()));
        assert_eq!(dto.kind, ErrorKind::Internal);
        assert_eq!(dto.code, "ledger_invariant");
        assert!(!dto.message.contains("Alice"));
        assert_eq!(dto.report_id.as_deref(), Some("report-1"));
        assert!(error.diagnostic().contains("Alice"));
    }
}
