//! StarVault CCM desktop shell.
//!
//! Thin Tauri layer: typed commands over `svccm-core`, no domain logic.
//! Errors surface as strings today; the typed error mapping lands with the
//! M3 reconciliation work (roadmap).

mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::ImportState::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_library,
            commands::detect_legacy_ccm,
            commands::import_analyze,
            commands::import_ingest,
            commands::import_cancel,
            commands::get_config,
            commands::save_config,
            commands::list_campaigns,
            commands::activate_campaign,
            commands::restore_campaign,
            commands::read_log,
            commands::reconcile,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
