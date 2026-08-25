//! StarVault CCM desktop shell.
//!
//! Thin Tauri layer: typed commands over `svccm-core`, no domain sequencing.
//! Public failures use the stable `CommandError` contract while full diagnostic
//! chains remain in the local operation log.

mod analytics;
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
        let check = match updater.check().await {
            Ok(v) => v,
            Err(e) => {
                commands::log_op(&app, "warn", "update", &format!("check failed: {e}"));
                return;
            }
        };
        let Some(update) = check else {
            return; // up to date
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
    // Local tracing remains separate from strict opt-in error telemetry.
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry().init();

    // Aptabase starts its polling task while Tauri initializes plugins, so
    // enter Tauri's Tokio context before building the application.
    let runtime = tauri::async_runtime::handle();
    let _runtime_guard = runtime.inner().enter();
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
        .plugin(analytics::plugin())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            commands::validate_regular_directory(&data_dir, "unsafe_app_data")?;
            let import_root = app.path().app_cache_dir()?.join("import");
            commands::init_log_level(&data_dir.join("config.toml"));
            let store =
                std::sync::Arc::new(svccm_core::store::Store::open(data_dir.join("store"))?);
            app.manage(commands::AppState::new(data_dir, import_root, store));
            // Marks the Log tab with the running build, so a stale exe is
            // obvious when diagnosing reports.
            commands::log_startup(app.handle());
            telemetry::init(app.handle());
            let state = app.state::<commands::AppState>();
            if let Ok(config) = commands::load_config(&state) {
                analytics::set_enabled(config.analytics_enabled);
                analytics::track(app.handle(), "app_started", &[]);
            }
            spawn_update_check(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::workflow::initialize,
            commands::workflow::list_library,
            commands::workflow::activate_package,
            commands::workflow::play_package,
            commands::workflow::restore_vanilla,
            commands::imports::import_analyze,
            commands::imports::import_ingest,
            commands::imports::import_cancel,
            commands::settings::get_config,
            commands::settings::save_config,
            commands::settings::set_analytics,
            commands::workflow::clear_all_data,
            commands::log::clear_log,
            commands::log::read_log,
            commands::settings::discover_game_exe,
            commands::packages::reveal_package,
            commands::settings::changelog,
            commands::settings::get_saves_status,
            commands::packages::remove_package,
            commands::packages::edit_package_metadata,
            commands::migration::list_migration_candidates,
            commands::migration::migrate_candidate,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                analytics::flush_on_exit(app);
            }
        });
}
