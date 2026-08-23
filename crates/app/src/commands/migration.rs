use svccm_core::layout::SlotId;
use svccm_core::library::{LegacyCcmInstall, MigrationCandidate};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::PackageId;
use tauri::AppHandle;

use super::{AppState, CommandResult};

fn legacy_install_present(state: &AppState) -> bool {
    state
        .app_data_path
        .parent()
        .and_then(LegacyCcmInstall::detect)
        .is_some()
}

#[tauri::command]
pub async fn list_migration_candidates(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<Vec<MigrationCandidate>> {
    let _mutation = state.mutation.lock().await;
    if !legacy_install_present(&state) {
        return Ok(Vec::new());
    }
    let config = super::error::map(
        &app,
        &state,
        "list_migration_candidates",
        super::load_config(&state),
    )?;
    if config.game_exe.is_none() {
        return Ok(Vec::new());
    }
    let layout = super::error::map(
        &app,
        &state,
        "list_migration_candidates",
        super::configured_layout(&config),
    )?;
    super::error::blocking(&app, &state, "list_migration_candidates", move || {
        layout.validate_mutation_roots()?;
        Ok(svccm_core::library::migration_candidates(&layout))
    })
    .await
}

fn validate_candidate_id(candidate_id: &str) -> svccm_core::error::Result<()> {
    if candidate_id.len() == 64
        && candidate_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(svccm_core::error::user_err(
            "invalid_migration_candidate_id",
            "migration candidate id is not a backend-issued token",
        ))
    }
}

fn run_migration_after_game_guard<T>(
    ensure_stopped: impl FnOnce() -> svccm_core::error::Result<()>,
    migrate: impl FnOnce() -> svccm_core::error::Result<T>,
) -> svccm_core::error::Result<T> {
    ensure_stopped()?;
    migrate()
}

#[tauri::command]
pub async fn migrate_candidate(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    candidate_id: String,
    id: String,
    faction: String,
) -> CommandResult<String> {
    super::error::map(
        &app,
        &state,
        "migrate_candidate",
        validate_candidate_id(&candidate_id),
    )?;
    let package_id = super::error::map(&app, &state, "migrate_candidate", PackageId::parse(id))?;
    let faction = super::error::map(&app, &state, "migrate_candidate", faction.parse::<SlotId>())?;
    let log_id = package_id.to_string();
    let _mutation = state.mutation.lock().await;
    let config = super::error::map(
        &app,
        &state,
        "migrate_candidate",
        super::load_config(&state),
    )?;
    let layout = super::error::map(
        &app,
        &state,
        "migrate_candidate",
        super::configured_layout(&config),
    )?;
    let store = super::error::map(&app, &state, "migrate_candidate", state.store())?;
    let revision = super::error::blocking(&app, &state, "migrate_candidate", move || {
        run_migration_after_game_guard(super::ensure_game_stopped, || {
            layout.validate_mutation_roots()?;
            let candidate = svccm_core::library::migration_candidates(&layout)
                .into_iter()
                .find(|candidate| candidate.candidate_id == candidate_id)
                .ok_or_else(|| {
                    svccm_core::error::user_err(
                        "migration_candidate_not_found",
                        "the migration candidate no longer exists",
                    )
                })?;
            let plan = plan_from_extracted(candidate.path())?;
            store.ingest(&package_id, faction, &plan)
        })
    })
    .await?;
    super::log::log_op(
        &app,
        "info",
        "migrate",
        &format!("{}@{}", log_id, &revision[..revision.len().min(12)]),
    );
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_ids_accept_only_opaque_hashes() {
        assert!(validate_candidate_id(&"a".repeat(64)).is_ok());
        for invalid in ["", "candidate", "../candidate"] {
            assert!(validate_candidate_id(invalid).is_err());
        }
    }

    #[test]
    fn running_game_rejection_prevents_migration_ingest() {
        let mutated = std::cell::Cell::new(false);

        let error = run_migration_after_game_guard(
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
    fn migration_guard_runs_before_ingest() {
        let guard_passed = std::cell::Cell::new(false);

        run_migration_after_game_guard(
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
