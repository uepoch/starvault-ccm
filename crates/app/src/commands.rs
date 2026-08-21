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

/// Metadata built purely from user-confirmed values (K2) when the package
/// carried none of its own.
fn fallback_metadata(
    title: Option<String>,
    desc: Option<String>,
) -> svccm_core::package::metadata::LegacyMetadata {
    svccm_core::package::metadata::LegacyMetadata {
        title,
        desc,
        ..Default::default()
    }
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
    let archive_name = zip_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let preview = preview_plan(&plan, archive_name.as_deref());
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
    title: Option<String>,
    desc: Option<String>,
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
        let mut plan = plan_from_extracted(&extracted_dir).map_err(|e| e.to_string())?;
        // K2: confirmed title/description win over detected ones.
        if let Some(m) = plan.metadata.as_mut() {
            if title.is_some() {
                m.title = title.clone();
            }
            if desc.is_some() {
                m.desc = desc.clone();
            }
        } else if title.is_some() || desc.is_some() {
            plan.metadata = Some(fallback_metadata(title, desc));
        }
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

/// Wipe all app data: store, ledger, log, config. Confirmation happens in
/// the UI; this is unrecoverable.
#[tauri::command]
pub fn clear_all_data(app: AppHandle) -> Result<(), String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?;
    // Roaming holds everything we own: store, ledger, log, config.
    if data_dir.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(&data_dir)
            .map_err(|e| format!("clear {}: {e}", data_dir.display()))?;
    }
    // The cache dir also hosts the WebView2 browser profile, which is locked
    // while the app runs — only remove our import scratch space inside it.
    let scratch = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("resolve cache dir: {e}"))?
        .join("import");
    if scratch.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(&scratch)
            .map_err(|e| format!("clear {}: {e}", scratch.display()))?;
    }
    Ok(())
}

// --- campaigns screen --------------------------------------------------------

use svccm_core::config::{Config, StrategyChoice};
use svccm_core::layout::WindowsLayout;
use svccm_core::slots::SlotManager;

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?
        .join("config.toml"))
}

fn load_config(app: &AppHandle) -> Result<Config, String> {
    Config::load(config_path(app)?).map_err(|e| e.to_string())
}

/// Game layout root derived from the configured exe (`…/StarCraft II.exe`).
fn layout_from_config(cfg: &Config) -> Result<WindowsLayout, String> {
    let exe = cfg.game_exe.as_ref().ok_or("game path not configured")?;
    let root = exe
        .parent()
        .ok_or("configured game path has no parent directory")?;
    let layout = WindowsLayout::new(root);
    layout.validate().map_err(|e| e.to_string())?;
    Ok(layout)
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigDto {
    pub game_exe: Option<String>,
    pub strategy_override: Option<String>,
    pub crash_reports_opt_in: bool,
}

#[tauri::command]
pub fn get_config(app: AppHandle) -> Result<ConfigDto, String> {
    let cfg = load_config(&app)?;
    Ok(ConfigDto {
        game_exe: cfg.game_exe.map(|p| p.display().to_string()),
        strategy_override: cfg.strategy_override.map(|s| match s {
            StrategyChoice::Junction => "junction".into(),
            StrategyChoice::Copy => "copy".into(),
        }),
        crash_reports_opt_in: cfg.crash_reports_opt_in,
    })
}

/// Persist settings; the game exe must exist when provided.
#[tauri::command]
pub fn save_config(
    app: AppHandle,
    game_exe: Option<String>,
    strategy_override: Option<String>,
    crash_reports_opt_in: bool,
) -> Result<(), String> {
    let mut cfg = load_config(&app)?;
    cfg.game_exe = game_exe.map(PathBuf::from);
    if let Some(ref exe) = cfg.game_exe {
        // The exact file the user typed must exist — a typo in the file name
        // must not pass just because its parent folder looks like an install.
        if !exe.is_file() {
            return Err(format!("no executable found at {}", exe.display()));
        }
        let root = exe.parent().ok_or("game path has no parent directory")?;
        WindowsLayout::new(root)
            .validate()
            .map_err(|e| e.to_string())?;
    }
    cfg.strategy_override = match strategy_override.as_deref() {
        Some("junction") => Some(StrategyChoice::Junction),
        Some("copy") => Some(StrategyChoice::Copy),
        _ => None,
    };
    cfg.crash_reports_opt_in = crash_reports_opt_in;
    cfg.save(config_path(&app)?).map_err(|e| e.to_string())
}

/// One slot card on the Campaigns screen.
#[derive(Debug, Clone, Serialize)]
pub struct CampaignSlot {
    pub slot: String,
    pub title: String,
    pub pkg_id: Option<String>,
    pub rev: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
}

/// The four slots as the UI renders them: plain campaign or package.
#[tauri::command]
pub fn list_campaigns(app: AppHandle) -> Result<Vec<CampaignSlot>, String> {
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    let active: HashMap<String, (String, String)> = store
        .active_slots()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(slot, id, rev)| (slot, (id, rev)))
        .collect();
    Ok(SlotId::ALL
        .into_iter()
        .map(|slot| {
            let (title, pkg_id, rev, author, version) = match active.get(slot.as_str()) {
                Some((id, r)) => match store.load_manifest(id, r) {
                    Ok(m) => (
                        m.title.clone().unwrap_or_else(|| id.clone()),
                        Some(id.clone()),
                        Some(r.clone()),
                        m.author,
                        m.version,
                    ),
                    Err(_) => (id.clone(), Some(id.clone()), Some(r.clone()), None, None),
                },
                None => ("Plain campaign".to_string(), None, None, None, None),
            };
            CampaignSlot {
                slot: slot.as_str().to_string(),
                title,
                pkg_id,
                rev,
                author,
                version,
            }
        })
        .collect())
}

/// Activate an installed package on a slot (K3: replaces whatever is there).
/// Cross-slot conflicts abort untouched; the error names both packages.
#[tauri::command]
pub fn activate_campaign(app: AppHandle, slot: String, id: String) -> Result<(), String> {
    let slot = slot_from_str(&slot)?;
    let cfg = load_config(&app)?;
    let layout = layout_from_config(&cfg)?;
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;

    // Latest installed revision of `id` (rows are sorted by revision string).
    let revs: Vec<String> = store
        .list_packages()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|(pid, _, _)| pid == &id)
        .map(|(_, rev, _)| rev)
        .collect();
    let rev = revs
        .last()
        .ok_or(format!("package `{id}` is not installed"))?
        .clone();

    // A campaign only loads through its own launcher, so the package's slot
    // is a fact, not a preference: enforce the binding at the boundary.
    let manifest_slot = store
        .load_manifest(&id, &rev)
        .map_err(|e| e.to_string())?
        .slot;
    if manifest_slot != slot.as_str() {
        return Err(format!(
            "`{id}` is built for the {} campaign and cannot go on {}",
            manifest_slot,
            slot.as_str()
        ));
    }

    let manager = SlotManager::new(&layout, &store).with_strategy(cfg.strategy_override);
    manager
        .activate(slot, &id, &rev)
        .map_err(|e| e.to_string())?;
    log_op(&app, "activate", &format!("{id} → {}", slot.as_str()));
    Ok(())
}

/// Return a slot to its plain Blizzard state.
#[tauri::command]
pub fn restore_campaign(app: AppHandle, slot: String) -> Result<(), String> {
    let slot = slot_from_str(&slot)?;
    let cfg = load_config(&app)?;
    let layout = layout_from_config(&cfg)?;
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    SlotManager::new(&layout, &store)
        .restore(slot)
        .map_err(|e| e.to_string())?;
    log_op(&app, "restore", slot.as_str());
    Ok(())
}

// --- operation log -----------------------------------------------------------

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// RFC 3339 timestamp.
    pub time: String,
    pub kind: String,
    pub detail: String,
}

fn log_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?
        .join("log.jsonl"))
}

fn log_op(app: &AppHandle, kind: &str, detail: &str) {
    let entry = LogEntry {
        // ponytail: second-resolution UTC stamp from std only; chrono when the
        // log needs sub-second ordering or local timezone display.
        time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
            .to_string(),
        kind: kind.to_string(),
        detail: detail.to_string(),
    };
    if let Ok(path) = log_path(app) {
        if let Ok(mut json) = serde_json::to_string(&entry) {
            json.push('\n');
            use std::io::Write;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = f.write_all(json.as_bytes());
            }
        }
    }
}

/// Recent operations, newest first (the support artifact).
#[tauri::command]
pub fn read_log(app: AppHandle, limit: usize) -> Result<Vec<LogEntry>, String> {
    let path = log_path(&app)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut out: Vec<LogEntry> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    out.reverse();
    out.truncate(limit);
    Ok(out)
}

// --- startup reconciliation --------------------------------------------------

/// Crash-recovery pass over all slots; returns repair notes for the log.
#[tauri::command]
pub fn reconcile(app: AppHandle) -> Result<Vec<String>, String> {
    let cfg = load_config(&app)?;
    if cfg.game_exe.is_none() {
        return Ok(Vec::new()); // nothing to reconcile without a game install
    }
    let layout = layout_from_config(&cfg)?;
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    SlotManager::new(&layout, &store)
        .with_strategy(cfg.strategy_override)
        .reconcile()
        .map_err(|e| e.to_string())
}

// --- launch (X1) -------------------------------------------------------------

use svccm_core::launch::{self, PreflightReport};

/// Verify-only pre-flight over the configured game install.
#[tauri::command]
pub fn launch_preflight(app: AppHandle) -> Result<PreflightReport, String> {
    let cfg = load_config(&app)?;
    let layout = layout_from_config(&cfg)?;
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    Ok(launch::preflight(&layout, &store))
}

/// Detached spawn of the game executable.
#[tauri::command]
pub fn launch_game(app: AppHandle) -> Result<(), String> {
    let cfg = load_config(&app)?;
    let layout = layout_from_config(&cfg)?;
    launch::launch(&layout)
        .map(|()| log_op(&app, "launch", "game started"))
        .map_err(|e| e.to_string())
}

/// Battle.net deep link when the local exe is unusable.
#[tauri::command]
pub fn launch_battlenet(app: AppHandle) -> Result<(), String> {
    launch::launch_battlenet()
        .map(|()| log_op(&app, "launch", "battlenet:// fallback"))
        .map_err(|e| e.to_string())
}

// --- migration (P2) ----------------------------------------------------------

use svccm_core::library::MigrationCandidate;

/// Custom campaign directories an old SC2CCM install left in Maps\Campaign.
#[tauri::command]
pub fn list_migration_candidates(app: AppHandle) -> Result<Vec<MigrationCandidate>, String> {
    let cfg = load_config(&app)?;
    if cfg.game_exe.is_none() {
        return Ok(Vec::new());
    }
    let layout = layout_from_config(&cfg)?;
    Ok(library::migration_candidates(&layout))
}

/// Import one legacy campaign through the normal pipeline so it is
/// normalized, hashed, and given a manifest like any other package.
#[tauri::command]
pub fn migrate_candidate(
    app: AppHandle,
    path: String,
    id: String,
    slot: String,
) -> Result<String, String> {
    let slot = slot_from_str(&slot)?;
    let src = PathBuf::from(&path);
    if !src.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    let plan = plan_from_extracted(&src).map_err(|e| e.to_string())?;
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    let rev = store
        .ingest_with_progress(&id, slot, &plan, |_| true)
        .map_err(|e| e.to_string())?
        .ok_or("migration cancelled")?;
    log_op(&app, "migrate", &format!("{id} from {path}"));
    Ok(rev)
}

// --- install discovery -------------------------------------------------------

/// Best-effort SC2 install detection (registry, then well-known folders).
#[tauri::command]
pub fn discover_game_exe() -> Option<String> {
    svccm_core::layout::discover_install().map(|p| p.display().to_string())
}

/// Remove an installed package (refuses while active on a faction).
#[tauri::command]
pub fn remove_package(app: AppHandle, id: String) -> Result<(), String> {
    let store = Store::open(store_root(&app)?).map_err(|e| e.to_string())?;
    store.remove_package(&id).map_err(|e| e.to_string())?;
    log_op(&app, "remove", &id);
    Ok(())
}
