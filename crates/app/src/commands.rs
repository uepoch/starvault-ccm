//! Typed Tauri commands over `svccm-core`.
//!
//! Each command maps a core `Error` to its display string for now; the
//! structured error surface (report ids, details expander) lands in M3.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use svccm_core::layout::{GameLayout, SlotId};
use svccm_core::library::{self, LegacyCcmInstall, LibraryEntry};
use svccm_core::package::import::{extract_archive, preview_plan, ImportProgress};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::{PackageManifest, Store};
use tauri::{AppHandle, Emitter, Manager};

#[tauri::command]
pub async fn list_library(
    store_state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
) -> Result<Vec<LibraryEntry>, String> {
    list_library_inner(&store_state, &cache)
}

fn list_library_inner(
    store_state: &tauri::State<'_, AppState>,
    cache: &tauri::State<'_, LibraryCache>,
) -> Result<Vec<LibraryEntry>, String> {
    let mut cached = cache.entries.lock().expect("library cache poisoned");
    if let Some(entries) = cached.as_ref() {
        return Ok(entries.clone());
    }
    let store = store_state.store()?;
    let entries = library::scan(&store).map_err(|e| e.to_string())?;
    *cached = Some(entries.clone());
    Ok(entries)
}

/// Recompute both caches off the UI path: after mutations and at startup,
/// so a slow cold read (AV scanning freshly written files) never lands on
/// the user.
pub fn spawn_refresh(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let store_state = app.state::<AppState>();
        let cache = app.state::<LibraryCache>();
        if let Err(e) = list_library_inner(&store_state, &cache) {
            log_op(
                &app,
                "warn",
                "perf",
                &format!("cache warm library failed: {e}"),
            );
        }
        if let Err(e) = list_campaigns_inner(&app, &store_state) {
            log_op(
                &app,
                "warn",
                "perf",
                &format!("cache warm campaigns failed: {e}"),
            );
        }
        let elapsed = started.elapsed();
        if elapsed > std::time::Duration::from_millis(500) {
            log_op(
                &app,
                "warn",
                "perf",
                &format!("cache warm took {elapsed:?}"),
            );
        }
    });
}

fn legacy_roaming_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?
        // Roaming parent: `<base>/StarVault/CCM` → strip to `%APPDATA%`.
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "cannot resolve roaming profile dir".to_string())
}

/// Detect an old SC2CCM install under the OS roaming dir (P2 migration).
#[tauri::command]
pub fn detect_legacy_ccm(app: AppHandle) -> Result<Option<LegacyCcmInstall>, String> {
    Ok(LegacyCcmInstall::detect(legacy_roaming_dir(&app)?))
}

// --- import wizard (K2) -----------------------------------------------------

/// One in-flight import: extracted tree plus its cancel flag.
pub struct ImportOp {
    pub extracted_dir: PathBuf,
    pub cancel: Arc<AtomicBool>,
}

/// One SQLite connection for the process lifetime. Reopening the ledger
/// per command made Defender rescan the whole database file every time.
pub struct AppState {
    /// Shared ledger connection. Held behind an Option so Clear-all can
    /// drop it — Windows keeps deleted-but-open files locked.
    pub store: Mutex<Option<Arc<Store>>>,
    pub store_path: PathBuf,
    /// `<app-data>/config.toml`.
    pub config_path: PathBuf,
    /// In-flight imports: extracted tree + cancel flag per operation.
    pub import_ops: Mutex<HashMap<String, ImportOp>>,
    /// Parsed config; written by save_config.
    pub config_cache: Mutex<Option<Config>>,
    /// Campaign slots; invalidated like the library cache.
    pub campaigns_cache: Mutex<Option<Vec<CampaignSlot>>>,
}

impl AppState {
    /// The shared store, reopening if Clear-all dropped it.
    pub fn store(&self) -> Result<Arc<Store>, String> {
        let mut guard = self.store.lock().expect("store poisoned");
        if let Some(store) = guard.as_ref() {
            return Ok(store.clone());
        }
        let store = Arc::new(Store::open(&self.store_path).map_err(|e| e.to_string())?);
        *guard = Some(store.clone());
        Ok(store)
    }
}

/// Library scan cache; any mutation clears it so tab switches read
/// memory instead of re-walking the store.
#[derive(Default)]
pub struct LibraryCache {
    entries: Mutex<Option<Vec<LibraryEntry>>>,
}

fn invalidate_library(cache: &tauri::State<'_, LibraryCache>) {
    *cache.entries.lock().expect("library cache poisoned") = None;
}

fn invalidate_campaigns(state: &tauri::State<'_, AppState>) {
    *state
        .campaigns_cache
        .lock()
        .expect("campaigns cache poisoned") = None;
}

/// Minimum recorded level: 0=info, 1=warn, 2=error. Set from config.
static LOG_MIN_LEVEL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn log_startup(app: &AppHandle) {
    log_op(
        app,
        "info",
        "startup",
        &format!("StarVault CCM v{}", env!("CARGO_PKG_VERSION")),
    );
}

pub fn init_log_level(config_path: &PathBuf) {
    let level = Config::load(config_path)
        .map(|c| c.log_level)
        .unwrap_or_default();
    set_log_level(&level);
}

pub fn set_log_level(level: &str) {
    let rank = match level {
        "warn" => 1,
        "error" => 2,
        _ => 0,
    };
    LOG_MIN_LEVEL.store(rank, std::sync::atomic::Ordering::Relaxed);
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

/// User-confirmed metadata from the import wizard (K2).
#[derive(serde::Deserialize)]
pub struct ConfirmedMeta {
    pub title: Option<String>,
    pub desc: Option<String>,
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

/// A cross-slot dependency conflict (M5): what blocks an activation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConflictInfo {
    /// First conflicting `Mods\` path.
    pub target: String,
    /// Total number of conflicting paths.
    pub conflict_count: usize,
    /// Package already deployed.
    pub other_id: String,
    /// Faction the other package occupies.
    pub other_slot: String,
}

/// Typed command failure so the UI can branch on conflicts.
#[derive(Debug, serde::Serialize)]
pub struct CommandError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<ConflictInfo>,
}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self {
            message,
            conflict: None,
        }
    }
}

impl From<svccm_core::Error> for CommandError {
    fn from(e: svccm_core::Error) -> Self {
        Self {
            message: e.to_string(),
            conflict: None,
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
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
    store_state: tauri::State<'_, AppState>,
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
    store_state
        .import_ops
        .lock()
        .expect("import ops poisoned")
        .insert(
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
    store_state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
    op_id: String,
    id: String,
    slot: String,
    meta: Option<ConfirmedMeta>,
) -> Result<Option<String>, String> {
    let slot = slot_from_str(&slot)?;
    let (extracted_dir, cancel) = {
        let mut ops = store_state.import_ops.lock().expect("import ops poisoned");
        let op = ops
            .remove(&op_id)
            .ok_or_else(|| format!("no such import operation: {op_id}"))?;
        (op.extracted_dir, op.cancel)
    };

    let result = (|| {
        let mut plan = plan_from_extracted(&extracted_dir).map_err(|e| e.to_string())?;
        // K2: confirmed title/description win over detected ones.
        let (title, desc) = match &meta {
            Some(m) => (m.title.clone(), m.desc.clone()),
            None => (None, None),
        };
        if let Some(m) = plan.metadata.as_mut() {
            if title.is_some() {
                m.title = title;
            }
            if desc.is_some() {
                m.desc = desc;
            }
        } else if title.is_some() || desc.is_some() {
            plan.metadata = Some(fallback_metadata(title, desc));
        }
        let store = store_state.store()?;
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
    invalidate_library(&cache);
    invalidate_campaigns(&store_state);
    match &result {
        Ok(Some(rev)) => log_op(
            &app,
            "info",
            "import",
            &format!("{id}@{}", &rev[..8.min(rev.len())]),
        ),
        Ok(None) => log_op(&app, "warn", "import", &format!("{id} cancelled")),
        Err(e) => log_op(&app, "error", "import", &format!("{id}: {e}")),
    }
    result
}

/// Cancel an in-flight import at the next file boundary.
#[tauri::command]
pub fn import_cancel(store_state: tauri::State<'_, AppState>, op_id: String) {
    if let Some(op) = store_state
        .import_ops
        .lock()
        .expect("import ops poisoned")
        .get(&op_id)
    {
        op.cancel.store(true, Ordering::Relaxed);
    }
}

/// Wipe all app data: store, ledger, log, config. Confirmation happens in
/// the UI; this is unrecoverable.
#[tauri::command]
pub fn clear_all_data(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
) -> Result<(), String> {
    // Drop the shared ledger connection first: Windows refuses to delete
    // files that are still open.
    *state.store.lock().expect("store poisoned") = None;
    *state.config_cache.lock().expect("config poisoned") = None;
    *state.campaigns_cache.lock().expect("campaigns poisoned") = None;
    invalidate_library(&cache);

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?;
    if data_dir.symlink_metadata().is_ok() {
        remove_with_retry(&data_dir)?;
    }
    // The cache dir also hosts the WebView2 browser profile, which is locked
    // while the app runs — only remove our import scratch space inside it.
    let scratch = app
        .path()
        .app_cache_dir()
        .map_err(|e| format!("resolve cache dir: {e}"))?
        .join("import");
    if scratch.symlink_metadata().is_ok() {
        remove_with_retry(&scratch)?;
    }
    // Warm the now-empty caches so the next visit reflects reality.
    spawn_refresh(&app);
    Ok(())
}

/// Deletion can race short-lived handles (background warm-up, AV); a few
/// short retries absorb that.
fn remove_with_retry(dir: &std::path::Path) -> Result<(), String> {
    let mut last = None;
    for attempt in 0..3 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(200 * (attempt + 1)));
            }
        }
    }
    Err(format!(
        "clear {}: {}",
        dir.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

// --- campaigns screen --------------------------------------------------------

use svccm_core::config::{Config, StrategyChoice};
use svccm_core::layout::WindowsLayout;
use svccm_core::slots::SlotManager;

pub fn load_config(state: &AppState) -> Result<Config, String> {
    let mut cached = state.config_cache.lock().expect("config cache poisoned");
    if let Some(cfg) = cached.as_ref() {
        return Ok(cfg.clone());
    }
    let cfg = Config::load(&state.config_path).map_err(|e| e.to_string())?;
    *cached = Some(cfg.clone());
    Ok(cfg)
}

fn persist_config(state: &AppState, cfg: &Config) -> Result<(), String> {
    cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    *state.config_cache.lock().expect("config cache poisoned") = Some(cfg.clone());
    Ok(())
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
    pub log_level: String,
    pub save_isolation: bool,
    pub saves_profile: Option<String>,
}

#[tauri::command]
pub fn get_config(store_state: tauri::State<'_, AppState>) -> Result<ConfigDto, String> {
    let cfg = load_config(&store_state)?;
    Ok(ConfigDto {
        game_exe: cfg.game_exe.map(|p| p.display().to_string()),
        strategy_override: cfg.strategy_override.map(|s| match s {
            StrategyChoice::Junction => "junction".into(),
            StrategyChoice::Copy => "copy".into(),
        }),
        crash_reports_opt_in: cfg.crash_reports_opt_in,
        log_level: cfg.log_level.clone(),
        save_isolation: cfg.save_isolation,
        saves_profile: cfg.saves_profile.clone(),
    })
}

/// Save-isolation settings appended to save_config (keeps the arg count
/// under clippy's limit).
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigExtras {
    pub save_isolation: Option<bool>,
    pub saves_profile: Option<String>,
}

/// Persist settings; the game exe must exist when provided.
#[tauri::command]
pub async fn save_config(
    store_state: tauri::State<'_, AppState>,
    game_exe: Option<String>,
    strategy_override: Option<String>,
    crash_reports_opt_in: bool,
    log_level: Option<String>,
    extras: Option<ConfigExtras>,
) -> Result<(), String> {
    let mut cfg = load_config(&store_state)?;
    if let Some(extras) = extras {
        if let Some(on) = extras.save_isolation {
            cfg.save_isolation = on;
        }
        if let Some(profile) = extras.saves_profile {
            cfg.saves_profile = if profile.is_empty() {
                None
            } else {
                Some(profile)
            };
        }
    }
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
    if let Some(level) = log_level {
        if matches!(level.as_str(), "info" | "warn" | "error") {
            cfg.log_level = level;
        }
    }
    set_log_level(&cfg.log_level);
    crate::telemetry::set_enabled(cfg.crash_reports_opt_in);
    persist_config(&store_state, &cfg)
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
pub async fn list_campaigns(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
) -> Result<Vec<CampaignSlot>, String> {
    list_campaigns_inner(&app, &store_state)
}

fn list_campaigns_inner(
    app: &AppHandle,
    store_state: &tauri::State<'_, AppState>,
) -> Result<Vec<CampaignSlot>, String> {
    let started = std::time::Instant::now();
    let store = store_state.store()?;
    let t_ledger = std::time::Instant::now();
    let active_map: HashMap<String, (String, String)> = store
        .active_slots()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(slot, id, rev)| (slot, (id, rev)))
        .collect();
    let ledger_ms = t_ledger.elapsed().as_millis();
    let t_manifests = std::time::Instant::now();
    let slots: Vec<CampaignSlot> = SlotId::ALL
        .into_iter()
        .map(|slot| {
            let (title, pkg_id, rev, author, version) = match active_map.get(slot.as_str()) {
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
        .collect();
    let elapsed = started.elapsed();
    let manifests_ms = t_manifests.elapsed().as_millis();
    if elapsed > std::time::Duration::from_millis(200) {
        log_op(
            app,
            "warn",
            "perf",
            &format!(
                "list_campaigns took {elapsed:?} (ledger {ledger_ms}ms, manifests {manifests_ms}ms)"
            ),
        );
    }
    *store_state
        .campaigns_cache
        .lock()
        .expect("campaigns cache poisoned") = Some(slots.clone());
    Ok(slots)
}

/// Activate an installed package on a slot (K3: replaces whatever is there).
/// Cross-slot conflicts abort untouched; the error names both packages.
#[tauri::command]
pub async fn activate_campaign(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
    slot: String,
    id: String,
) -> Result<(), CommandError> {
    let slot = slot_from_str(&slot)?;
    let cfg = load_config(&store_state)?;
    let layout = layout_from_config(&cfg)?;
    let store = store_state.store()?;

    // Latest installed revision of `id`.
    let revs: Vec<String> = store
        .list_packages()
        .map_err(CommandError::from)?
        .into_iter()
        .filter(|(pid, _, _)| pid == &id)
        .map(|(_, rev, _)| rev)
        .collect();
    let rev = revs
        .last()
        .ok_or_else(|| CommandError::from(format!("package `{id}` is not installed")))?;

    // A campaign only loads through its own launcher, so the package's slot
    // is a fact, not a preference: enforce the binding at the boundary.
    let candidate = store
        .load_manifest(&id, rev)
        .map_err(|e| CommandError::from(e.to_string()))?;
    if candidate.slot != slot.as_str() {
        return Err(CommandError::from(format!(
            "`{id}` is built for the {} campaign and cannot go on {}",
            candidate.slot,
            slot.as_str()
        )));
    }

    // Save-set owner before this switch; captured before any mutation.
    let prev_save_owner = save_owner(&store, slot);

    // M5 pre-check with structured detail for the conflict dialog. The
    // authoritative block still happens inside `activate`.
    let mut would_deploy: Vec<PackageManifest> = Vec::new();
    let mut other_slots: HashMap<String, String> = HashMap::new();
    for (active_slot, active_id, active_rev) in store.active_slots().map_err(CommandError::from)? {
        if active_slot == slot.as_str() {
            continue; // being replaced
        }
        other_slots.insert(active_id.clone(), active_slot.clone());
        would_deploy.push(
            store
                .load_manifest(&active_id, &active_rev)
                .map_err(CommandError::from)?,
        );
    }
    would_deploy.push(candidate.clone());
    let refs: Vec<&PackageManifest> = would_deploy.iter().collect();
    let conflicts = store.find_conflicts(&refs);
    if let Some(first) = conflicts.first() {
        // The conflicting owner that is not the package being activated.
        let other_id = if first.second.0 == id {
            &first.first.0
        } else {
            &first.second.0
        };
        let other_slot = other_slots
            .get(other_id.as_str())
            .cloned()
            .unwrap_or_default()
            .to_string();
        log_op(
            &app,
            "warn",
            "activate",
            &format!("{id} blocked by {other_id} on {}", first.target),
        );
        return Err(CommandError {
            message: format!(
                "`{id}` conflicts with `{other_id}` on {} (+{} more)",
                first.target,
                conflicts.len() - 1
            ),
            conflict: Some(ConflictInfo {
                target: first.target.clone(),
                conflict_count: conflicts.len(),
                other_id: other_id.to_string(),
                other_slot,
            }),
        });
    }

    let manager = SlotManager::new(&layout, &store).with_strategy(cfg.strategy_override);
    if let Err(e) = manager.activate(slot, &id, rev) {
        log_op(
            &app,
            "error",
            "activate",
            &format!("{id} → {}: {e}", slot.as_str()),
        );
        return Err(CommandError::from(e.to_string()));
    }
    swap_saves(&app, &store, &cfg, slot, &id, &prev_save_owner);
    log_op(
        &app,
        "info",
        "activate",
        &format!("{id} → {}", slot.as_str()),
    );
    invalidate_library(&cache);
    invalidate_campaigns(&store_state);
    Ok(())
}

/// Return a slot to its plain Blizzard state.
#[tauri::command]
pub fn restore_campaign(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
    slot: String,
) -> Result<(), String> {
    let slot = slot_from_str(&slot)?;
    let cfg = load_config(&store_state)?;
    let layout = layout_from_config(&cfg)?;
    let store = store_state.store()?;
    let prev_save_owner = save_owner(&store, slot);
    if let Err(e) = SlotManager::new(&layout, &store).restore(slot) {
        log_op(&app, "error", "restore", &format!("{}: {e}", slot.as_str()));
        return Err(e.to_string());
    }
    swap_saves(&app, &store, &cfg, slot, "plain", &prev_save_owner);
    log_op(&app, "info", "restore", slot.as_str());
    invalidate_library(&cache);
    invalidate_campaigns(&store_state);
    Ok(())
}

// --- operation log -----------------------------------------------------------

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// Unix seconds.
    pub time: String,
    /// `info`, `warn`, or `error`.
    #[serde(default = "default_level")]
    pub level: String,
    pub kind: String,
    pub detail: String,
}

fn default_level() -> String {
    "info".into()
}

fn log_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))?
        .join("log.jsonl"))
}

/// Size-based rotation: 256 KiB per generation, two kept. Good hygiene
/// without a rotation crate.
const LOG_ROTATE_BYTES: u64 = 256 * 1024;
const LOG_GENERATIONS: usize = 2;

fn rotate_log_if_needed(path: &PathBuf) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < LOG_ROTATE_BYTES {
        return;
    }
    // Shift log.jsonl.N -> log.jsonl.N+1, oldest out; current -> .1.
    for gen in (1..LOG_GENERATIONS).rev() {
        let from = path.with_extension(format!("jsonl.{gen}"));
        let to = path.with_extension(format!("jsonl.{}", gen + 1));
        if from.symlink_metadata().is_ok() {
            let _ = std::fs::rename(&from, &to);
        }
    }
    let _ = std::fs::rename(path, path.with_extension("jsonl.1"));
}

fn log_op(app: &AppHandle, level: &str, kind: &str, detail: &str) {
    let rank = match level {
        "warn" => 1,
        "error" => 2,
        _ => 0,
    };
    if rank < LOG_MIN_LEVEL.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // Opt-in telemetry: every surfaced failure is captured (S3: as much
    // signal as the user allowed us to collect).
    if rank == 2 {
        crate::telemetry::capture(detail);
    }
    let entry = LogEntry {
        // ponytail: second-resolution UTC stamp from std only; chrono when the
        // log needs sub-second ordering or local timezone display.
        time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
            .to_string(),
        level: level.to_string(),
        kind: kind.to_string(),
        detail: detail.to_string(),
    };
    let Ok(path) = log_path(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    rotate_log_if_needed(&path);
    if let Ok(mut json) = serde_json::to_string(&entry) {
        json.push('\n');
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(json.as_bytes());
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
pub async fn reconcile(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let cfg = load_config(&store_state)?;
    if cfg.game_exe.is_none() {
        return Ok(Vec::new()); // nothing to reconcile without a game install
    }
    let layout = layout_from_config(&cfg)?;
    let store = store_state.store()?;
    match SlotManager::new(&layout, &store)
        .with_strategy(cfg.strategy_override)
        .reconcile()
    {
        Ok(notes) => {
            for note in &notes {
                log_op(&app, "warn", "reconcile", note);
                // Repairs touch slots; drop cached views.
                invalidate_campaigns(&store_state);
            }
            Ok(notes)
        }
        Err(e) => {
            // Surface what failed and where — reconcile errors lose their
            // path context through `to_string` alone.
            log_op(&app, "error", "reconcile", &format!("{e:#}"));
            Err(e.to_string())
        }
    }
}

// --- launch (X1) -------------------------------------------------------------

use svccm_core::launch::{self, PreflightReport};

/// Verify-only pre-flight over the configured game install.
#[tauri::command]
pub fn launch_preflight(
    _app: AppHandle,
    store_state: tauri::State<'_, AppState>,
) -> Result<PreflightReport, String> {
    let cfg = load_config(&store_state)?;
    let layout = layout_from_config(&cfg)?;
    let store = store_state.store()?;
    Ok(launch::preflight(&layout, &store))
}

/// Detached spawn of the game executable.
#[tauri::command]
pub fn launch_game(app: AppHandle, store_state: tauri::State<'_, AppState>) -> Result<(), String> {
    let cfg = load_config(&store_state)?;
    let layout = layout_from_config(&cfg)?;
    launch::launch(&layout)
        .map(|()| log_op(&app, "info", "launch", "game started"))
        .map_err(|e| e.to_string())
}

/// One-click play from the Library: restore every active faction to plain,
/// activate `id` on its faction, then launch the game. Resetting first
/// guarantees a clean Mods\\ union, so no conflict is possible.
#[tauri::command]
pub async fn launch_package(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
    id: String,
) -> Result<(), CommandError> {
    let cfg = load_config(&store_state)?;
    let layout = layout_from_config(&cfg)?;
    let store = store_state.store()?;

    let revs: Vec<String> = store
        .list_packages()
        .map_err(CommandError::from)?
        .into_iter()
        .filter(|(pid, _, _)| pid == &id)
        .map(|(_, rev, _)| rev)
        .collect();
    let rev = revs
        .last()
        .ok_or_else(|| CommandError::from(format!("package `{id}` is not installed")))?;
    let candidate = store
        .load_manifest(&id, rev)
        .map_err(|e| CommandError::from(e.to_string()))?;
    let slot = slot_from_str(&candidate.slot)?;

    let manager = SlotManager::new(&layout, &store).with_strategy(cfg.strategy_override);
    let mut prev_owners = Vec::new();
    for (active_slot, active_id, _) in store.active_slots().map_err(CommandError::from)? {
        let s = slot_from_str(&active_slot)?;
        prev_owners.push((s, active_id));
        manager
            .restore(s)
            .map_err(|e| CommandError::from(e.to_string()))?;
    }
    manager
        .activate(slot, &id, rev)
        .map_err(|e| CommandError::from(e.to_string()))?;
    for (s, prev) in &prev_owners {
        swap_saves(&app, &store, &cfg, *s, "plain", prev);
    }
    let prev_owner = prev_owners
        .iter()
        .find(|(s, _)| *s == slot)
        .map(|(_, o)| o.clone())
        .unwrap_or_else(|| "plain".into());
    swap_saves(&app, &store, &cfg, slot, &id, &prev_owner);
    launch::launch(&layout).map_err(|e| CommandError::from(e.to_string()))?;

    log_op(
        &app,
        "info",
        "launch",
        &format!(
            "play {id}: factions reset, activated on {}, game launched",
            slot.as_str()
        ),
    );
    invalidate_library(&cache);
    invalidate_campaigns(&store_state);
    Ok(())
}

/// Battle.net deep link when the local exe is unusable.
#[tauri::command]
pub fn launch_battlenet(app: AppHandle) -> Result<(), String> {
    launch::launch_battlenet()
        .map(|()| log_op(&app, "info", "launch", "battlenet:// fallback"))
        .map_err(|e| e.to_string())
}

// --- migration (P2) ----------------------------------------------------------

use svccm_core::library::MigrationCandidate;

/// Custom campaign directories an old SC2CCM install left in Maps\Campaign.
#[tauri::command]
pub async fn list_migration_candidates(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
) -> Result<Vec<MigrationCandidate>, String> {
    // Walking candidate trees is expensive under real-time AV; only bother
    // when an old install was actually detected.
    if LegacyCcmInstall::detect(legacy_roaming_dir(&app)?).is_none() {
        return Ok(Vec::new());
    }
    let cfg = load_config(&store_state)?;
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
    store_state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
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
    let store = store_state.store()?;
    let rev = store
        .ingest_with_progress(&id, slot, &plan, |_| true)
        .map_err(|e| e.to_string())?
        .ok_or("migration cancelled")?;
    log_op(&app, "info", "migrate", &format!("{id} from {path}"));
    invalidate_library(&cache);
    invalidate_campaigns(&store_state);
    Ok(rev)
}

// --- install discovery -------------------------------------------------------

/// Best-effort SC2 install detection (registry, then well-known folders).
#[tauri::command]
pub fn discover_game_exe() -> Option<String> {
    svccm_core::layout::discover_install().map(|p| p.display().to_string())
}

/// Open the package's content folder in Explorer: the store's deploy tree
/// when one exists (that is what the game reads through the junction), else
/// the game's faction directory. Returns the path opened.
#[tauri::command]
pub fn reveal_package(
    store_state: tauri::State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let store = store_state.store()?;
    let revs: Vec<String> = store
        .list_packages()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|(pid, _, _)| pid == &id)
        .map(|(_, rev, _)| rev)
        .collect();
    let rev = revs
        .last()
        .ok_or_else(|| format!("package `{id}` is not installed"))?
        .clone();
    let manifest = store.load_manifest(&id, &rev).map_err(|e| e.to_string())?;

    let deploy = store
        .root()
        .join("deploy")
        .join(format!("{}-{}", manifest.slot, manifest.rev));
    let target = if deploy.is_dir() {
        deploy
    } else {
        let cfg = load_config(&store_state)?;
        let layout = layout_from_config(&cfg)?;
        let slot = slot_from_str(&manifest.slot)?;
        layout.slot_dir(slot)
    };

    #[cfg(windows)]
    let status = std::process::Command::new("explorer").arg(&target).spawn();
    #[cfg(not(windows))]
    let status = std::process::Command::new("xdg-open").arg(&target).spawn();
    status.map_err(|e| format!("open {}: {e}", target.display()))?;
    Ok(target.display().to_string())
}

/// Remove an installed package (refuses while active on a faction).
#[tauri::command]
pub fn remove_package(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
    cache: tauri::State<'_, LibraryCache>,
    id: String,
) -> Result<(), String> {
    let store = store_state.store()?;
    store.remove_package(&id).map_err(|e| e.to_string())?;
    // Save sets belong to the package; reclaim them with it.
    if let Some(live) = live_saves(&app, &load_config(&store_state)?).ok().flatten() {
        let mgr = SavesManager::new(live, store.root());
        let removed = mgr.remove_sets(&id);
        if removed > 0 {
            log_op(
                &app,
                "info",
                "saves",
                &format!("removed {removed} save set(s) of {id}"),
            );
        }
    }
    log_op(&app, "info", "remove", &id);
    invalidate_library(&cache);
    invalidate_campaigns(&store_state);
    Ok(())
}

// --- changelog ---------------------------------------------------------------

/// The changelog embedded at build time (crates/app/../../CHANGELOG.md).
#[tauri::command]
pub fn changelog() -> String {
    include_str!("../../../CHANGELOG.md").to_string()
}

// --- save isolation ----------------------------------------------------------

use svccm_core::saves::{discover, saves_dir as resolve_saves_dir, SavesManager};

#[derive(serde::Serialize)]
pub struct SavesStatus {
    pub supported: bool,
    pub reason: Option<String>,
    /// Discovered profiles as `<account>/<profile>`.
    pub profiles: Vec<String>,
    pub selected: Option<String>,
    pub enabled: bool,
}

fn documents_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .document_dir()
        .map_err(|e| format!("resolve Documents: {e}"))
}

/// Resolve the live Saves dir for the configured profile.
fn live_saves(app: &AppHandle, cfg: &Config) -> Result<Option<PathBuf>, String> {
    let docs = documents_dir(app)?;
    if svccm_core::saves::is_onedrive(&docs) {
        // Hard stop even if a stale profile was persisted before the check
        // existed: never swap inside an OneDrive-managed tree.
        return Ok(None);
    }
    let single_profile = {
        let mut found = discover(&docs).into_iter();
        match (found.next(), found.next()) {
            (Some(only), None) => Some(only.id),
            _ => None,
        }
    };
    let profile = cfg.saves_profile.clone().or(single_profile);
    Ok(profile.as_deref().and_then(|p| resolve_saves_dir(&docs, p)))
}

/// Saves tab state for Settings: support check, profiles, current choice.
#[tauri::command]
pub async fn get_saves_status(
    app: AppHandle,
    store_state: tauri::State<'_, AppState>,
) -> Result<SavesStatus, String> {
    let cfg = load_config(&store_state)?;
    let docs = documents_dir(&app)?;
    if svccm_core::saves::is_onedrive(&docs) {
        return Ok(SavesStatus {
            supported: false,
            reason: Some(
                "Documents is OneDrive-managed — save isolation is not supported there yet.".into(),
            ),
            profiles: Vec::new(),
            selected: cfg.saves_profile.clone(),
            enabled: cfg.save_isolation,
        });
    }
    let profiles: Vec<String> = discover(&docs).into_iter().map(|p| p.id).collect();
    // Exactly one profile and none persisted: default to it so isolation
    // works without a picker (the common single-account case).
    let selected = match (&cfg.saves_profile, profiles.len()) {
        (None, 1) => profiles.first().cloned(),
        (selected, _) => selected.clone(),
    };
    Ok(SavesStatus {
        supported: !profiles.is_empty(),
        reason: if profiles.is_empty() {
            Some("No StarCraft II save profile found (launch the game once first).".into())
        } else {
            None
        },
        profiles,
        selected,
        enabled: cfg.save_isolation,
    })
}

/// Swap a faction's save-set after a slot change. Best-effort: failures are
/// logged and surfaced, but never block the campaign switch itself.
fn swap_saves(
    app: &AppHandle,
    store: &svccm_core::store::Store,
    cfg: &Config,
    slot: SlotId,
    new_owner: &str,
    prev_owner: &str,
) {
    if !cfg.save_isolation {
        return;
    }
    let Some(live) = live_saves(app, cfg).ok().flatten() else {
        return;
    };
    let mgr = SavesManager::new(live, store.root());
    match mgr.swap(slot, new_owner, prev_owner) {
        Ok(notes) => {
            for note in notes {
                log_op(app, "info", "saves", &note);
            }
        }
        Err(e) => log_op(app, "error", "saves", &format!("{slot:?} swap: {e}")),
    }
}

/// The current save-set owner for a slot: its active package, or "plain".
fn save_owner(store: &svccm_core::store::Store, slot: SlotId) -> String {
    store
        .active_slots()
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|(s, _, _)| s == slot.as_str())
                .map(|(_, id, _)| id)
        })
        .unwrap_or_else(|| "plain".into())
}

/// Empty the operation log (and its rotated generations).
#[tauri::command]
pub fn clear_log(app: AppHandle) -> Result<(), String> {
    let path = log_path(&app)?;
    for p in [
        path.clone(),
        path.with_extension("jsonl.1"),
        path.with_extension("jsonl.2"),
    ] {
        if p.symlink_metadata().is_ok() {
            std::fs::remove_file(&p).map_err(|e| format!("clear {}: {e}", p.display()))?;
        }
    }
    Ok(())
}
