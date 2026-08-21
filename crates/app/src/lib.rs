//! StarVault CCM desktop shell.
//!
//! Thin Tauri layer: typed commands over `svccm-core`, no domain logic.
//! Errors surface as strings today; the typed error mapping lands with the
//! M3 reconciliation work (roadmap).

mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::list_library,
            commands::detect_legacy_ccm,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
