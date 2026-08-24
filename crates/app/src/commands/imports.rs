use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use svccm_core::layout::SlotId;
use svccm_core::package::import::{
    extract_archive, preview_plan, ImportOperationSnapshot, ImportOperationState, ImportProgress,
};
use svccm_core::package::metadata::LegacyMetadata;
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::PackageId;
use tauri::{AppHandle, Emitter};

use super::{ensure_game_stopped, AppState, CommandResult};

const CANCELLATION_WAIT_ATTEMPTS: usize = 200;
const CANCELLATION_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

pub(super) struct ImportOp {
    scratch: PathBuf,
    cancel: Arc<AtomicBool>,
    snapshot: ImportOperationSnapshot,
    cleanup_started: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ProgressEvent {
    op_id: String,
    phase: &'static str,
    files_done: u64,
    files_total: u64,
    current_file: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConfirmedMeta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    pub desc: Option<String>,
}

fn validate_operation_id(operation_id: &str) -> svccm_core::error::Result<()> {
    if !svccm_core::filesystem::is_safe_operation_id(operation_id) {
        return Err(svccm_core::error::user_err(
            "invalid_import_operation_id",
            "import operation id must be a bounded ASCII token",
        ));
    }
    Ok(())
}

fn lock_ops(
    operations: &Mutex<std::collections::HashMap<String, ImportOp>>,
) -> svccm_core::error::Result<std::sync::MutexGuard<'_, std::collections::HashMap<String, ImportOp>>>
{
    operations.lock().map_err(|_| {
        svccm_core::error::internal_err(
            "import_registry_poisoned",
            "StarVault could not access the import operation",
            "import operation mutex was poisoned",
        )
    })
}

fn emit_progress(
    app: &AppHandle,
    operation_id: &str,
    phase: &'static str,
    progress: ImportProgress,
) {
    let _ = app.emit(
        "import-progress",
        ProgressEvent {
            op_id: operation_id.into(),
            phase,
            files_done: progress.files_done,
            files_total: progress.files_total,
            current_file: progress.current_file,
        },
    );
}

fn is_regular_archive_source(metadata: &std::fs::Metadata) -> bool {
    !svccm_core::filesystem::is_link_or_reparse(metadata)
        && metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
}

fn cleanup_scratch(path: &Path) -> svccm_core::error::Result<()> {
    if let Some(parent) = path.parent() {
        super::validate_regular_directory(parent, "unsafe_import_scratch")?;
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(svccm_core::error::user_path_err(
                "inspect_import_scratch",
                error.to_string(),
                path,
                true,
            ));
        }
    };
    if svccm_core::filesystem::is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(svccm_core::error::user_path_err(
            "unsafe_import_scratch",
            "refusing to remove a linked or non-directory import scratch path",
            path,
            false,
        ));
    }
    validate_scratch_tree(path)?;
    std::fs::remove_dir_all(path).map_err(|error| {
        svccm_core::error::user_path_err("cleanup_import_scratch", error.to_string(), path, true)
    })
}

fn validate_scratch_tree(root: &Path) -> svccm_core::error::Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|error| {
            svccm_core::error::user_path_err(
                "inspect_import_scratch",
                error.to_string(),
                &directory,
                true,
            )
        })? {
            let entry = entry.map_err(|error| {
                svccm_core::error::user_path_err(
                    "inspect_import_scratch",
                    error.to_string(),
                    &directory,
                    true,
                )
            })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                svccm_core::error::user_path_err(
                    "inspect_import_scratch",
                    error.to_string(),
                    &path,
                    true,
                )
            })?;
            if svccm_core::filesystem::is_link_or_reparse(&metadata) {
                return Err(svccm_core::error::user_path_err(
                    "unsafe_import_scratch",
                    "refusing to remove import scratch data containing a link or junction",
                    &path,
                    false,
                ));
            }
            if metadata.is_dir() {
                pending.push(path);
            } else if !metadata.is_file() {
                return Err(svccm_core::error::user_path_err(
                    "unsafe_import_scratch",
                    "refusing to remove import scratch data containing a special filesystem entry",
                    &path,
                    false,
                ));
            }
        }
    }
    Ok(())
}

fn ensure_import_root(path: &Path) -> svccm_core::error::Result<()> {
    validate_existing_import_ancestors(path)?;
    if let Some(parent) = path.parent() {
        match std::fs::symlink_metadata(parent) {
            Ok(_) => super::validate_regular_directory(parent, "unsafe_import_root")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(parent).map_err(|error| {
                    svccm_core::error::user_path_err(
                        "create_import_root",
                        error.to_string(),
                        parent,
                        true,
                    )
                })?;
                validate_existing_import_ancestors(path)?;
                super::validate_regular_directory(parent, "unsafe_import_root")?;
            }
            Err(error) => {
                return Err(svccm_core::error::user_path_err(
                    "inspect_import_root",
                    error.to_string(),
                    parent,
                    true,
                ));
            }
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => super::validate_regular_directory(path, "unsafe_import_root"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(svccm_core::error::user_path_err(
                        "create_import_root",
                        error.to_string(),
                        path,
                        true,
                    ));
                }
            }
            validate_existing_import_ancestors(path)?;
            super::validate_regular_directory(path, "unsafe_import_root")
        }
        Err(error) => Err(svccm_core::error::user_path_err(
            "inspect_import_root",
            error.to_string(),
            path,
            true,
        )),
    }
}

fn validate_existing_import_ancestors(path: &Path) -> svccm_core::error::Result<()> {
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata)
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && !svccm_core::filesystem::is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(svccm_core::error::user_path_err(
                    "unsafe_import_root",
                    "refusing to use an import root below a linked or non-directory ancestor",
                    ancestor,
                    false,
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(svccm_core::error::user_path_err(
                    "inspect_import_root",
                    error.to_string(),
                    ancestor,
                    true,
                ));
            }
        }
    }
    Ok(())
}

async fn register_analysis(
    state: &AppState,
    operation_id: &str,
    scratch: &Path,
    cancel: Arc<AtomicBool>,
) -> svccm_core::error::Result<()> {
    // Clear-all holds this lock from its cancellation sweep through deletion.
    // A short admission section ensures it either observes this operation or
    // this operation starts only after the clear has completed.
    let _admission = state.mutation.lock().await;
    if scratch.symlink_metadata().is_ok() {
        return Err(svccm_core::error::user_path_err(
            "import_scratch_collision",
            "an import operation already uses this id",
            scratch,
            false,
        ));
    }
    ensure_import_root(&state.import_root)?;
    let mut operations = lock_ops(&state.import_ops)?;
    if operations.contains_key(operation_id) {
        return Err(svccm_core::error::user_err(
            "import_operation_exists",
            "an import operation already uses this id",
        ));
    }
    operations.insert(
        operation_id.to_string(),
        ImportOp {
            scratch: scratch.to_path_buf(),
            cancel,
            snapshot: ImportOperationSnapshot {
                op_id: operation_id.to_string(),
                state: ImportOperationState::Analyzing,
                preview: None,
                revision: None,
                error_code: None,
            },
            cleanup_started: false,
        },
    );
    Ok(())
}

async fn cleanup_claimed(
    state: &AppState,
    operation_id: &str,
    scratch: PathBuf,
) -> svccm_core::error::Result<()> {
    let cleanup =
        match tauri::async_runtime::spawn_blocking(move || cleanup_scratch(&scratch)).await {
            Ok(cleanup) => cleanup,
            Err(error) => {
                if let Some(operation) = lock_ops(&state.import_ops)?.get_mut(operation_id) {
                    operation.cleanup_started = false;
                }
                return Err(svccm_core::error::internal_err(
                    "import_cleanup_worker_failed",
                    "StarVault could not clean up the import operation",
                    error.to_string(),
                ));
            }
        };
    let mut operations = lock_ops(&state.import_ops)?;
    if cleanup.is_ok() {
        operations.remove(operation_id);
    } else if let Some(operation) = operations.get_mut(operation_id) {
        // Keep the cancellation token addressable until a later cleanup retry.
        operation.cleanup_started = false;
    }
    cleanup
}

async fn finish_operation(
    state: &AppState,
    operation_id: &str,
    terminal_state: ImportOperationState,
    revision: Option<String>,
    error_code: Option<String>,
) -> svccm_core::error::Result<()> {
    let scratch = {
        let mut operations = lock_ops(&state.import_ops)?;
        let operation = operations.get_mut(operation_id).ok_or_else(|| {
            svccm_core::error::user_err(
                "import_operation_not_found",
                "the import operation is no longer available",
            )
        })?;
        operation.snapshot.state = terminal_state;
        operation.snapshot.revision = revision;
        operation.snapshot.error_code = error_code;
        if operation.cleanup_started {
            None
        } else {
            operation.cleanup_started = true;
            Some(operation.scratch.clone())
        }
    };
    if let Some(scratch) = scratch {
        cleanup_claimed(state, operation_id, scratch).await
    } else {
        Ok(())
    }
}

fn combine_cleanup_error(
    original: svccm_core::Error,
    cleanup: svccm_core::Error,
) -> svccm_core::Error {
    svccm_core::error::internal_err(
        "import_terminal_cleanup_failed",
        "StarVault could not clean up the import operation",
        format!(
            "operation failed: {}; cleanup failed: {}",
            original.diagnostic(),
            cleanup.diagnostic()
        ),
    )
}

#[tauri::command]
pub async fn import_analyze(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    op_id: String,
    path: String,
) -> CommandResult<ImportOperationSnapshot> {
    super::error::map(
        &app,
        &state,
        "import_analyze",
        validate_operation_id(&op_id),
    )?;
    let archive = PathBuf::from(path);
    let archive_metadata = std::fs::symlink_metadata(&archive).map_err(|error| {
        super::error::report(
            &app,
            &state,
            "import_analyze",
            svccm_core::error::user_path_err(
                "archive_not_found",
                error.to_string(),
                &archive,
                false,
            ),
        )
    })?;
    if !is_regular_archive_source(&archive_metadata) {
        return Err(super::error::report(
            &app,
            &state,
            "import_analyze",
            svccm_core::error::user_path_err(
                "invalid_archive_source",
                "select a regular ZIP archive",
                archive,
                false,
            ),
        ));
    }
    let scratch = state.import_root.join(&op_id);
    let cancel = Arc::new(AtomicBool::new(false));
    super::error::map(
        &app,
        &state,
        "import_analyze",
        register_analysis(&state, &op_id, &scratch, cancel.clone()).await,
    )?;

    let worker_app = app.clone();
    let worker_id = op_id.clone();
    let worker_scratch = scratch.clone();
    let archive_name = archive
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned());
    let result = super::error::blocking(&app, &state, "import_analyze", move || {
        let completed = extract_archive(&archive, &worker_scratch, |progress| {
            emit_progress(&worker_app, &worker_id, "extract", progress);
            !cancel.load(Ordering::Relaxed)
        })?;
        if !completed {
            return Ok(None);
        }
        let plan = plan_from_extracted(&worker_scratch)?;
        Ok(Some(preview_plan(&plan, archive_name.as_deref())))
    })
    .await;

    match result {
        Ok(Some(preview)) => {
            let (cancelled, snapshot) = {
                let mut operations = lock_ops(&state.import_ops)
                    .map_err(|error| super::error::report(&app, &state, "import_analyze", error))?;
                let Some(operation) = operations.get_mut(&op_id) else {
                    return Err(super::error::report(
                        &app,
                        &state,
                        "import_analyze",
                        svccm_core::error::internal_err(
                            "import_operation_lost",
                            "StarVault lost the import operation",
                            "operation disappeared before analysis completed",
                        ),
                    ));
                };
                let cancelled = operation.cancel.load(Ordering::Relaxed);
                if !cancelled {
                    operation.snapshot.state = ImportOperationState::Ready;
                    operation.snapshot.preview = Some(preview.clone());
                }
                (cancelled, operation.snapshot.clone())
            };
            if cancelled {
                let cleanup =
                    finish_operation(&state, &op_id, ImportOperationState::Cancelled, None, None)
                        .await;
                return match cleanup {
                    Ok(()) => Err(super::error::report(
                        &app,
                        &state,
                        "import_analyze",
                        svccm_core::error::user_err(
                            "import_cancelled",
                            "package analysis was cancelled",
                        ),
                    )),
                    Err(error) => Err(super::error::report(&app, &state, "import_analyze", error)),
                };
            }
            Ok(snapshot)
        }
        Ok(None) => {
            let cleanup =
                finish_operation(&state, &op_id, ImportOperationState::Cancelled, None, None).await;
            match cleanup {
                Ok(()) => Err(super::error::report(
                    &app,
                    &state,
                    "import_analyze",
                    svccm_core::error::user_err(
                        "import_cancelled",
                        "package analysis was cancelled",
                    ),
                )),
                Err(error) => Err(super::error::report(&app, &state, "import_analyze", error)),
            }
        }
        Err(command_error) => {
            let original = svccm_core::error::user_err(
                command_error.code.clone(),
                command_error.message.clone(),
            );
            match finish_operation(
                &state,
                &op_id,
                ImportOperationState::Failed,
                None,
                Some(command_error.code.clone()),
            )
            .await
            {
                Ok(()) => Err(command_error),
                Err(cleanup) => Err(super::error::report(
                    &app,
                    &state,
                    "import_analyze",
                    combine_cleanup_error(original, cleanup),
                )),
            }
        }
    }
}

fn apply_confirmed_metadata(
    plan: &mut svccm_core::package::normalize::PackagePlan,
    meta: ConfirmedMeta,
) {
    let metadata = plan.metadata.get_or_insert_with(LegacyMetadata::default);
    metadata.title = meta.title;
    metadata.author = meta.author;
    metadata.version = meta.version;
    metadata.desc = meta.desc;
}

fn claim_ready_ingest(
    operations: &Mutex<std::collections::HashMap<String, ImportOp>>,
    operation_id: &str,
    ensure_stopped: impl FnOnce() -> svccm_core::error::Result<()>,
) -> svccm_core::error::Result<(PathBuf, Arc<AtomicBool>)> {
    // Check the process state before claiming the operation. Changing Ready to
    // Ingesting makes cancellation wait for a worker, so it counts as mutation.
    ensure_stopped()?;
    let mut operations = lock_ops(operations)?;
    let operation = operations.get_mut(operation_id).ok_or_else(|| {
        svccm_core::error::user_err(
            "import_operation_not_found",
            "analyze the package again before importing it",
        )
    })?;
    if operation.snapshot.state != ImportOperationState::Ready {
        return Err(svccm_core::error::user_err(
            "import_operation_not_ready",
            "the import operation is not ready to ingest",
        ));
    }
    operation.snapshot.state = ImportOperationState::Ingesting;
    Ok((operation.scratch.clone(), operation.cancel.clone()))
}

#[tauri::command]
pub async fn import_ingest(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    op_id: String,
    id: String,
    slot: String,
    meta: Option<ConfirmedMeta>,
) -> CommandResult<ImportOperationSnapshot> {
    super::error::map(&app, &state, "import_ingest", validate_operation_id(&op_id))?;
    let package_id = super::error::map(&app, &state, "import_ingest", PackageId::parse(id))?;
    let faction = super::error::map(&app, &state, "import_ingest", slot.parse::<SlotId>())?;
    let _mutation = state.mutation.lock().await;
    let (scratch, cancel) = super::error::map(
        &app,
        &state,
        "import_ingest",
        claim_ready_ingest(&state.import_ops, &op_id, ensure_game_stopped),
    )?;
    let store = super::error::map(&app, &state, "import_ingest", state.store())?;
    let log_id = package_id.to_string();
    let worker_app = app.clone();
    let worker_id = op_id.clone();
    let result = super::error::blocking(&app, &state, "import_ingest", move || {
        let mut plan = plan_from_extracted(&scratch)?;
        if let Some(meta) = meta {
            apply_confirmed_metadata(&mut plan, meta);
        }
        store.ingest_with_progress(&package_id, faction, &plan, |progress| {
            emit_progress(&worker_app, &worker_id, "ingest", progress);
            !cancel.load(Ordering::Relaxed)
        })
    })
    .await;

    match result {
        Ok(revision) => {
            let terminal = if revision.is_some() {
                ImportOperationState::Completed
            } else {
                ImportOperationState::Cancelled
            };
            let snapshot = ImportOperationSnapshot {
                op_id: op_id.clone(),
                state: terminal,
                preview: None,
                revision: revision.clone(),
                error_code: None,
            };
            let cleanup =
                finish_operation(&state, &op_id, snapshot.state, revision.clone(), None).await;
            match cleanup {
                Ok(()) => {
                    if let Some(revision) = &revision {
                        super::log::log_op(
                            &app,
                            "info",
                            "import",
                            &format!("{}@{}", log_id, short(revision)),
                        );
                    }
                    crate::analytics::track(
                        &app,
                        "package_installed",
                        &[("package", log_id.clone()), ("slot", slot.clone())],
                    );
                    Ok(snapshot)
                }
                Err(error) => Err(super::error::report(&app, &state, "import_ingest", error)),
            }
        }
        Err(command_error) => {
            let original = svccm_core::error::user_err(
                command_error.code.clone(),
                command_error.message.clone(),
            );
            match finish_operation(
                &state,
                &op_id,
                ImportOperationState::Failed,
                None,
                Some(command_error.code.clone()),
            )
            .await
            {
                Ok(()) => Err(command_error),
                Err(cleanup) => Err(super::error::report(
                    &app,
                    &state,
                    "import_ingest",
                    combine_cleanup_error(original, cleanup),
                )),
            }
        }
    }
}

fn short(revision: &str) -> &str {
    &revision[..revision.len().min(12)]
}

fn claim_immediate_cancel(
    state: &AppState,
    operation_id: &str,
) -> svccm_core::error::Result<Option<PathBuf>> {
    let mut operations = lock_ops(&state.import_ops)?;
    let Some(operation) = operations.get_mut(operation_id) else {
        return Ok(None);
    };
    operation.cancel.store(true, Ordering::Relaxed);
    if matches!(
        operation.snapshot.state,
        ImportOperationState::Analyzing | ImportOperationState::Ingesting
    ) || operation.cleanup_started
    {
        return Ok(None);
    }
    operation.snapshot.state = ImportOperationState::Cancelled;
    operation.cleanup_started = true;
    Ok(Some(operation.scratch.clone()))
}

enum CleanupWait {
    Removed,
    Claimed(PathBuf),
}

fn wait_for_cleanup_claim(
    operations: &Mutex<std::collections::HashMap<String, ImportOp>>,
    operation_id: &str,
) -> svccm_core::error::Result<CleanupWait> {
    for _ in 0..CANCELLATION_WAIT_ATTEMPTS {
        {
            let mut operations = lock_ops(operations)?;
            let Some(operation) = operations.get_mut(operation_id) else {
                return Ok(CleanupWait::Removed);
            };
            if !matches!(
                operation.snapshot.state,
                ImportOperationState::Analyzing | ImportOperationState::Ingesting
            ) && !operation.cleanup_started
            {
                operation.cleanup_started = true;
                return Ok(CleanupWait::Claimed(operation.scratch.clone()));
            }
        }
        std::thread::sleep(CANCELLATION_WAIT_INTERVAL);
    }
    Err(svccm_core::error::user_err(
        "import_cancellation_timed_out",
        "the import worker did not finish cleanup; retry after it stops",
    ))
}

async fn wait_for_operation_cleanup(
    state: &AppState,
    operation_id: &str,
) -> svccm_core::error::Result<()> {
    let operations = state.import_ops.clone();
    let worker_id = operation_id.to_string();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        wait_for_cleanup_claim(&operations, &worker_id)
    })
    .await
    .map_err(|error| {
        svccm_core::error::internal_err(
            "import_wait_worker_failed",
            "StarVault could not wait for import cancellation",
            error.to_string(),
        )
    })??;
    match outcome {
        CleanupWait::Removed => Ok(()),
        CleanupWait::Claimed(scratch) => cleanup_claimed(state, operation_id, scratch).await,
    }
}

#[tauri::command]
pub async fn import_cancel(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    op_id: String,
) -> CommandResult<()> {
    super::error::map(&app, &state, "import_cancel", validate_operation_id(&op_id))?;
    let scratch = super::error::map(
        &app,
        &state,
        "import_cancel",
        claim_immediate_cancel(&state, &op_id),
    )?;
    if let Some(scratch) = scratch {
        super::error::map(
            &app,
            &state,
            "import_cancel",
            cleanup_claimed(&state, &op_id, scratch).await,
        )?;
    }
    super::error::map(
        &app,
        &state,
        "import_cancel",
        wait_for_operation_cleanup(&state, &op_id).await,
    )
}

pub(super) async fn cancel_all(state: &AppState) -> svccm_core::error::Result<()> {
    let immediate = {
        let mut operations = lock_ops(&state.import_ops)?;
        operations
            .iter_mut()
            .filter_map(|(id, operation)| {
                operation.cancel.store(true, Ordering::Relaxed);
                if matches!(
                    operation.snapshot.state,
                    ImportOperationState::Analyzing | ImportOperationState::Ingesting
                ) || operation.cleanup_started
                {
                    return None;
                }
                operation.snapshot.state = ImportOperationState::Cancelled;
                operation.cleanup_started = true;
                Some((id.clone(), operation.scratch.clone()))
            })
            .collect::<Vec<_>>()
    };
    let claimed_ids = immediate
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let outcomes = match tauri::async_runtime::spawn_blocking(move || {
        immediate
            .into_iter()
            .map(|(id, scratch)| {
                let cleanup = cleanup_scratch(&scratch);
                (id, cleanup)
            })
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(outcomes) => outcomes,
        Err(error) => {
            let mut operations = lock_ops(&state.import_ops)?;
            for id in claimed_ids {
                if let Some(operation) = operations.get_mut(&id) {
                    operation.cleanup_started = false;
                }
            }
            return Err(svccm_core::error::internal_err(
                "import_cleanup_worker_failed",
                "StarVault could not clean up import operations",
                error.to_string(),
            ));
        }
    };
    let cleanup_error = {
        let mut operations = lock_ops(&state.import_ops)?;
        let mut first_error = None;
        for (id, cleanup) in outcomes {
            match cleanup {
                Ok(()) => {
                    operations.remove(&id);
                }
                Err(error) => {
                    if let Some(operation) = operations.get_mut(&id) {
                        operation.cleanup_started = false;
                    }
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        first_error
    };
    if let Some(error) = cleanup_error {
        return Err(error);
    }

    let operations = state.import_ops.clone();
    tauri::async_runtime::spawn_blocking(move || {
        for _ in 0..CANCELLATION_WAIT_ATTEMPTS {
            if lock_ops(&operations)?.is_empty() {
                return Ok(());
            }
            std::thread::sleep(CANCELLATION_WAIT_INTERVAL);
        }
        Err(svccm_core::error::user_err(
            "import_cancellation_timed_out",
            "an import worker did not stop; retry after it finishes",
        ))
    })
    .await
    .map_err(|error| {
        svccm_core::error::internal_err(
            "import_wait_worker_failed",
            "StarVault could not wait for import cancellation",
            error.to_string(),
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(temporary: &tempfile::TempDir) -> AppState {
        let app_data = temporary.path().join("app-data");
        let import_root = temporary.path().join("app-cache").join("import");
        let store = Arc::new(svccm_core::store::Store::open(app_data.join("store")).unwrap());
        AppState::new(app_data, import_root, store)
    }

    fn ready_operation(scratch: PathBuf) -> ImportOp {
        ImportOp {
            scratch,
            cancel: Arc::new(AtomicBool::new(false)),
            snapshot: ImportOperationSnapshot {
                op_id: "operation-1".into(),
                state: ImportOperationState::Ready,
                preview: None,
                revision: None,
                error_code: None,
            },
            cleanup_started: false,
        }
    }

    #[test]
    fn operation_ids_are_bounded_safe_path_components() {
        assert!(validate_operation_id("25f31f91-120f-4e45-a4c2-17ab1f925def").is_ok());
        for invalid in ["", "../escape", "with/slash", "with\\slash", "a.b"] {
            assert!(
                validate_operation_id(invalid).is_err(),
                "accepted {invalid}"
            );
        }
        assert!(validate_operation_id(&"a".repeat(97)).is_err());
    }

    #[test]
    fn first_import_creates_the_app_specific_cache_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let import_root = temporary.path().join("app-cache").join("import");

        assert!(!import_root.parent().unwrap().exists());
        ensure_import_root(&import_root).unwrap();

        assert!(import_root.is_dir());
        assert!(import_root.parent().unwrap().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn first_import_rejects_a_linked_existing_cache_ancestor() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        let linked_cache = temporary.path().join("linked-cache");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        symlink(&external, &linked_cache).unwrap();
        let import_root = linked_cache.join("starvault").join("import");

        let error = ensure_import_root(&import_root).unwrap_err();

        assert_eq!(error.code(), "unsafe_import_root");
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"keep");
        assert!(!external.join("starvault").exists());
    }

    #[test]
    fn analysis_registration_waits_for_the_mutation_gate() {
        use std::future::Future;
        use std::task::Poll;

        let temporary = tempfile::tempdir().unwrap();
        let state = test_state(&temporary);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let held = state.mutation.lock().await;
            let scratch = state.import_root.join("operation-1");
            let mut registration = Box::pin(register_analysis(
                &state,
                "operation-1",
                &scratch,
                Arc::new(AtomicBool::new(false)),
            ));
            let first_poll =
                std::future::poll_fn(|context| Poll::Ready(registration.as_mut().poll(context)))
                    .await;
            assert!(matches!(first_poll, Poll::Pending));
            assert!(lock_ops(&state.import_ops).unwrap().is_empty());

            drop(held);
            registration.await.unwrap();
            assert!(lock_ops(&state.import_ops)
                .unwrap()
                .contains_key("operation-1"));
        });
    }

    #[test]
    fn ready_cancel_claims_cleanup_before_ingest_can_start() {
        let temporary = tempfile::tempdir().unwrap();
        let state = test_state(&temporary);
        let scratch = state.import_root.join("operation-1");
        lock_ops(&state.import_ops)
            .unwrap()
            .insert("operation-1".into(), ready_operation(scratch.clone()));

        assert_eq!(
            claim_immediate_cancel(&state, "operation-1").unwrap(),
            Some(scratch)
        );
        let operations = lock_ops(&state.import_ops).unwrap();
        let operation = operations.get("operation-1").unwrap();
        assert_eq!(operation.snapshot.state, ImportOperationState::Cancelled);
        assert!(operation.cleanup_started);
        drop(operations);
        assert!(claim_immediate_cancel(&state, "operation-1")
            .unwrap()
            .is_none());
    }

    #[test]
    fn running_game_rejection_leaves_ready_import_unclaimed() {
        let temporary = tempfile::tempdir().unwrap();
        let state = test_state(&temporary);
        let scratch = state.import_root.join("operation-1");
        lock_ops(&state.import_ops)
            .unwrap()
            .insert("operation-1".into(), ready_operation(scratch));

        let error = claim_ready_ingest(&state.import_ops, "operation-1", || {
            Err(svccm_core::error::EnvironmentError::GameRunning.into())
        })
        .unwrap_err();

        assert_eq!(error.code(), "game_running");
        let operations = lock_ops(&state.import_ops).unwrap();
        assert_eq!(
            operations.get("operation-1").unwrap().snapshot.state,
            ImportOperationState::Ready
        );
    }

    #[test]
    fn ingest_claim_checks_game_before_changing_operation_state() {
        let temporary = tempfile::tempdir().unwrap();
        let state = test_state(&temporary);
        let scratch = state.import_root.join("operation-1");
        lock_ops(&state.import_ops)
            .unwrap()
            .insert("operation-1".into(), ready_operation(scratch));
        let guard_called = std::cell::Cell::new(false);

        claim_ready_ingest(&state.import_ops, "operation-1", || {
            assert_eq!(
                lock_ops(&state.import_ops)
                    .unwrap()
                    .get("operation-1")
                    .unwrap()
                    .snapshot
                    .state,
                ImportOperationState::Ready
            );
            guard_called.set(true);
            Ok(())
        })
        .unwrap();

        assert!(guard_called.get());
        assert_eq!(
            lock_ops(&state.import_ops)
                .unwrap()
                .get("operation-1")
                .unwrap()
                .snapshot
                .state,
            ImportOperationState::Ingesting
        );
    }

    #[test]
    fn cancellation_waits_until_a_running_operation_is_removed() {
        let temporary = tempfile::tempdir().unwrap();
        let state = test_state(&temporary);
        let mut operation = ready_operation(state.import_root.join("operation-1"));
        operation.snapshot.state = ImportOperationState::Analyzing;
        lock_ops(&state.import_ops)
            .unwrap()
            .insert("operation-1".into(), operation);
        let operations = state.import_ops.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            lock_ops(&operations).unwrap().remove("operation-1");
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime
            .block_on(wait_for_operation_cleanup(&state, "operation-1"))
            .unwrap();
        worker.join().unwrap();
        assert!(lock_ops(&state.import_ops).unwrap().is_empty());
    }

    #[test]
    fn cancellation_retries_cleanup_for_a_terminal_operation() {
        let temporary = tempfile::tempdir().unwrap();
        let state = test_state(&temporary);
        let scratch = state.import_root.join("operation-1");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("partial"), b"data").unwrap();
        let mut operation = ready_operation(scratch.clone());
        operation.snapshot.state = ImportOperationState::Cancelled;
        lock_ops(&state.import_ops)
            .unwrap()
            .insert("operation-1".into(), operation);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime
            .block_on(wait_for_operation_cleanup(&state, "operation-1"))
            .unwrap();

        assert!(!scratch.exists());
        assert!(lock_ops(&state.import_ops).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn linked_import_root_and_nested_scratch_link_preserve_external_data() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let external_root = temporary.path().join("external-root");
        let linked_root = temporary.path().join("linked-root");
        std::fs::create_dir(&external_root).unwrap();
        std::fs::write(external_root.join("sentinel"), b"keep").unwrap();
        symlink(&external_root, &linked_root).unwrap();
        assert_eq!(
            ensure_import_root(&linked_root).unwrap_err().code(),
            "unsafe_import_root"
        );

        let import_root = temporary.path().join("import-root");
        let scratch = import_root.join("operation-1");
        let external_nested = temporary.path().join("external-nested");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir(&external_nested).unwrap();
        std::fs::write(external_nested.join("sentinel"), b"keep").unwrap();
        symlink(&external_nested, scratch.join("nested-link")).unwrap();

        assert_eq!(
            cleanup_scratch(&scratch).unwrap_err().code(),
            "unsafe_import_scratch"
        );
        assert_eq!(
            std::fs::read(external_nested.join("sentinel")).unwrap(),
            b"keep"
        );
        assert!(scratch.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn junctioned_import_root_and_nested_scratch_junction_preserve_external_data() {
        let temporary = tempfile::tempdir().unwrap();
        let external_root = temporary.path().join("external-root");
        let linked_root = temporary.path().join("linked-root");
        std::fs::create_dir(&external_root).unwrap();
        std::fs::write(external_root.join("sentinel"), b"keep").unwrap();
        junction::create(&external_root, &linked_root).unwrap();
        assert_eq!(
            ensure_import_root(&linked_root).unwrap_err().code(),
            "unsafe_import_root"
        );

        let import_root = temporary.path().join("import-root");
        let scratch = import_root.join("operation-1");
        let external_nested = temporary.path().join("external-nested");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir(&external_nested).unwrap();
        std::fs::write(external_nested.join("sentinel"), b"keep").unwrap();
        junction::create(&external_nested, scratch.join("nested-junction")).unwrap();

        assert_eq!(
            cleanup_scratch(&scratch).unwrap_err().code(),
            "unsafe_import_scratch"
        );
        assert_eq!(
            std::fs::read(external_nested.join("sentinel")).unwrap(),
            b"keep"
        );
        assert!(scratch.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn archive_sources_reject_windows_reparse_points() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        let junction_path = temporary.path().join("archive.zip");
        std::fs::create_dir(&target).unwrap();
        junction::create(&target, &junction_path).unwrap();

        let metadata = std::fs::symlink_metadata(&junction_path).unwrap();
        assert!(svccm_core::filesystem::is_link_or_reparse(&metadata));
        assert!(!is_regular_archive_source(&metadata));
    }
}
