//! Typed Tauri commands over `svccm-core`.
//!
//! Each command maps a core `Error` to its display string for now; the
//! structured error surface (report ids, details expander) lands in M3.

use svccm_core::library::{self, LegacyCcmInstall, LibraryEntry};
use svccm_core::store::Store;
use tauri::Manager;

/// Store root under the OS app-data dir (`%APPDATA%\StarVault\CCM\store`).
fn store_root(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?;
    Ok(dir.join("store"))
}

#[tauri::command]
pub fn list_library(app: tauri::AppHandle) -> Result<Vec<LibraryEntry>, String> {
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    library::scan(&store).map_err(|e| e.to_string())
}

/// Detect an old SC2CCM install under the OS roaming dir (P2 migration).
#[tauri::command]
pub fn detect_legacy_ccm(app: tauri::AppHandle) -> Result<Option<LegacyCcmInstall>, String> {
    let appdata = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?
        // Roaming parent: `<base>/StarVault/CCM` → strip to `%APPDATA%`.
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "cannot resolve roaming profile dir".to_string())?;
    Ok(LegacyCcmInstall::detect(appdata))
}
