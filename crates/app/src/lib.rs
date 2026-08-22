//! StarVault CCM desktop shell.
//!
//! Thin Tauri layer: typed commands over `svccm-core`, no domain logic.
//! Errors surface as strings today; the typed error mapping lands with the
//! M3 reconciliation work (roadmap).

mod commands;
mod telemetry;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
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
            commands::list_campaigns,
            commands::activate_campaign,
            commands::restore_campaign,
            commands::read_log,
            commands::reconcile,
            commands::discover_game_exe,
            commands::reveal_package,
            commands::changelog,
            commands::remove_package,
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
