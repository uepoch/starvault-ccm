//! Typed Tauri commands over `svccm-core`.
//!
//! Each command maps a core `Error` to its display string for now; the
//! structured error surface (report ids, details expander) lands in M3.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use svccm_core::layout::SlotId;
use svccm_core::library::{self, LegacyCcmInstall, LibraryEntry};
use svccm_core::package::import::{extract_archive, preview_plan, ImportProgress};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::Store;
use tauri::{AppHandle, Emitter, Manager};

/// Store root under the OS app-data dir (`%APPDATA%\StarVault\CCM\store`).
fn store_root(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?;
    Ok(dir.join("store"))
}

#[tauri::command]
pub fn list_library(app: AppHandle) -> Result<Vec<LibraryEntry>, String> {
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    library::scan(&store).map_err(|e| e.to_string())
}

/// Detect an old SC2CCM install under the OS roaming dir (P2 migration).
#[tauri::command]
pub fn detect_legacy_ccm(app: AppHandle) -> Result<Option<LegacyCcmInstall>, String> {
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

// --- import wizard (K2) -----------------------------------------------------

/// One in-flight import: extracted tree plus its cancel flag.
struct ImportOp {
    extracted_dir: PathBuf,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct ImportState {
    ops: Mutex<HashMap<String, ImportOp>>,
}

/// Progress event shape emitted as `import-progress`.
#[derive(Debug, Clone, Serialize)]
struct ProgressEvent<'a> {
    op_id: &'a str,
    /// `extract` or `ingest`.
    phase: &'a str,
    files_done: u64,
    files_total: u64,
    current_file: &'a str,
}

fn slot_from_str(slot: &str) -> Result<SlotId, String> {
    SlotId::ALL
        .into_iter()
        .find(|s| s.as_str() == slot)
        .ok_or_else(|| format!("unknown slot `{slot}`"))
}

/// Analyze a package archive: extract to a scratch dir and preview what an
/// import would do. Emits `import-progress` events for the extract phase.
#[tauri::command]
pub fn import_analyze(
    app: AppHandle,
    state: tauri::State<ImportState>,
    op_id: String,
    path: String,
) -> Result<svccm_core::package::import::ImportPreview, String> {
    let zip_path = PathBuf::from(&path);
    if !zip_path.is_file() {
        return Err(format!("not a file: {path}"));
    }
    let scratch = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("resolve cache dir: {e}"))?;
    let extracted_dir = scratch.join("import").join(&op_id);
    std::fs::create_dir_all(&extracted_dir).map_err(|e| e.to_string())?;

    let app_for_cb = app.clone();
    let op_id_for_cb = op_id.clone();
    let completed = extract_archive(&zip_path, &extracted_dir, |p: ImportProgress| {
        let _ = app_for_cb.emit(
            "import-progress",
            ProgressEvent {
                op_id: &op_id_for_cb,
                phase: "extract",
                files_done: p.files_done,
                files_total: p.files_total,
                current_file: &p.current_file,
            },
        );
        true
    })
    .map_err(|e| e.to_string())?;
    if !completed {
        let _ = std::fs::remove_dir_all(&extracted_dir);
        return Err("extraction cancelled".into());
    }

    let plan = plan_from_extracted(&extracted_dir).map_err(|e| e.to_string())?;
    let preview = preview_plan(&plan);
    state.ops.lock().expect("import ops poisoned").insert(
        op_id,
        ImportOp {
            extracted_dir,
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    Ok(preview)
}

/// Ingest a confirmed import. Emits `import-progress` events for the ingest
/// phase; honours [`import_cancel`]. Returns the new revision, or `None` when
/// cancelled.
#[tauri::command]
pub fn import_ingest(
    app: AppHandle,
    state: tauri::State<ImportState>,
    op_id: String,
    id: String,
    slot: String,
) -> Result<Option<String>, String> {
    let slot = slot_from_str(&slot)?;
    let (extracted_dir, cancel) = {
        let mut ops = state.ops.lock().expect("import ops poisoned");
        let op = ops
            .remove(&op_id)
            .ok_or_else(|| format!("no such import operation: {op_id}"))?;
        (op.extracted_dir, op.cancel)
    };

    let result = (|| {
        let plan = plan_from_extracted(&extracted_dir).map_err(|e| e.to_string())?;
        let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
        let app_for_cb = app.clone();
        let op_id_for_cb = op_id.clone();
        store
            .ingest_with_progress(&id, slot, &plan, |p: ImportProgress| {
                let _ = app_for_cb.emit(
                    "import-progress",
                    ProgressEvent {
                        op_id: &op_id_for_cb,
                        phase: "ingest",
                        files_done: p.files_done,
                        files_total: p.files_total,
                        current_file: &p.current_file,
                    },
                );
                !cancel.load(Ordering::Relaxed)
            })
            .map_err(|e| e.to_string())
    })();

    let _ = std::fs::remove_dir_all(&extracted_dir);
    result
}

/// Cancel an in-flight import at the next file boundary.
#[tauri::command]
pub fn import_cancel(state: tauri::State<ImportState>, op_id: String) {
    if let Some(op) = state.ops.lock().expect("import ops poisoned").get(&op_id) {
        op.cancel.store(true, Ordering::Relaxed);
    }
}
