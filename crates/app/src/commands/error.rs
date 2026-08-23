use tauri::AppHandle;

use super::{AppState, CommandResult};

fn local_diagnostic(error: &svccm_core::Error) -> String {
    match error.path() {
        Some(path) => format!(
            "{}: {} [path: {}]",
            error.code(),
            error.diagnostic(),
            path.display()
        ),
        None => format!("{}: {}", error.code(), error.diagnostic()),
    }
}

pub(super) fn report(
    app: &AppHandle,
    state: &AppState,
    operation: &str,
    error: svccm_core::Error,
) -> svccm_core::CommandError {
    super::log::log_op(app, "error", operation, &local_diagnostic(&error));
    let report_id = crate::telemetry::capture_internal(&state.telemetry, operation, &error);
    svccm_core::CommandError::from_core(&error, report_id)
}

pub(super) fn map<T>(
    app: &AppHandle,
    state: &AppState,
    operation: &str,
    result: svccm_core::error::Result<T>,
) -> CommandResult<T> {
    result.map_err(|error| report(app, state, operation, error))
}

pub(super) async fn blocking<T, F>(
    app: &AppHandle,
    state: &AppState,
    operation: &'static str,
    task: F,
) -> CommandResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> svccm_core::error::Result<T> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(task).await {
        Ok(result) => map(app, state, operation, result),
        Err(join_error) => Err(report(
            app,
            state,
            operation,
            svccm_core::error::internal_err(
                "blocking_worker_failed",
                "StarVault could not complete the operation",
                join_error.to_string(),
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_diagnostics_retain_actionable_paths() {
        let path = std::path::PathBuf::from("C:/Users/Commander/StarCraft II.exe");
        let error = svccm_core::error::user_path_err(
            "invalid_game_executable",
            "the selected executable does not exist",
            &path,
            false,
        );

        let diagnostic = local_diagnostic(&error);
        assert!(diagnostic.contains("invalid_game_executable"));
        assert!(diagnostic.contains(&path.display().to_string()));
    }
}
