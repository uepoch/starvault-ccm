//! Content-addressed blobs, one current manifest per package, and the ledger.
//!
//! ```text
//! <root>/
//!   blobs/<ab>/<sha256>
//!   packages/<package-id>/manifest.json
//!   ledger.db
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::contracts::{ActiveCampaign, Health};
use crate::error::{internal_err, package_err, pkg_err, user_err, user_path_err, Result};
use crate::identity::PackageId;
use crate::layout::SlotId;
use crate::package::import::{
    is_safe_package_path_segment, ArchiveLimits, ImportProgress, CANCELLATION_CHUNK_BYTES,
};
use crate::package::normalize::PackagePlan;

const STORE_SCHEMA_VERSION: i64 = 2;
const MANIFEST_FILE: &str = "manifest.json";
// 20,000 maximally sized canonical paths plus hashes, sizes, JSON framing,
// and bounded package metadata fit comfortably below this ceiling.
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
static NEXT_BLOB_TEMP: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    /// Canonical package path beginning with `slot/` or `mods/`.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageManifest {
    pub id: PackageId,
    pub revision: String,
    pub faction: SlotId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<u64>,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CorruptPackage {
    /// Directory name when it was valid UTF-8. It may still be an invalid ID.
    pub directory_name: Option<String>,
    pub manifest_path: PathBuf,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct PackageInventory {
    pub packages: Vec<PackageManifest>,
    pub corrupt: Vec<CorruptPackage>,
}

impl PackageInventory {
    pub fn is_clean(&self) -> bool {
        self.corrupt.is_empty()
    }

    fn require_clean(self) -> Result<Vec<PackageManifest>> {
        if self.corrupt.is_empty() {
            Ok(self.packages)
        } else {
            Err(package_err(
                "corrupt_package_inventory",
                format!(
                    "{} package manifest(s) are unreadable; no storage was deleted",
                    self.corrupt.len()
                ),
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedModDisposition {
    Created,
    Borrowed,
}

impl ManagedModDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Borrowed => "borrowed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "created" => Ok(Self::Created),
            "borrowed" => Ok(Self::Borrowed),
            _ => Err(internal_err(
                "invalid_managed_mod_disposition",
                "StarVault could not read its deployment ledger",
                format!("unknown managed Mods disposition `{value}`"),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedMod {
    /// Path relative to the game's `Mods` directory.
    pub path: String,
    pub sha256: String,
    pub disposition: ManagedModDisposition,
}

pub struct Store {
    root: PathBuf,
    conn: Mutex<rusqlite::Connection>,
    manifests: Mutex<HashMap<PackageId, PackageManifest>>,
    workflow_health: Mutex<Option<CachedWorkflowHealth>>,
    verified_deployments: Mutex<HashSet<PathBuf>>,
    import_reserve_bytes: u64,
}

#[derive(Clone)]
struct CachedWorkflowHealth {
    layout_root: PathBuf,
    save_isolation_expected: bool,
    save_isolation_available: bool,
    health: Health,
}

impl Store {
    /// Open a fresh schema or an existing version 2 store.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_import_reserve(root, ArchiveLimits::default().reserve_bytes)
    }

    /// Deterministic integration-test constructor. Production callers must
    /// use [`Store::open`], which enforces the 1 GiB import reserve.
    #[doc(hidden)]
    pub fn open_for_tests(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_import_reserve(root, 0)
    }

    fn open_with_import_reserve(root: impl AsRef<Path>, import_reserve_bytes: u64) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        match std::fs::symlink_metadata(&root) {
            Ok(metadata) => ensure_real_directory_metadata(&root, &metadata, "store root")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&root).map_err(|error| {
                    user_path_err("create_store_root", error.to_string(), &root, false)
                })?;
                ensure_real_directory(&root, "store root")?;
            }
            Err(error) => {
                return Err(user_path_err(
                    "inspect_store_root",
                    error.to_string(),
                    &root,
                    false,
                ));
            }
        }
        let blobs = root.join("blobs");
        ensure_or_create_real_directory(&blobs, "blob store")?;
        let packages = root.join("packages");
        ensure_or_create_real_directory(&packages, "package store")?;
        ensure_optional_real_directory(&root.join("blob-staging"), "blob staging")?;
        ensure_optional_real_directory(&root.join("deploy"), "deployment store")?;
        let ledger = root.join("ledger.db");
        ensure_optional_real_file(&ledger, "store ledger")?;
        let conn = rusqlite::Connection::open_with_flags(
            &ledger,
            rusqlite::OpenFlags::default() | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| ledger_error("open", &ledger, error))?;
        ensure_real_file(&ledger, "store ledger")?;
        initialize_schema(&conn, &ledger)?;
        Ok(Self {
            root,
            conn: Mutex::new(conn),
            manifests: Mutex::new(HashMap::new()),
            workflow_health: Mutex::new(None),
            verified_deployments: Mutex::new(HashSet::new()),
            import_reserve_bytes,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn cached_workflow_health(
        &self,
        layout_root: &Path,
        save_isolation_expected: bool,
        save_isolation_available: bool,
    ) -> Option<Health> {
        self.workflow_health
            .lock()
            .expect("workflow health cache poisoned")
            .as_ref()
            .filter(|cached| {
                cached.layout_root == layout_root
                    && cached.save_isolation_expected == save_isolation_expected
                    && cached.save_isolation_available == save_isolation_available
            })
            .map(|cached| cached.health.clone())
    }

    pub(crate) fn cache_workflow_health(
        &self,
        layout_root: &Path,
        save_isolation_expected: bool,
        save_isolation_available: bool,
        health: Health,
    ) {
        *self
            .workflow_health
            .lock()
            .expect("workflow health cache poisoned") = Some(CachedWorkflowHealth {
            layout_root: layout_root.to_path_buf(),
            save_isolation_expected,
            save_isolation_available,
            health,
        });
    }

    pub fn deploy_dir(&self, faction: SlotId, revision: &str) -> Result<PathBuf> {
        validate_sha256(revision, "revision")?;
        self.ensure_store_root()?;
        let deploy = self.root.join("deploy");
        ensure_or_create_real_directory(&deploy, "deployment store")?;
        let target = deploy.join(format!("{}-{revision}", faction.as_str()));
        ensure_optional_real_directory(&target, "deployment tree")?;
        Ok(target)
    }

    pub(crate) fn deployment_was_verified(&self, path: &Path) -> bool {
        self.verified_deployments
            .lock()
            .expect("deployment verification cache poisoned")
            .contains(path)
    }

    pub(crate) fn mark_deployment_verified(&self, path: &Path) {
        self.verified_deployments
            .lock()
            .expect("deployment verification cache poisoned")
            .insert(path.to_path_buf());
    }

    pub(crate) fn forget_deployment(&self, path: &Path) {
        self.verified_deployments
            .lock()
            .expect("deployment verification cache poisoned")
            .remove(path);
    }

    pub fn ingest(&self, id: &PackageId, faction: SlotId, plan: &PackagePlan) -> Result<String> {
        self.ingest_with_progress(id, faction, plan, |_| true)?
            .ok_or_else(|| package_err("import_cancelled", "package ingestion was cancelled"))
    }

    /// Ingest one package and atomically replace its sole current manifest.
    /// The callback is consulted before each chunk of at most 4 MiB.
    #[tracing::instrument(skip_all, fields(pkg = %id, faction = faction.as_str()))]
    pub fn ingest_with_progress(
        &self,
        id: &PackageId,
        faction: SlotId,
        plan: &PackagePlan,
        mut on_progress: impl FnMut(ImportProgress) -> bool,
    ) -> Result<Option<String>> {
        self.reject_pending_operation()?;
        self.reject_active_package(id)?;
        self.validate_package_for_write(id)?;
        validate_plan(plan)?;
        let source_sizes = plan
            .files
            .iter()
            .map(|planned| {
                std::fs::symlink_metadata(&planned.source)
                    .map_err(|error| {
                        user_path_err(
                            "inspect_import_file",
                            error.to_string(),
                            &planned.source,
                            false,
                        )
                    })
                    .map(|metadata| metadata.len())
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some((planned, _)) = plan
            .files
            .iter()
            .zip(&source_sizes)
            .find(|(_, size)| **size > ArchiveLimits::default().max_file_bytes)
        {
            return Err(pkg_err(
                &planned.target,
                "import source exceeds the 2 GiB file limit",
            ));
        }
        let declared_size = source_sizes.iter().try_fold(0_u64, |total, size| {
            let total = total.checked_add(*size).ok_or_else(|| {
                package_err("package_size_overflow", "package size overflows u64")
            })?;
            if total > ArchiveLimits::default().max_total_bytes {
                return Err(package_err(
                    "package_total_limit",
                    "package exceeds the 8 GiB uncompressed limit",
                ));
            }
            Ok(total)
        })?;
        crate::package::import::require_available_space(
            &self.root,
            declared_size,
            self.import_reserve_bytes,
        )?;

        let files_total = plan.files.len() as u64;
        let mut files = Vec::with_capacity(plan.files.len());
        for (index, (planned, expected_size)) in plan.files.iter().zip(source_sizes).enumerate() {
            let progress = ImportProgress {
                files_done: index as u64,
                files_total,
                current_file: planned.target.clone(),
            };
            let Some(file) = self.ingest_file(
                &planned.source,
                &planned.target,
                expected_size,
                &progress,
                &mut on_progress,
            )?
            else {
                return Ok(None);
            };
            files.push(file);
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        validate_manifest_files(&files)?;

        let revision = content_revision(faction, &files)?;
        let manifest = PackageManifest {
            id: id.clone(),
            revision: revision.clone(),
            faction,
            title: plan
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.title.clone()),
            author: plan
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.author.clone()),
            version: plan
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.version.clone()),
            desc: plan
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.desc.clone()),
            imported_at: Some(unix_timestamp()),
            files,
        };
        validate_manifest(id, &manifest)?;

        // The process-wide mutation lock is the primary guard. Checking again
        // here also protects direct core callers that race activation.
        self.reject_pending_operation()?;
        self.reject_active_package(id)?;
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
            internal_err(
                "serialize_package_manifest",
                "StarVault could not save the imported package",
                error.to_string(),
            )
        })?;
        validate_manifest_size(bytes.len())?;
        let manifest_path = self.manifest_path_for_write(id)?;
        crate::atomic_file::write(&manifest_path, &bytes)?;
        self.manifests
            .lock()
            .expect("manifest cache poisoned")
            .insert(id.clone(), manifest);
        Ok(Some(revision))
    }

    fn ingest_file(
        &self,
        source: &Path,
        target: &str,
        expected_size: u64,
        progress: &ImportProgress,
        on_progress: &mut impl FnMut(ImportProgress) -> bool,
    ) -> Result<Option<ManifestFile>> {
        let metadata = std::fs::symlink_metadata(source).map_err(|error| {
            user_path_err("inspect_import_file", error.to_string(), source, false)
        })?;
        if !metadata.file_type().is_file() || is_link(&metadata) {
            return Err(pkg_err(target, "import source must be a regular file"));
        }
        if metadata.len() != expected_size {
            return Err(pkg_err(target, "import source changed before it was read"));
        }
        if expected_size > ArchiveLimits::default().max_file_bytes {
            return Err(pkg_err(
                target,
                "import source exceeds the 2 GiB file limit",
            ));
        }

        self.ensure_store_root()?;
        let staging_dir = self.root.join("blob-staging");
        ensure_or_create_real_directory(&staging_dir, "blob staging")?;
        let sequence = NEXT_BLOB_TEMP.fetch_add(1, Ordering::Relaxed);
        let temporary = staging_dir.join(format!("blob-{}-{sequence}.partial", std::process::id()));
        let result = self.copy_source_to_blob(
            source,
            target,
            expected_size,
            &temporary,
            progress,
            on_progress,
        );
        if result.is_err() || matches!(result, Ok(None)) {
            if let Err(cleanup) = remove_blob_temporary(&staging_dir, &temporary) {
                return match result {
                    Err(primary) => Err(internal_err(
                        "blob_staging_cleanup_failed",
                        "StarVault could not safely clean up an incomplete package import",
                        format!("import failed: {primary}; cleanup failed: {cleanup}"),
                    )),
                    Ok(None) => Err(cleanup),
                    Ok(Some(_)) => unreachable!("completed blobs are not cleaned up"),
                };
            }
        }
        result
    }

    fn copy_source_to_blob(
        &self,
        source: &Path,
        target: &str,
        expected_size: u64,
        temporary: &Path,
        progress: &ImportProgress,
        on_progress: &mut impl FnMut(ImportProgress) -> bool,
    ) -> Result<Option<ManifestFile>> {
        use sha2::{Digest, Sha256};

        let mut input = std::fs::File::open(source)
            .map_err(|error| user_path_err("open_import_file", error.to_string(), source, false))?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary)
            .map_err(|error| {
                user_path_err("create_blob_staging", error.to_string(), temporary, false)
            })?;
        let mut hasher = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = vec![0_u8; CANCELLATION_CHUNK_BYTES];
        loop {
            if !on_progress(progress.clone()) {
                return Ok(None);
            }
            let read = input.read(&mut buffer).map_err(|error| {
                user_path_err("read_import_file", error.to_string(), source, false)
            })?;
            if read == 0 {
                break;
            }
            copied = copied
                .checked_add(read as u64)
                .ok_or_else(|| pkg_err(target, "file size overflows u64"))?;
            if copied > expected_size || copied > ArchiveLimits::default().max_file_bytes {
                return Err(pkg_err(
                    target,
                    "import source changed or exceeds its size limit",
                ));
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read]).map_err(|error| {
                user_path_err("write_blob_staging", error.to_string(), temporary, false)
            })?;
        }
        if copied != expected_size {
            return Err(pkg_err(target, "import source changed while it was read"));
        }
        output.flush().map_err(|error| {
            user_path_err("flush_blob_staging", error.to_string(), temporary, false)
        })?;
        output.sync_all().map_err(|error| {
            user_path_err("sync_blob_staging", error.to_string(), temporary, false)
        })?;
        drop(output);

        let sha256 = hex::encode(hasher.finalize());
        let staging_dir = temporary.parent().ok_or_else(|| {
            package_err(
                "invalid_blob_staging",
                "staged blob has no parent directory",
            )
        })?;
        ensure_real_directory(staging_dir, "blob staging")?;
        ensure_real_file(temporary, "staged blob")?;
        let blob = self.blob_path_for_write(&sha256)?;
        if ensure_optional_real_file(&blob, "package blob")? {
            // The staged file was hashed while it was written. Replacing an
            // existing blob repairs latent corruption without a second long,
            // uncancellable read of the old blob.
            ensure_real_file(&blob, "package blob")?;
            replace_file(temporary, &blob).map_err(|error| {
                user_path_err("replace_package_blob", error.to_string(), &blob, false)
            })?;
            sync_directory(blob.parent().expect("blob has a shard parent"))?;
        } else {
            let parent = blob.parent().expect("blob has a shard parent");
            ensure_real_directory(parent, "blob shard")?;
            if let Err(error) = std::fs::rename(temporary, &blob) {
                if ensure_optional_real_file(&blob, "package blob")? {
                    ensure_real_file(&blob, "package blob")?;
                    verify_blob(&blob, &sha256, copied)?;
                    std::fs::remove_file(temporary)?;
                } else {
                    return Err(user_path_err(
                        "commit_blob",
                        error.to_string(),
                        &blob,
                        false,
                    ));
                }
            }
            ensure_real_file(&blob, "package blob")?;
            sync_directory(parent)?;
        }
        verify_blob(&blob, &sha256, copied)?;
        Ok(Some(ManifestFile {
            path: target.to_string(),
            sha256,
            size: copied,
        }))
    }

    #[tracing::instrument(skip_all, fields(pkg = %id))]
    pub fn set_metadata(
        &self,
        id: &PackageId,
        title: &str,
        author: &str,
        version: &str,
        desc: &str,
    ) -> Result<()> {
        self.reject_pending_operation()?;
        let clean = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_string());
        let mut manifest = self.load_manifest_fresh(id)?;
        manifest.title = clean(title);
        manifest.author = clean(author);
        manifest.version = clean(version);
        manifest.desc = clean(desc);
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
            internal_err(
                "serialize_package_manifest",
                "StarVault could not save package metadata",
                error.to_string(),
            )
        })?;
        validate_manifest_size(bytes.len())?;
        self.reject_pending_operation()?;
        let manifest_path = self.manifest_path_for_write(id)?;
        crate::atomic_file::write(&manifest_path, &bytes)?;
        self.manifests
            .lock()
            .expect("manifest cache poisoned")
            .insert(id.clone(), manifest);
        Ok(())
    }

    pub fn load_manifest(&self, id: &PackageId) -> Result<PackageManifest> {
        self.manifest_path_for_read(id)?;
        if let Some(manifest) = self
            .manifests
            .lock()
            .expect("manifest cache poisoned")
            .get(id)
            .cloned()
        {
            return Ok(manifest);
        }
        let manifest = self.read_manifest(id)?;
        self.manifests
            .lock()
            .expect("manifest cache poisoned")
            .insert(id.clone(), manifest.clone());
        Ok(manifest)
    }

    /// Re-read the manifest from its atomic on-disk source and refresh the
    /// cache. Workflows use this when manifest bytes are recovery evidence:
    /// a stale in-process cache must never outlive an atomic replacement.
    pub(crate) fn load_manifest_fresh(&self, id: &PackageId) -> Result<PackageManifest> {
        let manifest = self.read_manifest(id)?;
        self.manifests
            .lock()
            .expect("manifest cache poisoned")
            .insert(id.clone(), manifest.clone());
        Ok(manifest)
    }

    /// Validate the manifest and every referenced blob's type and size before
    /// deployment. Ingestion already hashes content while creating immutable
    /// blobs; activation must not reread an entire package.
    pub fn verify_package(&self, id: &PackageId) -> Result<PackageManifest> {
        let manifest = self.read_manifest(id)?;
        for file in &manifest.files {
            verify_blob_metadata(&self.blob_path(&file.sha256)?, file.size)?;
        }
        self.manifests
            .lock()
            .expect("manifest cache poisoned")
            .insert(id.clone(), manifest.clone());
        Ok(manifest)
    }

    fn read_manifest(&self, id: &PackageId) -> Result<PackageManifest> {
        let path = self.manifest_path_for_read(id)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            user_path_err("inspect_package_manifest", error.to_string(), &path, false)
        })?;
        ensure_real_file(&path, "package manifest")?;
        validate_manifest_size(metadata.len().try_into().unwrap_or(usize::MAX))?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::fs::File::open(&path)
            .map_err(|error| {
                user_path_err("read_package_manifest", error.to_string(), &path, false)
            })?
            .take((MAX_MANIFEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                user_path_err("read_package_manifest", error.to_string(), &path, false)
            })?;
        validate_manifest_size(bytes.len())?;
        let manifest: PackageManifest = serde_json::from_slice(&bytes).map_err(|error| {
            package_err(
                "corrupt_package_manifest",
                format!("package `{id}` has an unreadable manifest: {error}"),
            )
        })?;
        validate_manifest(id, &manifest)?;
        Ok(manifest)
    }

    /// Enumerate every package directory. Corrupt entries remain visible.
    pub fn inventory(&self) -> Result<PackageInventory> {
        let packages_dir = self.root.join("packages");
        self.ensure_store_root()?;
        ensure_real_directory(&packages_dir, "package store")?;
        let mut inventory = PackageInventory::default();
        for entry in read_dir_sorted(&packages_dir)? {
            let path = entry.path();
            let directory_name = entry.file_name().into_string().ok();
            let manifest_path = path.join(MANIFEST_FILE);
            let result = (|| -> Result<PackageManifest> {
                let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                    user_path_err("inspect_package_directory", error.to_string(), &path, false)
                })?;
                if !metadata.is_dir() || is_link(&metadata) {
                    return Err(package_err(
                        "corrupt_package_directory",
                        "package inventory entry is not a real directory",
                    ));
                }
                let name = directory_name.as_deref().ok_or_else(|| {
                    package_err(
                        "invalid_package_id",
                        "package directory name is not valid UTF-8",
                    )
                })?;
                let id = PackageId::parse(name)?;
                self.read_manifest(&id).map_err(|error| {
                    if error.code() == "package_not_installed" {
                        package_err(
                            "corrupt_package_manifest",
                            format!("package `{id}` has no manifest"),
                        )
                    } else {
                        error
                    }
                })
            })();
            match result {
                Ok(manifest) => inventory.packages.push(manifest),
                Err(error) => inventory.corrupt.push(CorruptPackage {
                    directory_name,
                    manifest_path,
                    code: error.code().to_string(),
                    message: error.to_string(),
                }),
            }
        }
        inventory
            .packages
            .sort_by(|left, right| left.id.cmp(&right.id));
        inventory.corrupt.sort_by(|left, right| {
            left.manifest_path
                .as_os_str()
                .cmp(right.manifest_path.as_os_str())
        });
        let cache = inventory
            .packages
            .iter()
            .cloned()
            .map(|manifest| (manifest.id.clone(), manifest))
            .collect();
        *self.manifests.lock().expect("manifest cache poisoned") = cache;
        Ok(inventory)
    }

    pub fn all_manifests(&self) -> Result<Vec<PackageManifest>> {
        self.inventory()?.require_clean()
    }

    pub fn materialize_slot(&self, manifest: &PackageManifest, dest: &Path) -> Result<()> {
        self.materialize_tree(manifest, "slot/", dest)
    }

    /// Materialize the complete loose override view rooted at
    /// `Maps/Campaign`. Only the selected faction contains package files; the
    /// other faction directories remain empty while the campaign is active.
    pub fn materialize_campaign(&self, manifest: &PackageManifest, dest: &Path) -> Result<()> {
        match std::fs::symlink_metadata(dest) {
            Ok(metadata) => {
                ensure_real_directory_metadata(dest, &metadata, "campaign deployment root")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(dest)?;
                ensure_real_directory(dest, "campaign deployment root")?;
            }
            Err(error) => return Err(error.into()),
        }
        for directory in ["swarm", "void", "voidprologue", "nova"] {
            ensure_or_create_real_directory(&dest.join(directory), "campaign faction directory")?;
        }
        let slot = match manifest.faction {
            SlotId::Wol => dest.to_path_buf(),
            SlotId::HotS => dest.join("swarm"),
            SlotId::LotV => dest.join("void"),
            SlotId::Nco => dest.join("nova"),
        };
        self.materialize_slot(manifest, &slot)
    }

    pub fn materialize_mods(&self, manifest: &PackageManifest, dest: &Path) -> Result<()> {
        self.materialize_tree(manifest, "mods/", dest)
    }

    fn materialize_tree(
        &self,
        manifest: &PackageManifest,
        prefix: &str,
        dest: &Path,
    ) -> Result<()> {
        validate_manifest(&manifest.id, manifest)?;
        match std::fs::symlink_metadata(dest) {
            Ok(metadata) => {
                ensure_real_directory_metadata(dest, &metadata, "materialization root")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(dest)?;
                ensure_real_directory(dest, "materialization root")?;
            }
            Err(error) => return Err(error.into()),
        }
        for file in &manifest.files {
            let Some(relative) = file.path.strip_prefix(prefix) else {
                continue;
            };
            let target = dest.join(relative);
            prepare_file_target(dest, &target)?;
            copy_with_retry(&self.blob_path(&file.sha256)?, &target)?;
        }
        Ok(())
    }

    // Ledger

    pub fn active_campaign(&self) -> Result<Option<ActiveCampaign>> {
        let conn = self.conn.lock().expect("ledger poisoned");
        let mut statement = conn
            .prepare(
                "SELECT package_id, revision, faction
                 FROM active_campaign WHERE singleton = 1",
            )
            .map_err(|error| ledger_query_error("prepare active campaign query", error))?;
        let mut rows = statement
            .query([])
            .map_err(|error| ledger_query_error("query active campaign", error))?;
        let Some(row) = rows
            .next()
            .map_err(|error| ledger_query_error("read active campaign", error))?
        else {
            return Ok(None);
        };
        let package_id: String = row
            .get(0)
            .map_err(|error| ledger_query_error("read active package id", error))?;
        let revision: String = row
            .get(1)
            .map_err(|error| ledger_query_error("read active revision", error))?;
        let faction: String = row
            .get(2)
            .map_err(|error| ledger_query_error("read active faction", error))?;
        validate_sha256(&revision, "active revision").map_err(|error| {
            internal_err(
                "corrupt_active_campaign",
                "StarVault could not read its activation state",
                error.diagnostic(),
            )
        })?;
        Ok(Some(ActiveCampaign {
            id: PackageId::parse(package_id).map_err(|error| {
                internal_err(
                    "corrupt_active_campaign",
                    "StarVault could not read its activation state",
                    error.diagnostic(),
                )
            })?,
            revision,
            faction: faction.parse().map_err(|error: crate::Error| {
                internal_err(
                    "corrupt_active_campaign",
                    "StarVault could not read its activation state",
                    error.diagnostic(),
                )
            })?,
        }))
    }

    pub fn managed_mods(&self) -> Result<Vec<ManagedMod>> {
        let conn = self.conn.lock().expect("ledger poisoned");
        let mut statement = conn
            .prepare(
                "SELECT path, sha256, disposition
                 FROM managed_mods ORDER BY path COLLATE NOCASE",
            )
            .map_err(|error| ledger_query_error("prepare managed Mods query", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| ledger_query_error("query managed Mods", error))?;
        let mut mods = Vec::new();
        for row in rows {
            let (path, sha256, disposition) =
                row.map_err(|error| ledger_query_error("read managed Mods", error))?;
            let managed = ManagedMod {
                path,
                sha256,
                disposition: ManagedModDisposition::parse(&disposition)?,
            };
            validate_managed_mod(&managed).map_err(|error| {
                internal_err(
                    "corrupt_managed_mods",
                    "StarVault could not read its deployment state",
                    error.diagnostic(),
                )
            })?;
            mods.push(managed);
        }
        Ok(mods)
    }

    /// Atomically replace the singleton campaign and all managed Mods rows.
    pub fn commit_active_state(
        &self,
        campaign: Option<&ActiveCampaign>,
        managed_mods: &[ManagedMod],
    ) -> Result<()> {
        if campaign.is_none() && !managed_mods.is_empty() {
            return Err(internal_err(
                "managed_mods_without_campaign",
                "StarVault could not save its activation state",
                "managed Mods cannot exist without an active campaign",
            ));
        }
        if let Some(campaign) = campaign {
            let manifest = self.load_manifest_fresh(&campaign.id)?;
            if manifest.revision != campaign.revision || manifest.faction != campaign.faction {
                return Err(package_err(
                    "active_campaign_manifest_mismatch",
                    "active campaign does not match the installed package manifest",
                ));
            }
            verify_managed_mods_manifest(&manifest, managed_mods)?;
        }
        validate_managed_mods(managed_mods)?;

        let mut conn = self.conn.lock().expect("ledger poisoned");
        let transaction = conn
            .transaction()
            .map_err(|error| ledger_query_error("begin active state transaction", error))?;
        transaction
            .execute("DELETE FROM active_campaign", [])
            .map_err(|error| ledger_query_error("clear active campaign", error))?;
        transaction
            .execute("DELETE FROM managed_mods", [])
            .map_err(|error| ledger_query_error("clear managed Mods", error))?;
        if let Some(campaign) = campaign {
            transaction
                .execute(
                    "INSERT INTO active_campaign(singleton, package_id, revision, faction)
                     VALUES(1, ?1, ?2, ?3)",
                    rusqlite::params![
                        campaign.id.as_str(),
                        campaign.revision,
                        campaign.faction.as_str()
                    ],
                )
                .map_err(|error| ledger_query_error("write active campaign", error))?;
        }
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO managed_mods(path, sha256, disposition)
                     VALUES(?1, ?2, ?3)",
                )
                .map_err(|error| ledger_query_error("prepare managed Mods insert", error))?;
            for managed in managed_mods {
                statement
                    .execute(rusqlite::params![
                        managed.path,
                        managed.sha256,
                        managed.disposition.as_str()
                    ])
                    .map_err(|error| ledger_query_error("write managed Mods", error))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| ledger_query_error("commit active state", error))
    }

    pub fn set_active_campaign(&self, campaign: &ActiveCampaign) -> Result<()> {
        self.commit_active_state(Some(campaign), &[])
    }

    pub fn replace_managed_mods(&self, managed_mods: &[ManagedMod]) -> Result<()> {
        let campaign = self.active_campaign()?;
        self.commit_active_state(campaign.as_ref(), managed_mods)
    }

    pub fn clear_active_campaign(&self) -> Result<()> {
        self.commit_active_state(None, &[])
    }

    /// Prove that the deployment ledger contains exactly one row for every
    /// Mods file in the active package. Disposition is intentionally not part
    /// of the package manifest: it records whether an identical external file
    /// was borrowed when the deployment was prepared.
    pub(crate) fn verify_managed_mods_manifest(
        &self,
        manifest: &PackageManifest,
        managed_mods: &[ManagedMod],
    ) -> Result<()> {
        verify_managed_mods_manifest(manifest, managed_mods)
    }

    #[tracing::instrument(skip_all, fields(pkg = %id))]
    pub fn remove_package(&self, id: &PackageId) -> Result<()> {
        self.reject_pending_operation()?;
        self.reject_active_package(id)?;
        let inventory = self.inventory()?;
        if !inventory.is_clean() {
            return Err(package_err(
                "corrupt_package_inventory",
                format!(
                    "{} package manifest(s) are unreadable; no package was removed",
                    inventory.corrupt.len()
                ),
            ));
        }
        let Some(removed) = inventory
            .packages
            .iter()
            .find(|manifest| &manifest.id == id)
            .cloned()
        else {
            return Err(package_err(
                "package_not_installed",
                format!("package `{id}` is not installed"),
            ));
        };
        let survivors: Vec<_> = inventory
            .packages
            .iter()
            .filter(|manifest| &manifest.id != id)
            .cloned()
            .collect();
        let active = self.active_campaign()?;
        // Deployment trees are content caches, not package-owned directories.
        // Equal package content intentionally shares one `{faction}-{revision}`
        // tree, which may also be the target of the active campaign-root
        // junction.
        let deploy_is_referenced = survivors.iter().any(|manifest| {
            manifest.faction == removed.faction && manifest.revision == removed.revision
        }) || active.as_ref().is_some_and(|campaign| {
            campaign.faction == removed.faction && campaign.revision == removed.revision
        });
        let deploys = if deploy_is_referenced {
            Vec::new()
        } else {
            vec![self.deploy_dir(removed.faction, &removed.revision)?]
        };
        let package_dir = self.package_dir(id);
        ensure_real_directory(&package_dir, "package directory")?;
        validate_package_directory_layout(&package_dir, id)?;
        for deploy in &deploys {
            validate_real_tree_for_removal(deploy, "deployment tree")?;
        }
        self.reject_pending_operation()?;
        // Deployment trees are derived caches. Remove them first so a later
        // package-directory failure can never leave an installed package
        // without its manifest.
        for deploy in &deploys {
            remove_real_tree_for_removal(deploy, "deployment tree")?;
            self.forget_deployment(deploy);
        }
        remove_real_tree_for_removal(&package_dir, "package directory")?;
        self.manifests
            .lock()
            .expect("manifest cache poisoned")
            .remove(id);
        self.sweep_unreferenced_blobs(&survivors)
    }

    /// Delete only blobs unreferenced by a completely readable inventory.
    pub fn gc(&self) -> Result<()> {
        self.reject_pending_operation()?;
        let manifests = self.inventory()?.require_clean()?;
        self.sweep_unreferenced_blobs(&manifests)
    }

    fn sweep_unreferenced_blobs(&self, manifests: &[PackageManifest]) -> Result<()> {
        let referenced: BTreeSet<&str> = manifests
            .iter()
            .flat_map(|manifest| manifest.files.iter().map(|file| file.sha256.as_str()))
            .collect();
        let blobs_dir = self.root.join("blobs");
        self.ensure_store_root()?;
        ensure_real_directory(&blobs_dir, "blob store")?;
        let staging_dir = self.root.join("blob-staging");
        ensure_optional_real_directory(&staging_dir, "blob staging")?;
        let mut files_to_remove = Vec::new();
        let mut shards = Vec::new();
        for shard in read_dir_sorted(&blobs_dir)? {
            let shard_path = shard.path();
            let shard_name = shard.file_name().into_string().map_err(|_| {
                package_err(
                    "corrupt_blob_store",
                    "blob shard name is not valid UTF-8; no blobs were deleted",
                )
            })?;
            let shard_metadata = std::fs::symlink_metadata(&shard_path).map_err(|error| {
                user_path_err("inspect_blob_shard", error.to_string(), &shard_path, false)
            })?;
            if shard_name.len() != 2
                || !shard_name
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || !shard_metadata.is_dir()
                || is_link(&shard_metadata)
            {
                return Err(package_err(
                    "corrupt_blob_store",
                    "unexpected blob-store entry; no blobs were deleted",
                ));
            }
            for blob in read_dir_sorted(&shard_path)? {
                let blob_path = blob.path();
                let sha256 = blob.file_name().into_string().map_err(|_| {
                    package_err(
                        "corrupt_blob_store",
                        "blob name is not valid UTF-8; no blobs were deleted",
                    )
                })?;
                let blob_metadata = std::fs::symlink_metadata(&blob_path).map_err(|error| {
                    user_path_err("inspect_blob", error.to_string(), &blob_path, false)
                })?;
                if !blob_metadata.is_file()
                    || is_link(&blob_metadata)
                    || validate_sha256(&sha256, "blob name").is_err()
                    || !sha256.starts_with(&shard_name)
                {
                    return Err(package_err(
                        "corrupt_blob_store",
                        "unexpected blob-store entry; no blobs were deleted",
                    ));
                }
                if !referenced.contains(sha256.as_str()) {
                    files_to_remove.push(blob_path);
                }
            }
            shards.push(shard_path);
        }
        for path in files_to_remove {
            ensure_real_directory(&blobs_dir, "blob store")?;
            ensure_real_directory(
                path.parent().expect("blob has a shard parent"),
                "blob shard",
            )?;
            ensure_real_file(&path, "package blob")?;
            std::fs::remove_file(&path).map_err(|error| {
                user_path_err("remove_unreferenced_blob", error.to_string(), &path, false)
            })?;
        }
        for shard in shards {
            ensure_real_directory(&blobs_dir, "blob store")?;
            ensure_real_directory(&shard, "blob shard")?;
            if std::fs::read_dir(&shard)
                .map_err(|error| {
                    user_path_err("read_blob_shard", error.to_string(), &shard, false)
                })?
                .next()
                .is_none()
            {
                std::fs::remove_dir(&shard).map_err(|error| {
                    user_path_err("remove_empty_blob_shard", error.to_string(), &shard, false)
                })?;
            }
        }
        ensure_optional_real_directory(&staging_dir, "blob staging")?;
        remove_if_empty(&staging_dir)?;
        Ok(())
    }

    fn reject_active_package(&self, id: &PackageId) -> Result<()> {
        if self
            .active_campaign()?
            .as_ref()
            .is_some_and(|campaign| &campaign.id == id)
        {
            return Err(user_err(
                "active_package_requires_restore",
                format!("`{id}` is active; return to vanilla before replacing or removing it"),
            ));
        }
        Ok(())
    }

    fn reject_pending_operation(&self) -> Result<()> {
        if crate::operation::PendingOperation::load(&self.root)?.is_some() {
            return Err(package_err(
                "recovery_required",
                "recover the interrupted campaign operation before changing package storage",
            ));
        }
        Ok(())
    }

    fn package_dir(&self, id: &PackageId) -> PathBuf {
        self.root.join("packages").join(id.as_str())
    }

    fn ensure_store_root(&self) -> Result<()> {
        ensure_real_directory(&self.root, "store root")
    }

    fn blob_path(&self, sha256: &str) -> Result<PathBuf> {
        validate_sha256(sha256, "blob hash")?;
        self.ensure_store_root()?;
        let blobs = self.root.join("blobs");
        ensure_real_directory(&blobs, "blob store")?;
        let shard = blobs.join(&sha256[..2]);
        ensure_optional_real_directory(&shard, "blob shard")?;
        let blob = shard.join(sha256);
        ensure_optional_real_file(&blob, "package blob")?;
        Ok(blob)
    }

    fn blob_path_for_write(&self, sha256: &str) -> Result<PathBuf> {
        validate_sha256(sha256, "blob hash")?;
        self.ensure_store_root()?;
        let blobs = self.root.join("blobs");
        ensure_real_directory(&blobs, "blob store")?;
        let shard = blobs.join(&sha256[..2]);
        ensure_or_create_real_directory(&shard, "blob shard")?;
        let blob = shard.join(sha256);
        ensure_optional_real_file(&blob, "package blob")?;
        Ok(blob)
    }

    fn validate_package_for_write(&self, id: &PackageId) -> Result<()> {
        self.ensure_store_root()?;
        let packages = self.root.join("packages");
        ensure_real_directory(&packages, "package store")?;
        reject_package_case_alias(&packages, id)?;
        let package = self.package_dir(id);
        if ensure_optional_real_directory(&package, "package directory")? {
            ensure_optional_real_file(&package.join(MANIFEST_FILE), "package manifest")?;
            validate_package_directory_layout(&package, id)?;
        }
        Ok(())
    }

    fn manifest_path_for_write(&self, id: &PackageId) -> Result<PathBuf> {
        self.ensure_store_root()?;
        let packages = self.root.join("packages");
        ensure_real_directory(&packages, "package store")?;
        reject_package_case_alias(&packages, id)?;
        let package = self.package_dir(id);
        ensure_or_create_real_directory(&package, "package directory")?;
        let manifest = package.join(MANIFEST_FILE);
        ensure_optional_real_file(&manifest, "package manifest")?;
        validate_package_directory_layout(&package, id)?;
        Ok(manifest)
    }

    fn manifest_path_for_read(&self, id: &PackageId) -> Result<PathBuf> {
        self.ensure_store_root()?;
        let packages = self.root.join("packages");
        ensure_real_directory(&packages, "package store")?;
        reject_package_case_alias(&packages, id)?;
        let package = self.package_dir(id);
        match std::fs::symlink_metadata(&package) {
            Ok(metadata) => {
                ensure_real_directory_metadata(&package, &metadata, "package directory")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(package_err(
                    "package_not_installed",
                    format!("package `{id}` is not installed"),
                ));
            }
            Err(error) => {
                return Err(user_path_err(
                    "inspect_package_directory",
                    error.to_string(),
                    &package,
                    false,
                ));
            }
        }
        let manifest = package.join(MANIFEST_FILE);
        match std::fs::symlink_metadata(&manifest) {
            Ok(metadata) if metadata.is_file() && !is_link(&metadata) => {
                validate_package_directory_layout(&package, id)?;
                Ok(manifest)
            }
            Ok(_) => Err(package_err(
                "corrupt_package_manifest",
                format!("package `{id}` manifest is not a regular file"),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(package_err(
                "package_not_installed",
                format!("package `{id}` is not installed"),
            )),
            Err(error) => Err(user_path_err(
                "inspect_package_manifest",
                error.to_string(),
                &manifest,
                false,
            )),
        }
    }
}

#[derive(Serialize)]
struct RevisionContent<'a> {
    faction: SlotId,
    files: &'a [ManifestFile],
}

fn content_revision(faction: SlotId, files: &[ManifestFile]) -> Result<String> {
    use sha2::{Digest, Sha256};

    let canonical = serde_json::to_vec(&RevisionContent { faction, files }).map_err(|error| {
        internal_err(
            "serialize_revision_content",
            "StarVault could not calculate the package revision",
            error.to_string(),
        )
    })?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

fn validate_plan(plan: &PackagePlan) -> Result<()> {
    if plan.files.is_empty() {
        return Err(package_err(
            "empty_package",
            "package contains no deployable files",
        ));
    }
    let limits = ArchiveLimits::default();
    if plan.files.len() > limits.max_entries {
        return Err(package_err(
            "package_entry_limit",
            format!("package contains more than {} files", limits.max_entries),
        ));
    }
    let mut total = 0_u64;
    let mut targets = BTreeSet::new();
    for planned in &plan.files {
        validate_manifest_path(&planned.target)?;
        if !targets.insert(planned.target.to_ascii_lowercase()) {
            return Err(pkg_err(
                &planned.target,
                "duplicate target under Windows case-insensitive rules",
            ));
        }
        let metadata = std::fs::symlink_metadata(&planned.source).map_err(|error| {
            user_path_err(
                "inspect_import_file",
                error.to_string(),
                &planned.source,
                false,
            )
        })?;
        if !metadata.file_type().is_file() || is_link(&metadata) {
            return Err(pkg_err(
                &planned.target,
                "import source must be a regular file",
            ));
        }
        if metadata.len() > limits.max_file_bytes {
            return Err(pkg_err(
                &planned.target,
                "import source exceeds the 2 GiB file limit",
            ));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| package_err("package_size_overflow", "package size overflows u64"))?;
        if total > limits.max_total_bytes {
            return Err(package_err(
                "package_total_limit",
                "package exceeds the 8 GiB uncompressed limit",
            ));
        }
    }
    Ok(())
}

fn validate_manifest(expected_id: &PackageId, manifest: &PackageManifest) -> Result<()> {
    if &manifest.id != expected_id {
        return Err(package_err(
            "manifest_identity_mismatch",
            format!(
                "manifest id `{}` does not match package directory `{expected_id}`",
                manifest.id
            ),
        ));
    }
    validate_manifest_files(&manifest.files)?;
    validate_sha256(&manifest.revision, "manifest revision")?;
    let expected_revision = content_revision(manifest.faction, &manifest.files)?;
    if manifest.revision != expected_revision {
        return Err(package_err(
            "manifest_revision_mismatch",
            "manifest revision does not match its faction and file records",
        ));
    }
    Ok(())
}

fn validate_manifest_size(size: usize) -> Result<()> {
    if size <= MAX_MANIFEST_BYTES {
        Ok(())
    } else {
        Err(package_err(
            "manifest_size_limit",
            format!(
                "package manifest exceeds the {} MiB limit",
                MAX_MANIFEST_BYTES / (1024 * 1024)
            ),
        ))
    }
}

fn validate_manifest_files(files: &[ManifestFile]) -> Result<()> {
    if files.is_empty() {
        return Err(package_err(
            "empty_package",
            "manifest contains no deployable files",
        ));
    }
    let limits = ArchiveLimits::default();
    if files.len() > limits.max_entries {
        return Err(package_err(
            "package_entry_limit",
            "manifest exceeds the package entry limit",
        ));
    }
    let mut previous_path: Option<&str> = None;
    let mut compared = BTreeSet::new();
    let mut total = 0_u64;
    for file in files {
        validate_manifest_path(&file.path)?;
        validate_sha256(&file.sha256, &file.path)?;
        if file.size > limits.max_file_bytes {
            return Err(pkg_err(&file.path, "manifest file exceeds the 2 GiB limit"));
        }
        total = total
            .checked_add(file.size)
            .ok_or_else(|| package_err("package_size_overflow", "manifest size overflows u64"))?;
        if total > limits.max_total_bytes {
            return Err(package_err(
                "package_total_limit",
                "manifest exceeds the 8 GiB total limit",
            ));
        }
        if previous_path.is_some_and(|previous| previous >= file.path.as_str()) {
            return Err(package_err(
                "unsorted_manifest",
                "manifest file records must be sorted by path and unique",
            ));
        }
        if !compared.insert(file.path.to_ascii_lowercase()) {
            return Err(pkg_err(
                &file.path,
                "duplicate path under Windows case-insensitive rules",
            ));
        }
        previous_path = Some(&file.path);
    }
    Ok(())
}

fn validate_manifest_path(path: &str) -> Result<()> {
    let limits = ArchiveLimits::default();
    if path.is_empty()
        || path.len() > limits.max_path_bytes
        || path.contains(['\\', ':', '\0'])
        || !(path.starts_with("slot/") || path.starts_with("mods/"))
    {
        return Err(pkg_err(path, "invalid canonical package path"));
    }
    for segment in path.split('/') {
        if !is_safe_package_path_segment(segment) {
            return Err(pkg_err(path, "unsafe canonical package path segment"));
        }
    }
    Ok(())
}

fn validate_sha256(value: &str, context: &str) -> Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(pkg_err(context, "expected a lowercase SHA-256 value"))
    }
}

fn validate_managed_mods(managed_mods: &[ManagedMod]) -> Result<()> {
    let mut paths = BTreeSet::new();
    for managed in managed_mods {
        validate_managed_mod(managed)?;
        if !paths.insert(managed.path.to_ascii_lowercase()) {
            return Err(package_err(
                "duplicate_managed_mod",
                "managed Mods paths must be unique under Windows rules",
            ));
        }
    }
    Ok(())
}

fn verify_managed_mods_manifest(
    manifest: &PackageManifest,
    managed_mods: &[ManagedMod],
) -> Result<()> {
    validate_managed_mods(managed_mods)?;
    let expected: BTreeSet<(String, String)> = manifest
        .files
        .iter()
        .filter_map(|file| {
            file.path
                .strip_prefix("mods/")
                .map(|path| (path.to_ascii_lowercase(), file.sha256.clone()))
        })
        .collect();
    let actual: BTreeSet<(String, String)> = managed_mods
        .iter()
        .map(|managed| (managed.path.to_ascii_lowercase(), managed.sha256.clone()))
        .collect();
    if actual != expected || actual.len() != managed_mods.len() {
        return Err(package_err(
            "managed_mods_manifest_mismatch",
            "the managed Mods ledger does not match the active package manifest",
        ));
    }
    Ok(())
}

fn validate_managed_mod(managed: &ManagedMod) -> Result<()> {
    validate_manifest_path(&format!("mods/{}", managed.path))?;
    validate_sha256(&managed.sha256, &managed.path)
}

fn verify_blob(path: &Path, expected_hash: &str, expected_size: u64) -> Result<()> {
    verify_blob_metadata(path, expected_size)?;
    let actual_hash = hash_file(path)?;
    if actual_hash != expected_hash {
        return Err(package_err(
            "corrupt_package_blob",
            "stored package content failed SHA-256 verification",
        ));
    }
    Ok(())
}

fn verify_blob_metadata(path: &Path, expected_size: u64) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        package_err(
            "missing_package_blob",
            format!("required package content is unavailable: {error}"),
        )
    })?;
    if !metadata.file_type().is_file() || is_link(&metadata) || metadata.len() != expected_size {
        return Err(package_err(
            "corrupt_package_blob",
            "stored package content has the wrong file type or size",
        ));
    }
    Ok(())
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path)
        .map_err(|error| user_path_err("open_file_for_hash", error.to_string(), path, false))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| user_path_err("hash_file", error.to_string(), path, false))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn prepare_file_target(root: &Path, target: &Path) -> Result<()> {
    ensure_real_directory(root, "materialization root")?;
    let parent = target.parent().ok_or_else(|| {
        package_err(
            "invalid_materialization_path",
            "target has no parent directory",
        )
    })?;
    let relative_parent = parent.strip_prefix(root).map_err(|_| {
        package_err(
            "invalid_materialization_path",
            "target escapes the materialization root",
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative_parent.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() && !is_link(&metadata) => {}
            Ok(_) => {
                remove_path_without_following(&current)?;
                std::fs::create_dir(&current)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    if let Ok(metadata) = std::fs::symlink_metadata(target) {
        if metadata.file_type().is_dir() || is_link(&metadata) {
            remove_path_without_following(target)?;
        }
    }
    Ok(())
}

fn remove_path_without_following(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if is_link(&metadata) {
        std::fs::remove_file(path)
            .or_else(|_| std::fs::remove_dir(path))
            .map_err(Into::into)
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path).map_err(Into::into)
    } else {
        std::fs::remove_file(path).map_err(Into::into)
    }
}

fn copy_with_retry(source: &Path, destination: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..3 {
        match std::fs::copy(source, destination) {
            Ok(_) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(150 * (attempt + 1)));
                }
            }
        }
    }
    let error = last_error.expect("copy attempted at least once");
    Err(user_path_err(
        "materialize_package_file",
        error.to_string(),
        destination,
        matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
        ),
    ))
}

fn initialize_schema(conn: &rusqlite::Connection, ledger: &Path) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| ledger_error("read schema version", ledger, error))?;
    let tables = table_names(conn, ledger)?;
    if version == 0 && tables.is_empty() {
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE active_campaign (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 package_id TEXT NOT NULL,
                 revision TEXT NOT NULL,
                 faction TEXT NOT NULL
             );
             CREATE TABLE managed_mods (
                 path TEXT PRIMARY KEY COLLATE NOCASE,
                 sha256 TEXT NOT NULL,
                 disposition TEXT NOT NULL
                     CHECK (disposition IN ('created', 'borrowed'))
             );
             PRAGMA user_version = 2;
             COMMIT;",
        )
        .map_err(|error| ledger_error("create schema", ledger, error))?;
        return Ok(());
    }
    if version != STORE_SCHEMA_VERSION {
        return Err(user_path_err(
            "unsupported_store_schema",
            format!("store schema version {version} is unsupported"),
            ledger,
            false,
        ));
    }
    let expected = vec!["active_campaign".to_string(), "managed_mods".to_string()];
    if tables != expected
        || table_columns(conn, "active_campaign", ledger)?
            != ["singleton", "package_id", "revision", "faction"]
        || table_columns(conn, "managed_mods", ledger)? != ["path", "sha256", "disposition"]
    {
        return Err(user_path_err(
            "corrupt_store_schema",
            "store schema does not match version 2; no migration was attempted",
            ledger,
            false,
        ));
    }
    Ok(())
}

fn table_names(conn: &rusqlite::Connection, ledger: &Path) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .map_err(|error| ledger_error("prepare schema inventory", ledger, error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| ledger_error("query schema inventory", ledger, error))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ledger_error("read schema inventory", ledger, error))
}

fn table_columns(conn: &rusqlite::Connection, table: &str, ledger: &Path) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| ledger_error("prepare table inventory", ledger, error))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| ledger_error("query table inventory", ledger, error))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| ledger_error("read table inventory", ledger, error))
}

fn ledger_error(action: &str, path: &Path, error: rusqlite::Error) -> crate::Error {
    internal_err(
        "ledger_failure",
        "StarVault could not open its local store",
        format!("{action} {}: {error}", path.display()),
    )
}

fn ledger_query_error(action: &str, error: rusqlite::Error) -> crate::Error {
    internal_err(
        "ledger_failure",
        "StarVault could not update its local store",
        format!("{action}: {error}"),
    )
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_store_path", error.to_string(), path, false))?;
    ensure_real_directory_metadata(path, &metadata, label)
}

fn ensure_real_directory_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    label: &str,
) -> Result<()> {
    if metadata.is_dir() && !is_link(metadata) {
        Ok(())
    } else {
        Err(user_path_err(
            "unsafe_store_path",
            format!("{label} must be a real directory, not a link or reparse point"),
            path,
            false,
        ))
    }
}

fn ensure_optional_real_directory(path: &Path, label: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure_real_directory_metadata(path, &metadata, label)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_path_err(
            "inspect_store_path",
            error.to_string(),
            path,
            false,
        )),
    }
}

fn ensure_or_create_real_directory(path: &Path, label: &str) -> Result<()> {
    if ensure_optional_real_directory(path, label)? {
        return Ok(());
    }
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(user_path_err(
                "create_store_directory",
                error.to_string(),
                path,
                false,
            ));
        }
    }
    ensure_real_directory(path, label)
}

fn ensure_real_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| user_path_err("inspect_store_path", error.to_string(), path, false))?;
    if metadata.is_file() && !is_link(&metadata) {
        Ok(())
    } else {
        Err(user_path_err(
            "unsafe_store_path",
            format!("{label} must be a real file, not a link or reparse point"),
            path,
            false,
        ))
    }
}

fn ensure_optional_real_file(path: &Path, label: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => Ok(true),
        Ok(_) => Err(user_path_err(
            "unsafe_store_path",
            format!("{label} must be a real file, not a link or reparse point"),
            path,
            false,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(user_path_err(
            "inspect_store_path",
            error.to_string(),
            path,
            false,
        )),
    }
}

fn reject_package_case_alias(packages: &Path, id: &PackageId) -> Result<()> {
    for entry in read_dir_sorted(packages)? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != id.as_str() && name.eq_ignore_ascii_case(id.as_str()) {
            return Err(user_path_err(
                "package_id_case_alias",
                format!("package directory `{name}` aliases `{id}` under Windows rules"),
                entry.path(),
                false,
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn read_dir_sorted(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
    ensure_real_directory(path, "store directory")?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)
        .map_err(|error| user_path_err("read_store_directory", error.to_string(), path, false))?
    {
        entries.push(
            entry.map_err(|error| {
                user_path_err("read_store_entry", error.to_string(), path, false)
            })?,
        );
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn validate_package_directory_layout(package: &Path, id: &PackageId) -> Result<()> {
    for entry in read_dir_sorted(package)? {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
            user_path_err("inspect_package_directory", error.to_string(), &path, false)
        })?;
        if entry.file_name() != std::ffi::OsStr::new(MANIFEST_FILE)
            || !metadata.is_file()
            || is_link(&metadata)
        {
            return Err(package_err(
                "corrupt_package_directory",
                format!(
                    "package `{id}` contains an unexpected entry; only a real {MANIFEST_FILE} file is allowed"
                ),
            ));
        }
    }
    Ok(())
}

/// Validate an owned, optional directory tree without following any link or
/// Windows reparse point. Callers use this as a complete preflight before the
/// first removal so discovering one unsafe tree cannot produce a partial
/// package removal.
fn validate_real_tree_for_removal(path: &Path, label: &str) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(user_path_err(
                "inspect_store_path",
                error.to_string(),
                path,
                false,
            ));
        }
    };
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(user_path_err(
            "unsafe_store_path",
            format!("{label} must be a real directory without links or reparse points"),
            path,
            false,
        ));
    }
    for entry in read_dir_sorted(path)? {
        let child = entry.path();
        let child_metadata = std::fs::symlink_metadata(&child).map_err(|error| {
            user_path_err("inspect_store_path", error.to_string(), &child, false)
        })?;
        if is_link(&child_metadata) {
            return Err(user_path_err(
                "unsafe_store_path",
                format!("{label} contains a link or reparse point"),
                &child,
                false,
            ));
        }
        if child_metadata.is_dir() {
            validate_real_tree_for_removal(&child, label)?;
        } else if !child_metadata.is_file() {
            return Err(user_path_err(
                "unsafe_store_path",
                format!("{label} contains an unsupported filesystem entry"),
                &child,
                false,
            ));
        }
    }
    Ok(())
}

fn remove_real_tree_for_removal(path: &Path, label: &str) -> Result<()> {
    validate_real_tree_for_removal(path, label)?;
    if !ensure_optional_real_directory(path, label)? {
        return Ok(());
    }
    remove_validated_real_tree(path, label)
}

fn remove_validated_real_tree(path: &Path, label: &str) -> Result<()> {
    ensure_real_directory(path, label)?;
    for entry in read_dir_sorted(path)? {
        let child = entry.path();
        let metadata = std::fs::symlink_metadata(&child).map_err(|error| {
            user_path_err("inspect_store_path", error.to_string(), &child, false)
        })?;
        if is_link(&metadata) {
            return Err(user_path_err(
                "unsafe_store_path",
                format!("{label} contains a link or reparse point"),
                &child,
                false,
            ));
        }
        if metadata.is_dir() {
            remove_validated_real_tree(&child, label)?;
        } else if metadata.is_file() {
            std::fs::remove_file(&child).map_err(|error| {
                user_path_err("remove_store_file", error.to_string(), &child, false)
            })?;
        } else {
            return Err(user_path_err(
                "unsafe_store_path",
                format!("{label} contains an unsupported filesystem entry"),
                &child,
                false,
            ));
        }
    }
    ensure_real_directory(path, label)?;
    std::fs::remove_dir(path)
        .map_err(|error| user_path_err("remove_store_directory", error.to_string(), path, false))
}

fn sync_directory(path: &Path) -> Result<()> {
    ensure_real_directory(path, "blob shard")?;
    #[cfg(unix)]
    {
        std::fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                user_path_err("sync_blob_directory", error.to_string(), path, false)
            })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn remove_blob_temporary(staging_dir: &Path, temporary: &Path) -> Result<()> {
    ensure_real_directory(staging_dir, "blob staging")?;
    if temporary.parent() != Some(staging_dir) {
        return Err(internal_err(
            "invalid_blob_staging",
            "StarVault could not clean up an incomplete package import",
            temporary.display().to_string(),
        ));
    }
    match std::fs::symlink_metadata(temporary) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(user_path_err(
            "inspect_blob_staging",
            error.to_string(),
            temporary,
            false,
        )),
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => {
            ensure_real_directory(staging_dir, "blob staging")?;
            ensure_real_file(temporary, "staged blob")?;
            std::fs::remove_file(temporary).map_err(|error| {
                user_path_err("remove_blob_staging", error.to_string(), temporary, false)
            })
        }
        Ok(_) => Err(user_path_err(
            "unsafe_store_path",
            "staged blob must be a real file, not a link or reparse point",
            temporary,
            false,
        )),
    }
}

fn remove_if_empty(path: &Path) -> Result<()> {
    if !ensure_optional_real_directory(path, "store directory")? {
        return Ok(());
    }
    let entries = std::fs::read_dir(path)?;
    if entries.count() == 0 {
        std::fs::remove_dir(path)?;
    }
    Ok(())
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}
