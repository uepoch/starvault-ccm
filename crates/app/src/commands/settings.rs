use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use svccm_core::config::{Config, StrategyChoice};
use svccm_core::contracts::ActiveCampaign;
use svccm_core::operation::PendingOperation;
use svccm_core::saves::{create_recovery_backup, discover, resolve_profile};
use svccm_core::ProfileId;
use tauri::AppHandle;

use super::{AppState, CommandResult, WorkflowContext};

#[derive(Debug, Clone, Serialize)]
pub struct ConfigDto {
    pub game_exe: Option<String>,
    pub strategy_override: Option<String>,
    pub crash_reports_opt_in: bool,
    pub analytics_enabled: bool,
    pub analytics_acknowledged: bool,
    pub log_level: String,
    pub save_isolation: bool,
    pub saves_profile: Option<String>,
    pub replace_external_mods: bool,
}

impl From<Config> for ConfigDto {
    fn from(config: Config) -> Self {
        Self {
            game_exe: config
                .game_exe
                .map(|executable| executable.display().to_string()),
            strategy_override: config.strategy_override.map(|strategy| match strategy {
                StrategyChoice::Junction => "junction".into(),
                StrategyChoice::Copy => "copy".into(),
            }),
            crash_reports_opt_in: config.crash_reports_opt_in,
            analytics_enabled: config.analytics_enabled,
            analytics_acknowledged: config.analytics_acknowledged,
            log_level: config.log_level,
            save_isolation: config.save_isolation,
            saves_profile: config
                .saves_profile
                .map(|profile| profile.as_str().to_string()),
            replace_external_mods: config.replace_external_mods,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigExtras {
    pub save_isolation: Option<bool>,
    pub saves_profile: Option<String>,
    pub replace_external_mods: Option<bool>,
    pub analytics_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavesProfileDto {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SavesStatus {
    pub supported: bool,
    pub reason: Option<String>,
    pub profiles: Vec<SavesProfileDto>,
    pub selected: Option<String>,
    pub enabled: bool,
}

fn guard_protected_settings_change(
    active_campaign: Option<&ActiveCampaign>,
) -> svccm_core::error::Result<()> {
    if active_campaign.is_some() {
        return Err(svccm_core::error::user_err(
            "settings_locked_while_active",
            "return to vanilla before changing deployment or save settings",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct SavePreparation {
    documents: PathBuf,
    profile_id: ProfileId,
    create_backup: bool,
}

fn guard_unconfigured_settings_change(
    store_root: &std::path::Path,
    ensure_stopped: impl FnOnce() -> svccm_core::error::Result<()>,
) -> svccm_core::error::Result<()> {
    ensure_stopped()?;
    if PendingOperation::load(store_root)?.is_some() {
        return Err(svccm_core::error::package_err(
            "recovery_required",
            "configure the previous game path before changing protected settings",
        ));
    }
    Ok(())
}

fn prepare_save_profile(request: &SavePreparation) -> svccm_core::error::Result<()> {
    resolve_profile(&request.documents, &request.profile_id)?;
    if request.create_backup {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                svccm_core::error::internal_err(
                    "system_clock_before_epoch",
                    "StarVault could not timestamp the recovery backup",
                    error.to_string(),
                )
            })?
            .as_secs();
        create_recovery_backup(&request.documents, &request.profile_id, timestamp)?;
    }
    Ok(())
}

fn persist_config_after_save_preparation(
    config: &Config,
    config_path: &std::path::Path,
    preparation: Option<&SavePreparation>,
) -> svccm_core::error::Result<()> {
    if let Some(preparation) = preparation {
        prepare_save_profile(preparation)?;
    }
    config.save(config_path)
}

fn needs_save_preparation(save_isolation: bool, profile_changed: bool) -> bool {
    save_isolation || profile_changed
}

#[tauri::command]
pub async fn get_config(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<ConfigDto> {
    let _mutation = state.mutation.lock().await;
    super::error::map(&app, &state, "get_config", super::load_config(&state)).map(Into::into)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn save_config(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    game_exe: Option<String>,
    strategy_override: Option<String>,
    crash_reports_opt_in: bool,
    log_level: Option<String>,
    extras: Option<ConfigExtras>,
) -> CommandResult<()> {
    let game_exe = game_exe.map(PathBuf::from);
    if let Some(executable) = &game_exe {
        if !executable.is_file() {
            return Err(super::error::report(
                &app,
                &state,
                "save_config",
                svccm_core::error::user_path_err(
                    "invalid_game_executable",
                    "the selected StarCraft II executable does not exist",
                    executable,
                    false,
                ),
            ));
        }
        let root = executable.parent().ok_or_else(|| {
            super::error::report(
                &app,
                &state,
                "save_config",
                svccm_core::error::user_err(
                    "invalid_game_executable",
                    "the selected executable has no parent directory",
                ),
            )
        })?;
        let layout = svccm_core::layout::WindowsLayout::new(root);
        super::error::map(&app, &state, "save_config", layout.validate())?;
        super::error::map(
            &app,
            &state,
            "save_config",
            layout.validate_mutation_roots(),
        )?;
    }
    let strategy = match strategy_override.as_deref() {
        None => None,
        Some("junction") => Some(StrategyChoice::Junction),
        Some("copy") => Some(StrategyChoice::Copy),
        Some(_) => {
            return Err(super::error::report(
                &app,
                &state,
                "save_config",
                svccm_core::error::user_err(
                    "invalid_deployment_strategy",
                    "deployment strategy must be auto, junction, or copy",
                ),
            ));
        }
    };
    let log_level = log_level.unwrap_or_else(|| "info".into());
    if !matches!(log_level.as_str(), "info" | "warn" | "error") {
        return Err(super::error::report(
            &app,
            &state,
            "save_config",
            svccm_core::error::user_err(
                "invalid_log_level",
                "log level must be info, warn, or error",
            ),
        ));
    }
    let extras = extras.unwrap_or_default();
    let requested_profile = match extras.saves_profile.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(value) => Some(Some(super::error::map(
            &app,
            &state,
            "save_config",
            ProfileId::parse(value),
        )?)),
    };

    let _mutation = state.mutation.lock().await;
    let previous = super::error::map(&app, &state, "save_config", super::load_config(&state))?;
    let target = Config {
        game_exe,
        strategy_override: strategy,
        crash_reports_opt_in,
        log_level: log_level.clone(),
        save_isolation: extras.save_isolation.unwrap_or(previous.save_isolation),
        saves_profile: requested_profile.unwrap_or_else(|| previous.saves_profile.clone()),
        replace_external_mods: extras
            .replace_external_mods
            .unwrap_or(previous.replace_external_mods),
        analytics_enabled: extras
            .analytics_enabled
            .unwrap_or(previous.analytics_enabled),
        analytics_acknowledged: previous.analytics_acknowledged,
    };
    if target.save_isolation && target.saves_profile.is_none() {
        return Err(super::error::report(
            &app,
            &state,
            "save_config",
            svccm_core::error::user_err(
                "save_profile_required",
                "select a StarCraft II save profile before enabling isolation",
            ),
        ));
    }

    let deployment_changed = previous.game_exe != target.game_exe
        || previous.strategy_override != target.strategy_override;
    let save_changed = previous.save_isolation != target.save_isolation
        || previous.saves_profile != target.saves_profile;
    let mut save_preparation = None;
    if deployment_changed || save_changed {
        let store = super::error::map(&app, &state, "save_config", state.store())?;
        let active_campaign =
            super::error::map(&app, &state, "save_config", store.active_campaign())?;
        super::error::map(
            &app,
            &state,
            "save_config",
            guard_protected_settings_change(active_campaign.as_ref()),
        )?;

        if previous.game_exe.is_some() {
            let context = super::error::map(
                &app,
                &state,
                "save_config",
                WorkflowContext::readable(&app, &state),
            )?;
            super::error::blocking(&app, &state, "save_config", move || {
                context.workflow().restore_vanilla()
            })
            .await?;
        } else {
            super::error::map(
                &app,
                &state,
                "save_config",
                guard_unconfigured_settings_change(store.root(), super::ensure_game_stopped),
            )?;
        }

        if target.game_exe.is_some() && target.game_exe != previous.game_exe {
            let context = super::error::map(
                &app,
                &state,
                "save_config",
                WorkflowContext::without_saves(&state, &target, false),
            )?;
            super::error::blocking(&app, &state, "save_config", move || {
                context.workflow().restore_vanilla()
            })
            .await?;
        }

        let profile_changed = previous.saves_profile != target.saves_profile;
        let enabling = !previous.save_isolation && target.save_isolation;
        if let Some(profile_id) = target
            .saves_profile
            .clone()
            .filter(|_| needs_save_preparation(target.save_isolation, profile_changed))
        {
            let documents =
                super::error::map(&app, &state, "save_config", super::documents_dir(&app))?;
            if super::documents_on_onedrive(&documents) {
                return Err(super::error::report(
                    &app,
                    &state,
                    "save_config",
                    svccm_core::error::EnvironmentError::OneDriveUnsupported.into(),
                ));
            }
            save_preparation = Some(SavePreparation {
                documents,
                profile_id,
                create_backup: enabling || profile_changed,
            });
        }
    }

    let persisted = target.clone();
    let config_path = state.config_path.clone();
    super::error::blocking(&app, &state, "save_config", move || {
        persist_config_after_save_preparation(&persisted, &config_path, save_preparation.as_ref())
    })
    .await?;
    state.telemetry.set_enabled(target.crash_reports_opt_in);
    crate::analytics::set_enabled(target.analytics_enabled);
    super::log::set_log_level(&target.log_level);
    super::log::log_op(&app, "info", "config", "settings saved");
    Ok(())
}

/// Acknowledge the first-launch analytics disclaimer and/or flip the
/// opt-out. The only write path the disclaimer itself uses.
#[tauri::command]
pub async fn set_analytics(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
    acknowledged: bool,
) -> CommandResult<()> {
    let config_path = state.config_path.clone();
    let _mutation = state.mutation.lock().await;
    let mut config = super::error::map(&app, &state, "set_analytics", super::load_config(&state))?;
    config.analytics_enabled = enabled;
    config.analytics_acknowledged = acknowledged;
    let persisted = config.clone();
    super::error::blocking(&app, &state, "set_analytics", move || {
        persisted.save(&config_path)
    })
    .await?;
    crate::analytics::set_enabled(enabled);
    Ok(())
}

#[tauri::command]
pub async fn get_saves_status(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<SavesStatus> {
    let _mutation = state.mutation.lock().await;
    let config = super::error::map(&app, &state, "get_saves_status", super::load_config(&state))?;
    let documents =
        super::error::map(&app, &state, "get_saves_status", super::documents_dir(&app))?;
    if super::documents_on_onedrive(&documents) {
        return Ok(SavesStatus {
            supported: false,
            reason: Some("Documents is managed by OneDrive. Save isolation is unavailable.".into()),
            profiles: Vec::new(),
            selected: config
                .saves_profile
                .map(|profile| profile.as_str().to_string()),
            enabled: config.save_isolation,
        });
    }
    let profiles = super::error::blocking(&app, &state, "get_saves_status", move || {
        discover(&documents)
    })
    .await?;
    let selected = config
        .saves_profile
        .as_ref()
        .map(|profile| profile.as_str().to_string())
        .or_else(|| (profiles.len() == 1).then(|| profiles[0].id.as_str().to_string()));
    Ok(SavesStatus {
        supported: !profiles.is_empty(),
        reason: profiles
            .is_empty()
            .then(|| "No StarCraft II save profile was found. Launch the game once first.".into()),
        profiles: profiles
            .into_iter()
            .map(|profile| SavesProfileDto {
                id: profile.id.as_str().to_string(),
                label: profile.display_label,
            })
            .collect(),
        selected,
        enabled: config.save_isolation,
    })
}

#[tauri::command]
pub fn discover_game_exe() -> Option<String> {
    svccm_core::layout::discover_install().map(|path| path.display().to_string())
}

#[tauri::command]
pub fn changelog() -> String {
    include_str!("../../../../CHANGELOG.md").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use svccm_core::error::ErrorKind;
    use svccm_core::identity::PackageId;
    use svccm_core::layout::SlotId;

    fn discovered_profile(documents: &std::path::Path) -> ProfileId {
        let profile = documents.join("StarCraft II/Accounts/120927238/2-S2-1-3475134");
        std::fs::create_dir_all(profile.join("Saves/Campaign")).unwrap();
        std::fs::create_dir_all(profile.join("Banks/author")).unwrap();
        std::fs::write(profile.join("Saves/Campaign/save.SC2Save"), b"save").unwrap();
        std::fs::write(profile.join("Banks/author/bank.SC2Bank"), b"bank").unwrap();
        discover(documents).unwrap().remove(0).id
    }

    #[test]
    fn protected_settings_are_locked_while_a_campaign_is_active() {
        let active = ActiveCampaign {
            id: PackageId::parse("campaign-a").unwrap(),
            revision: "revision-a".into(),
            faction: SlotId::LotV,
        };

        let error = guard_protected_settings_change(Some(&active)).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::User);
        assert_eq!(error.code(), "settings_locked_while_active");
        assert_eq!(
            error.to_string(),
            "return to vanilla before changing deployment or save settings"
        );
        assert!(!error.retryable());
    }

    #[test]
    fn protected_settings_are_allowed_in_vanilla() {
        guard_protected_settings_change(None).unwrap();
    }

    #[test]
    fn disabling_isolation_does_not_require_the_old_profile_to_resolve() {
        assert!(!needs_save_preparation(false, false));
        assert!(needs_save_preparation(true, false));
        assert!(needs_save_preparation(false, true));
    }

    #[test]
    fn running_game_guard_precedes_unconfigured_settings_recovery_checks() {
        let temporary = tempfile::tempdir().unwrap();
        let store_root = temporary.path().join("store");
        std::fs::create_dir(&store_root).unwrap();
        std::fs::write(
            PendingOperation::path(&store_root),
            b"not a valid operation journal",
        )
        .unwrap();

        let error = guard_unconfigured_settings_change(&store_root, || {
            Err(svccm_core::error::EnvironmentError::GameRunning.into())
        })
        .unwrap_err();

        assert_eq!(error.code(), "game_running");
    }

    #[test]
    fn failed_save_preparation_preserves_the_previous_config() {
        let temporary = tempfile::tempdir().unwrap();
        let config_path = temporary.path().join("config.toml");
        let previous = Config {
            log_level: "warn".into(),
            ..Config::default()
        };
        previous.save(&config_path).unwrap();
        let target = Config {
            save_isolation: true,
            saves_profile: Some(ProfileId::parse("a".repeat(64)).unwrap()),
            ..Config::default()
        };
        let preparation = SavePreparation {
            documents: temporary.path().join("Documents"),
            profile_id: target.saves_profile.clone().unwrap(),
            create_backup: true,
        };

        persist_config_after_save_preparation(&target, &config_path, Some(&preparation))
            .unwrap_err();

        assert_eq!(Config::load(&config_path).unwrap(), previous);
    }

    #[test]
    fn save_backup_is_created_before_config_persistence() {
        let temporary = tempfile::tempdir().unwrap();
        let documents = temporary.path().join("Documents");
        std::fs::create_dir(&documents).unwrap();
        let profile_id = discovered_profile(&documents);
        let occupied_config_path = temporary.path().join("config.toml");
        std::fs::create_dir(&occupied_config_path).unwrap();
        let target = Config {
            save_isolation: true,
            saves_profile: Some(profile_id.clone()),
            ..Config::default()
        };
        let preparation = SavePreparation {
            documents: documents.clone(),
            profile_id,
            create_backup: true,
        };

        persist_config_after_save_preparation(&target, &occupied_config_path, Some(&preparation))
            .unwrap_err();

        let backups = std::fs::read_dir(documents.join("StarVault CCM Recovery"))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read(backups[0].path().join("Saves/Campaign/save.SC2Save")).unwrap(),
            b"save"
        );
        assert_eq!(
            std::fs::read(backups[0].path().join("Banks/author/bank.SC2Bank")).unwrap(),
            b"bank"
        );
    }
}
