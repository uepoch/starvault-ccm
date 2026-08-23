use svccm_core::operation::PendingOperation;
use svccm_core::PackageId;
use tauri::AppHandle;

use super::{AppState, CommandResult};

fn run_package_mutation_after_game_guard<T>(
    ensure_stopped: impl FnOnce() -> svccm_core::error::Result<()>,
    mutate: impl FnOnce() -> svccm_core::error::Result<T>,
) -> svccm_core::error::Result<T> {
    ensure_stopped()?;
    mutate()
}

#[tauri::command]
pub async fn reveal_package(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> CommandResult<String> {
    let package_id = super::error::map(&app, &state, "reveal_package", PackageId::parse(id))?;
    let _mutation = state.mutation.lock().await;
    let store = super::error::map(&app, &state, "reveal_package", state.store())?;
    let manifest = super::error::map(
        &app,
        &state,
        "reveal_package",
        store.load_manifest(&package_id),
    )?;
    let deployed = super::error::map(
        &app,
        &state,
        "reveal_package",
        store.deploy_dir(manifest.faction, &manifest.revision),
    )?;
    let target = if deployed.is_dir() {
        deployed
    } else {
        store.root().join("packages").join(package_id.as_str())
    };
    open_directory(&target)
        .map_err(|error| super::error::report(&app, &state, "reveal_package", error))?;
    Ok(target.display().to_string())
}

fn open_directory(path: &std::path::Path) -> svccm_core::error::Result<()> {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    result.map(|_| ()).map_err(|error| {
        svccm_core::error::user_path_err("open_package_directory", error.to_string(), path, true)
    })
}

#[tauri::command]
pub async fn remove_package(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> CommandResult<()> {
    let package_id = super::error::map(&app, &state, "remove_package", PackageId::parse(id))?;
    let log_id = package_id.to_string();
    let _mutation = state.mutation.lock().await;
    let store = super::error::map(&app, &state, "remove_package", state.store())?;
    super::error::blocking(&app, &state, "remove_package", move || {
        run_package_mutation_after_game_guard(super::ensure_game_stopped, || {
            if PendingOperation::load(store.root())?.is_some() {
                return Err(svccm_core::error::package_err(
                    "recovery_required",
                    "recover the interrupted campaign operation before removing packages",
                ));
            }
            store.remove_package(&package_id)
        })
    })
    .await?;
    super::log::log_op(&app, "info", "remove", &log_id);
    Ok(())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn edit_package_metadata(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    title: String,
    author: String,
    version: String,
    desc: String,
) -> CommandResult<()> {
    let package_id =
        super::error::map(&app, &state, "edit_package_metadata", PackageId::parse(id))?;
    let log_id = package_id.to_string();
    let _mutation = state.mutation.lock().await;
    let store = super::error::map(&app, &state, "edit_package_metadata", state.store())?;
    super::error::blocking(&app, &state, "edit_package_metadata", move || {
        run_package_mutation_after_game_guard(super::ensure_game_stopped, || {
            if PendingOperation::load(store.root())?.is_some() {
                return Err(svccm_core::error::package_err(
                    "recovery_required",
                    "recover the interrupted campaign operation before editing packages",
                ));
            }
            store.set_metadata(&package_id, &title, &author, &version, &desc)
        })
    })
    .await?;
    super::log::log_op(&app, "info", "metadata", &format!("updated {log_id}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_game_rejection_prevents_package_mutation() {
        let mutated = std::cell::Cell::new(false);

        let error = run_package_mutation_after_game_guard(
            || Err(svccm_core::error::EnvironmentError::GameRunning.into()),
            || {
                mutated.set(true);
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code(), "game_running");
        assert!(!mutated.get());
    }

    #[test]
    fn game_guard_runs_before_package_mutation() {
        let guard_passed = std::cell::Cell::new(false);

        run_package_mutation_after_game_guard(
            || {
                guard_passed.set(true);
                Ok(())
            },
            || {
                assert!(guard_passed.get());
                Ok(())
            },
        )
        .unwrap();
    }
}
