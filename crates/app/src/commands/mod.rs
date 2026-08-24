//! Thin Tauri adapters over the core single-campaign workflow.

pub(crate) mod error;
pub(crate) mod imports;
pub(crate) mod log;
pub(crate) mod migration;
pub(crate) mod packages;
pub(crate) mod settings;
pub(crate) mod workflow;

pub(crate) use log::log_op;
pub use log::{init_log_level, log_startup};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use svccm_core::config::Config;
use svccm_core::error::{internal_err, user_err, EnvironmentError, Result};
use svccm_core::layout::WindowsLayout;
use svccm_core::saves::{resolve_profile, SavesManager};
use svccm_core::store::Store;
use svccm_core::workflow::Workflow;
use tauri::{AppHandle, Manager};

use self::imports::ImportOp;

pub type CommandResult<T> = std::result::Result<T, svccm_core::CommandError>;

/// Process-wide shell state. Domain state remains in `Store` and `Workflow`.
pub struct AppState {
    store: Mutex<Option<Arc<Store>>>,
    pub store_path: PathBuf,
    pub config_path: PathBuf,
    pub app_data_path: PathBuf,
    pub import_root: PathBuf,
    import_ops: Arc<Mutex<HashMap<String, ImportOp>>>,
    pub mutation: tokio::sync::Mutex<()>,
    pub telemetry: crate::telemetry::TelemetryState,
}

impl AppState {
    pub fn new(app_data_path: PathBuf, import_root: PathBuf, store: Arc<Store>) -> Self {
        Self {
            store: Mutex::new(Some(store)),
            store_path: app_data_path.join("store"),
            config_path: app_data_path.join("config.toml"),
            app_data_path,
            import_root,
            import_ops: Arc::new(Mutex::new(HashMap::new())),
            mutation: tokio::sync::Mutex::new(()),
            telemetry: Default::default(),
        }
    }

    pub fn store(&self) -> Result<Arc<Store>> {
        let mut guard = self.store.lock().map_err(|_| {
            internal_err(
                "store_lock_poisoned",
                "StarVault could not open its store",
                "store mutex was poisoned",
            )
        })?;
        if let Some(store) = guard.as_ref() {
            return Ok(store.clone());
        }
        let store = Arc::new(Store::open(&self.store_path)?);
        *guard = Some(store.clone());
        Ok(store)
    }

    pub(super) fn close_store(&self) -> Result<()> {
        let mut guard = self.store.lock().map_err(|_| {
            internal_err(
                "store_lock_poisoned",
                "StarVault could not close its store",
                "store mutex was poisoned",
            )
        })?;
        guard.take();
        Ok(())
    }
}

pub fn load_config(state: &AppState) -> Result<Config> {
    Config::load(&state.config_path)
}

pub(super) fn configured_layout(config: &Config) -> Result<WindowsLayout> {
    let executable = config
        .game_exe
        .as_ref()
        .ok_or(EnvironmentError::GameNotFound)?;
    if !executable.is_file() {
        return Err(EnvironmentError::GameNotFound.into());
    }
    let root = executable.parent().ok_or(EnvironmentError::GameNotFound)?;
    let layout = WindowsLayout::new(root);
    layout.validate()?;
    Ok(layout)
}

fn readable_layout(state: &AppState, config: &Config) -> WindowsLayout {
    config
        .game_exe
        .as_ref()
        .and_then(|executable| executable.parent())
        .map(WindowsLayout::new)
        .or_else(|| {
            svccm_core::layout::discover_install()
                .and_then(|executable| executable.parent().map(WindowsLayout::new))
        })
        .unwrap_or_else(|| WindowsLayout::new(state.store_path.join("unconfigured-game")))
}

pub(super) fn documents_dir(app: &AppHandle) -> Result<PathBuf> {
    app.path().document_dir().map_err(|error| {
        user_err(
            "documents_unavailable",
            format!("StarVault could not locate Documents: {error}"),
        )
    })
}

const ONEDRIVE_ROOT_ENVIRONMENT: [&str; 3] = ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"];

fn onedrive_roots_from(
    mut read_environment: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    ONEDRIVE_ROOT_ENVIRONMENT
        .iter()
        .filter_map(|name| read_environment(name))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

pub(super) fn documents_on_onedrive(documents: &Path) -> bool {
    let roots = onedrive_roots_from(|name| std::env::var_os(name));
    svccm_core::saves::is_onedrive_with_roots(documents, &roots)
}

fn saves_manager(app: &AppHandle, config: &Config, store: &Store) -> Result<Option<SavesManager>> {
    if !config.save_isolation {
        return Ok(None);
    }
    let documents = documents_dir(app)?;
    if documents_on_onedrive(&documents) {
        return Err(EnvironmentError::OneDriveUnsupported.into());
    }
    let profile_id = config.saves_profile.as_ref().ok_or_else(|| {
        user_err(
            "save_profile_required",
            "select a StarCraft II save profile before enabling isolation",
        )
    })?;
    let profile = resolve_profile(&documents, profile_id)?;
    Ok(Some(SavesManager::new(
        profile.saves_dir().to_path_buf(),
        store.root(),
    )))
}

pub(super) struct WorkflowContext {
    pub store: Arc<Store>,
    pub layout: WindowsLayout,
    strategy: Option<svccm_core::config::StrategyChoice>,
    saves: Option<SavesManager>,
    save_isolation_expected: bool,
    external_mods_policy: svccm_core::mods::ExternalModsPolicy,
}

impl WorkflowContext {
    pub fn configured(app: &AppHandle, state: &AppState) -> Result<Self> {
        let config = load_config(state)?;
        Self::from_config(app, state, &config, true)
    }

    pub fn readable(app: &AppHandle, state: &AppState) -> Result<Self> {
        let config = load_config(state)?;
        Self::from_config(app, state, &config, false)
    }

    pub fn from_config(
        app: &AppHandle,
        state: &AppState,
        config: &Config,
        require_valid_game: bool,
    ) -> Result<Self> {
        let store = state.store()?;
        let layout = if require_valid_game {
            configured_layout(config)?
        } else {
            readable_layout(state, config)
        };
        let saves = match saves_manager(app, config, &store) {
            Ok(saves) => saves,
            Err(_) if config.save_isolation && !require_valid_game => None,
            Err(error) => return Err(error),
        };
        Ok(Self {
            store,
            layout,
            strategy: config.strategy_override,
            saves,
            save_isolation_expected: config.save_isolation,
            external_mods_policy: if config.replace_external_mods {
                svccm_core::mods::ExternalModsPolicy::Replace
            } else {
                svccm_core::mods::ExternalModsPolicy::Reject
            },
        })
    }

    pub fn without_saves(
        state: &AppState,
        config: &Config,
        save_isolation_expected: bool,
    ) -> Result<Self> {
        Ok(Self {
            store: state.store()?,
            layout: configured_layout(config)?,
            strategy: config.strategy_override,
            saves: None,
            save_isolation_expected,
            external_mods_policy: if config.replace_external_mods {
                svccm_core::mods::ExternalModsPolicy::Replace
            } else {
                svccm_core::mods::ExternalModsPolicy::Reject
            },
        })
    }

    pub fn replace_external_mods_for_this_operation(&mut self) {
        self.external_mods_policy = svccm_core::mods::ExternalModsPolicy::Replace;
    }

    pub fn workflow(&self) -> Workflow<'_> {
        Workflow::new(&self.layout, &self.store)
            .with_strategy(self.strategy)
            .with_external_mods_policy(self.external_mods_policy)
            .with_saves(self.saves.clone())
            .with_save_isolation_expected(self.save_isolation_expected)
    }
}

pub(super) fn ensure_game_stopped() -> Result<()> {
    if svccm_core::launch::sc2_running() {
        Err(EnvironmentError::GameRunning.into())
    } else {
        Ok(())
    }
}

pub(super) fn validate_regular_directory(path: &Path, code: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| svccm_core::error::user_path_err(code, error.to_string(), path, false))?;
    if !metadata.is_dir() || svccm_core::filesystem::is_link_or_reparse(&metadata) {
        return Err(svccm_core::error::user_path_err(
            code,
            "refusing to operate on a linked or non-directory root",
            path,
            false,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onedrive_detection_uses_all_shell_environment_roots() {
        let roots = onedrive_roots_from(|name| match name {
            "OneDrive" => Some(std::ffi::OsString::from("C:/Cloud/Personal")),
            "OneDriveConsumer" => Some(std::ffi::OsString::new()),
            "OneDriveCommercial" => Some(std::ffi::OsString::from("C:/Cloud/Company")),
            _ => None,
        });

        assert_eq!(
            roots,
            vec![
                PathBuf::from("C:/Cloud/Personal"),
                PathBuf::from("C:/Cloud/Company")
            ]
        );
        assert!(svccm_core::saves::is_onedrive_with_roots(
            Path::new("C:/Cloud/Company/Documents"),
            &roots
        ));
    }
}
