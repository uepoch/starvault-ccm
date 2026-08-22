//! StarVault CCM desktop shell.
//!
//! Thin Tauri layer: typed commands over `svccm-core`, no domain logic.
//! Errors surface as strings today; the typed error mapping lands with the
//! M3 reconciliation work (roadmap).

mod commands;
mod telemetry;

use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

/// Transparent self-update: check once at startup, and if a newer release
/// exists, download and install it in the background (passive NSIS UI). The
/// new version takes effect on the next launch - never mid-session. Every
/// outcome is logged; failures stay invisible to the user.
fn spawn_update_check(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Ok(updater) = app.updater() else {
            return;
        };
        let Ok(Some(update)) = updater.check().await else {
            return;
        };
        let version = update.version.clone();
        let level = match update
            .download_and_install(|_chunk, _total| {}, || {})
            .await
        {
            Ok(()) => ("info", format!("updated to {version} - restarting")),
            Err(e) => ("warn", format!("install of {version} failed: {e}")),
        };
        commands::log_op(&app, level.0, "update", &level.1);
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be the FIRST plugin: a second app instance exits here and
        // focuses the existing window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(commands::LibraryCache::default())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            commands::init_log_level(&data_dir.join("config.toml"));
            app.manage(commands::AppState {
                store: std::sync::Mutex::new(Some(std::sync::Arc::new(
                    svccm_core::store::Store::open(data_dir.join("store"))?,
                ))),
                store_path: data_dir.join("store"),
                import_ops: Default::default(),
                config_cache: Default::default(),
                campaigns_cache: Default::default(),
                config_path: data_dir.join("config.toml"),
            });
            // Marks the Log tab with the running build, so a stale exe is
            // obvious when diagnosing reports.
            commands::log_startup(app.handle());
            commands::spawn_refresh(app.handle());
            telemetry::init(app.handle());
            spawn_update_check(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_library,
            commands::detect_legacy_ccm,
            commands::import_analyze,
            commands::import_ingest,
            commands::import_cancel,
            commands::get_config,
            commands::save_config,
            commands::clear_all_data,
            commands::clear_log,
            commands::list_campaigns,
            commands::activate_campaign,
            commands::restore_campaign,
            commands::read_log,
            commands::reconcile,
            commands::discover_game_exe,
            commands::reveal_package,
            commands::changelog,
            commands::get_saves_status,
            commands::remove_package,
            commands::edit_package_metadata,
            commands::launch_preflight,
            commands::launch_game,
            commands::launch_package,
            commands::launch_battlenet,
            commands::list_migration_candidates,
            commands::migrate_candidate,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
