use std::path::Path;

use svccm_core::contracts::{HealthState, LibrarySnapshot, StartupReport};
use svccm_core::operation::PendingOperation;
use svccm_core::PackageId;
use tauri::{AppHandle, Manager};

use super::{AppState, CommandResult, WorkflowContext};

#[tauri::command]
pub async fn initialize(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<StartupReport> {
    let _mutation = state.mutation.lock().await;
    let context = super::error::map(
        &app,
        &state,
        "initialize",
        WorkflowContext::readable(&app, &state),
    )?;
    super::error::blocking(&app, &state, "initialize", move || {
        context.workflow().initialize()
    })
    .await
}

#[tauri::command]
pub async fn list_library(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<LibrarySnapshot> {
    let _mutation = state.mutation.lock().await;
    let context = super::error::map(
        &app,
        &state,
        "list_library",
        WorkflowContext::readable(&app, &state),
    )?;
    super::error::blocking(&app, &state, "list_library", move || {
        context.workflow().library_snapshot()
    })
    .await
}

#[tauri::command]
pub async fn activate_package(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> CommandResult<()> {
    let package_id = super::error::map(&app, &state, "activate_package", PackageId::parse(id))?;
    let _mutation = state.mutation.lock().await;
    let context = super::error::map(
        &app,
        &state,
        "activate_package",
        WorkflowContext::configured(&app, &state),
    )?;
    let active = super::error::blocking(&app, &state, "activate_package", move || {
        context.workflow().activate(&package_id)
    })
    .await?;
    super::log::log_op(
        &app,
        "info",
        "activate",
        &format!("{}@{}", active.id, short(&active.revision)),
    );
    Ok(())
}

#[tauri::command]
pub async fn play_package(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> CommandResult<()> {
    let package_id = super::error::map(&app, &state, "play_package", PackageId::parse(id))?;
    let _mutation = state.mutation.lock().await;
    let context = super::error::map(
        &app,
        &state,
        "play_package",
        WorkflowContext::configured(&app, &state),
    )?;
    let active = super::error::blocking(&app, &state, "play_package", move || {
        context.workflow().play(&package_id)
    })
    .await?;
    super::log::log_op(
        &app,
        "info",
        "play",
        &format!("{}@{}", active.id, short(&active.revision)),
    );
    Ok(())
}

#[tauri::command]
pub async fn restore_vanilla(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<()> {
    let _mutation = state.mutation.lock().await;
    let context = super::error::map(
        &app,
        &state,
        "restore_vanilla",
        WorkflowContext::configured(&app, &state),
    )?;
    super::error::blocking(&app, &state, "restore_vanilla", move || {
        context.workflow().restore_vanilla()
    })
    .await?;
    super::log::log_op(&app, "info", "restore", "returned to vanilla");
    Ok(())
}

#[tauri::command]
pub async fn repair_active(app: AppHandle, state: tauri::State<'_, AppState>) -> CommandResult<()> {
    let _mutation = state.mutation.lock().await;
    let context = super::error::map(
        &app,
        &state,
        "repair_active",
        WorkflowContext::configured(&app, &state),
    )?;
    super::error::blocking(&app, &state, "repair_active", move || {
        context.workflow().repair_active()
    })
    .await?;
    super::log::log_op(&app, "info", "repair", "repaired active campaign");
    Ok(())
}

#[tauri::command]
pub async fn clear_all_data(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<()> {
    let _mutation = state.mutation.lock().await;
    super::error::map(
        &app,
        &state,
        "clear_all_data",
        super::imports::cancel_all(&state).await,
    )?;

    let context = super::error::map(
        &app,
        &state,
        "clear_all_data",
        WorkflowContext::configured(&app, &state),
    )?;
    super::error::blocking(&app, &state, "clear_all_data", move || {
        let workflow = context.workflow();
        restore_and_verify_for_clear(&workflow, context.store.root())
    })
    .await?;

    let resolved_data = app.path().app_data_dir().map_err(|error| {
        super::error::report(
            &app,
            &state,
            "clear_all_data",
            svccm_core::error::internal_err(
                "resolve_app_data",
                "StarVault could not resolve its data directory",
                error.to_string(),
            ),
        )
    })?;
    if resolved_data != state.app_data_path {
        return Err(super::error::report(
            &app,
            &state,
            "clear_all_data",
            svccm_core::error::internal_err(
                "app_data_path_changed",
                "StarVault refused to clear an unexpected directory",
                format!(
                    "managed path `{}` differs from resolver path `{}`",
                    state.app_data_path.display(),
                    resolved_data.display()
                ),
            ),
        ));
    }
    super::error::map(
        &app,
        &state,
        "clear_all_data",
        validate_clear_target(&state.app_data_path),
    )?;
    if state.import_root.symlink_metadata().is_ok() {
        super::error::map(
            &app,
            &state,
            "clear_all_data",
            validate_clear_target(&state.import_root),
        )?;
    }

    let import_root = state.import_root.clone();
    super::error::blocking(&app, &state, "clear_all_data", move || {
        remove_owned_root_if_exists(&import_root)
    })
    .await?;
    super::log::log_op(&app, "info", "clear", "removing owned application data");
    state.telemetry.set_enabled(false);
    super::error::map(&app, &state, "clear_all_data", state.close_store())?;
    let app_data = state.app_data_path.clone();
    super::error::blocking(&app, &state, "clear_all_data", move || {
        super::log::with_log_io_lock(|| remove_owned_root_if_exists(&app_data))
    })
    .await
}

fn restore_and_verify_for_clear(
    workflow: &svccm_core::workflow::Workflow<'_>,
    store_root: &Path,
) -> svccm_core::error::Result<()> {
    workflow.restore_vanilla()?;
    let health = workflow.preflight(None)?;
    if health.state != HealthState::Ready {
        return Err(svccm_core::error::package_err(
            "vanilla_verification_failed",
            "StarVault could not verify the vanilla game state",
        ));
    }
    if PendingOperation::load(store_root)?.is_some() {
        return Err(svccm_core::error::package_err(
            "recovery_required",
            "an operation journal remains after restoration",
        ));
    }
    Ok(())
}

fn validate_clear_target(path: &Path) -> svccm_core::error::Result<()> {
    let parent = path.parent();
    if !path.is_absolute()
        || path.file_name().is_none()
        || parent.is_none()
        || parent.is_some_and(|parent| parent.parent().is_none())
    {
        return Err(svccm_core::error::internal_err(
            "unsafe_clear_target",
            "StarVault refused to clear a broad directory",
            path.display().to_string(),
        ));
    }
    super::validate_regular_directory(path, "unsafe_clear_target")
}

fn remove_owned_root_if_exists(path: &Path) -> svccm_core::error::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(svccm_core::error::user_path_err(
                "inspect_clear_target",
                error.to_string(),
                path,
                true,
            ));
        }
        Ok(_) => validate_clear_target(path)?,
    }
    validate_owned_tree(path)?;
    let mut last_error = None;
    for attempt in 0..4 {
        validate_owned_tree(path)?;
        match std::fs::remove_dir_all(path) {
            Ok(()) => break,
            Err(error) => {
                let retryable = matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                );
                if !retryable || attempt == 3 {
                    return Err(svccm_core::error::user_path_err(
                        "clear_owned_data",
                        error.to_string(),
                        path,
                        retryable,
                    ));
                }
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
            }
        }
    }
    if path.symlink_metadata().is_ok() {
        return Err(svccm_core::error::internal_err(
            "clear_verification_failed",
            "StarVault could not verify that its data was removed",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| path.display().to_string()),
        ));
    }
    Ok(())
}

fn validate_owned_tree(root: &Path) -> svccm_core::error::Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|error| {
            svccm_core::error::user_path_err(
                "inspect_clear_target",
                error.to_string(),
                &directory,
                true,
            )
        })? {
            let entry = entry.map_err(|error| {
                svccm_core::error::user_path_err(
                    "inspect_clear_target",
                    error.to_string(),
                    &directory,
                    true,
                )
            })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                svccm_core::error::user_path_err(
                    "inspect_clear_target",
                    error.to_string(),
                    &path,
                    true,
                )
            })?;
            if metadata.file_type().is_symlink() || super::is_reparse_point(&metadata) {
                return Err(svccm_core::error::user_path_err(
                    "unsafe_clear_target",
                    "refusing to clear application data containing a link or junction",
                    &path,
                    false,
                ));
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.is_file() {
                return Err(svccm_core::error::user_path_err(
                    "unsafe_clear_target",
                    "refusing to clear application data containing a special filesystem entry",
                    &path,
                    false,
                ));
            }
        }
    }
    Ok(())
}

fn short(revision: &str) -> &str {
    &revision[..revision.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::{remove_owned_root_if_exists, restore_and_verify_for_clear, validate_clear_target};

    #[test]
    fn clear_preflight_restores_and_verifies_vanilla_before_deletion() {
        use svccm_core::config::StrategyChoice;
        use svccm_core::identity::PackageId;
        use svccm_core::layout::{SlotId, WindowsLayout};
        use svccm_core::package::normalize::plan_from_extracted;
        use svccm_core::store::Store;
        use svccm_core::workflow::Workflow;

        let temporary = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temporary.path().join("game"));
        std::fs::create_dir_all(layout.root()).unwrap();
        std::fs::write(layout.exe(), b"test executable").unwrap();
        let store = Store::open_for_tests(temporary.path().join("store")).unwrap();
        let source = temporary.path().join("source");
        let map = source.join("Maps/campaign/clear-test.SC2Map");
        std::fs::create_dir_all(&map).unwrap();
        std::fs::write(map.join("payload"), b"campaign").unwrap();
        std::fs::create_dir_all(source.join("Mods")).unwrap();
        std::fs::write(source.join("Mods/clear-test.SC2Mod"), b"mod").unwrap();
        let id = PackageId::parse("clear-test").unwrap();
        store
            .ingest(&id, SlotId::LotV, &plan_from_extracted(&source).unwrap())
            .unwrap();
        let workflow = Workflow::new(&layout, &store)
            .with_strategy(Some(StrategyChoice::Copy))
            .with_running_probe(|| false);
        workflow.activate(&id).unwrap();

        restore_and_verify_for_clear(&workflow, store.root()).unwrap();

        assert!(store.active_campaign().unwrap().is_none());
        assert!(store.managed_mods().unwrap().is_empty());
        assert!(svccm_core::operation::PendingOperation::load(store.root())
            .unwrap()
            .is_none());
        assert!(!layout.mods_dir().join("clear-test.SC2Mod").exists());
    }

    #[test]
    fn broad_clear_targets_are_rejected() {
        let filesystem_root = std::path::Path::new(std::path::MAIN_SEPARATOR_STR);
        assert!(validate_clear_target(filesystem_root).is_err());
        assert!(validate_clear_target(std::path::Path::new("relative/path")).is_err());

        let temporary_root = std::env::temp_dir();
        if temporary_root
            .parent()
            .is_some_and(|parent| parent.parent().is_none())
        {
            assert!(validate_clear_target(&temporary_root).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn clear_rejects_a_symlink_root_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let linked = temporary.path().join("owned-looking-link");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        symlink(&external, &linked).unwrap();

        assert_eq!(
            remove_owned_root_if_exists(&linked).unwrap_err().code(),
            "unsafe_clear_target"
        );
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert!(linked.symlink_metadata().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn clear_rejects_a_nested_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let owned = temporary.path().join("owned-data");
        std::fs::create_dir(&external).unwrap();
        std::fs::create_dir(&owned).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        symlink(&external, owned.join("nested-link")).unwrap();

        assert_eq!(
            remove_owned_root_if_exists(&owned).unwrap_err().code(),
            "unsafe_clear_target"
        );
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert!(owned.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn clear_rejects_a_junction_root_without_touching_its_target() {
        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let linked = temporary.path().join("owned-looking-junction");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        junction::create(&external, &linked).unwrap();

        assert_eq!(
            remove_owned_root_if_exists(&linked).unwrap_err().code(),
            "unsafe_clear_target"
        );
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert!(linked.symlink_metadata().is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn clear_rejects_a_nested_junction_without_touching_its_target() {
        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let owned = temporary.path().join("owned-data");
        std::fs::create_dir(&external).unwrap();
        std::fs::create_dir(&owned).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        junction::create(&external, owned.join("nested-junction")).unwrap();

        assert_eq!(
            remove_owned_root_if_exists(&owned).unwrap_err().code(),
            "unsafe_clear_target"
        );
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert!(owned.is_dir());
    }
}
