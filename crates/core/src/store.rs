//! Content-addressed blob store, package manifests, and deployment ledger.
//!
//! Layout (docs/design/dependency-store.md):
//!
//! ```text
//! <root>/
//!   blobs/<ab>/<sha256…>              every unique file content, once
//!   packages/<id>/<rev>/manifest.json package revision → file list
//!   ledger.db                         SQLite, single writer
//! ```
//!
//! The store is game-agnostic: it materializes subtrees to directories it is
//! given; slot/Mods path knowledge stays in `layout` and `slots`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{pkg_err, Result};
use crate::layout::SlotId;
use crate::package::import::ImportProgress;
use crate::package::normalize::PackagePlan;

/// One file in a package revision manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    /// Canonical path inside the package: `slot/…` or `mods/…`.
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// A package revision: immutable once written.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageManifest {
    pub id: String,
    pub rev: String,
    pub slot: String,
    /// Detected metadata, when the package carried any (K2). Absent from
    /// the canonical hash so revisions stay content-addressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<String>,
    /// Unix seconds when this revision entered the library. Not part of the
    /// content hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<u64>,
    pub files: Vec<ManifestFile>,
}

/// A deployment conflict across simultaneously-active packages (decision M5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// Game-relative path under `Mods\`, lowercased for comparison.
    pub target: String,
    pub first: (String, String),
    pub second: (String, String),
}

/// One file in the union of active packages' `mods/**` subtrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModsUnionEntry<'a> {
    /// Path relative to `Mods\`, original spelling preserved.
    pub rel_path: String,
    pub file: &'a ManifestFile,
    pub owner: &'a str,
}

pub struct Store {
    root: PathBuf,
    conn: Mutex<rusqlite::Connection>,
    /// Loaded manifests by `id\0rev`. Revisions are immutable, so entries
    /// live for the process lifetime — on AV-heavy machines a single small
    /// file open can cost hundreds of ms, and nothing here ever changes.
    manifests: Mutex<HashMap<(String, String), PackageManifest>>,
}

impl Store {
    /// Open (creating if needed) a store at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("packages"))?;
        let conn = rusqlite::Connection::open(root.join("ledger.db"))
            .map_err(|e| pkg_err(root.display().to_string(), format!("ledger open: {e}")))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS active_slots (
                 slot   TEXT PRIMARY KEY,
                 pkg_id TEXT,
                 rev    TEXT
             );
             CREATE TABLE IF NOT EXISTS deployments (
                 game_path TEXT NOT NULL,
                 sha256    TEXT NOT NULL,
                 rev       TEXT NOT NULL,
                 PRIMARY KEY (game_path, rev)
             );
             CREATE TABLE IF NOT EXISTS blob_refs (
                 sha256   TEXT PRIMARY KEY,
                 refcount INTEGER NOT NULL DEFAULT 0
             );",
        )
        .map_err(|e| pkg_err(root.display().to_string(), format!("ledger init: {e}")))?;

        Ok(Self {
            root,
            conn: Mutex::new(conn),
            manifests: Mutex::new(HashMap::new()),
        })
    }

    fn blob_path(&self, sha256: &str) -> PathBuf {
        self.root.join("blobs").join(&sha256[..2]).join(sha256)
    }

    /// Store root, for components that stage materialized trees beside it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ingest a normalization plan as package `id` targeting `slot`.
    ///
    /// Copies every planned file into the blob store (deduplicated by
    /// content), computes the content revision id, and writes the manifest.
    /// Returns the revision id.
    pub fn ingest(&self, id: &str, slot: SlotId, plan: &PackagePlan) -> Result<String> {
        self.ingest_with_progress(id, slot, plan, |_| true)
            .and_then(|rev| rev.ok_or_else(|| pkg_err(id, "ingest cancelled")))
    }

    /// Like [`Store::ingest`], reporting per-file progress. The callback
    /// runs before each file; returning `false` cancels at that boundary
    /// and yields `Ok(None)` — partial blobs are orphans reclaimed by GC.
    pub fn ingest_with_progress(
        &self,
        id: &str,
        slot: SlotId,
        plan: &PackagePlan,
        mut on_progress: impl FnMut(ImportProgress) -> bool,
    ) -> Result<Option<String>> {
        if plan.files.is_empty() {
            return Err(pkg_err(id, "plan contains no files"));
        }

        let total = plan.files.len() as u64;
        let mut files = Vec::with_capacity(plan.files.len());
        for (done, planned) in plan.files.iter().enumerate() {
            if !on_progress(ImportProgress {
                files_done: done as u64,
                files_total: total,
                current_file: planned.target.clone(),
            }) {
                return Ok(None);
            }
            let sha256 = hash_file(&planned.source)?;
            let size = std::fs::metadata(&planned.source)
                .map_err(|e| pkg_err(planned.source.display().to_string(), e.to_string()))?
                .len();
            let blob = self.blob_path(&sha256);
            if !blob.exists() {
                if let Some(parent) = blob.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Copy-then-rename so a crash never leaves a partial blob.
                let tmp = blob.with_extension("partial");
                std::fs::copy(&planned.source, &tmp)?;
                std::fs::rename(&tmp, &blob)?;
            }
            files.push(ManifestFile {
                path: planned.target.clone(),
                sha256,
                size,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        // Revision id: hash over the manifest content sans rev field.
        let mut manifest = PackageManifest {
            id: id.to_string(),
            rev: String::new(),
            slot: slot.as_str().to_string(),
            title: plan.metadata.as_ref().and_then(|m| m.title.clone()),
            author: plan.metadata.as_ref().and_then(|m| m.author.clone()),
            version: plan.metadata.as_ref().and_then(|m| m.version.clone()),
            desc: plan.metadata.as_ref().and_then(|m| m.desc.clone()),
            imported_at: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
            ),
            files,
        };
        let canonical =
            serde_json::to_string(&manifest).map_err(|e| pkg_err(id, format!("serialize: {e}")))?;
        use sha2::{Digest, Sha256};
        let rev = hex::encode(Sha256::digest(canonical.as_bytes()));
        manifest.rev = rev.clone();

        let dir = self.root.join("packages").join(id).join(&rev);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("manifest.json");
        serde_json::to_writer_pretty(
            std::fs::File::create(&path)
                .map_err(|e| pkg_err(path.display().to_string(), e.to_string()))?,
            &manifest,
        )
        .map_err(|e| pkg_err(id, format!("write manifest: {e}")))?;

        Ok(Some(rev))
    }

    /// Load a stored manifest.
    pub fn load_manifest(&self, id: &str, rev: &str) -> Result<PackageManifest> {
        {
            let manifests = self.manifests.lock().expect("manifest cache poisoned");
            if let Some(hit) = manifests.get(&(id.to_string(), rev.to_string())) {
                return Ok(hit.clone());
            }
        }
        let path = self
            .root
            .join("packages")
            .join(id)
            .join(rev)
            .join("manifest.json");
        let file = std::fs::File::open(&path)
            .map_err(|e| pkg_err(path.display().to_string(), e.to_string()))?;
        let manifest: PackageManifest = serde_json::from_reader(file)
            .map_err(|e| pkg_err(id, format!("parse manifest: {e}")))?;
        self.manifests
            .lock()
            .expect("manifest cache poisoned")
            .insert((id.to_string(), rev.to_string()), manifest.clone());
        Ok(manifest)
    }

    /// All installed packages as (id, rev, slot), one row per revision.
    pub fn list_packages(&self) -> Result<Vec<(String, String, String)>> {
        Ok(self
            .all_manifests()?
            .into_iter()
            .map(|m| (m.id.clone(), m.rev, m.slot))
            .collect())
    }

    /// Every stored manifest in one pass — callers get metadata for free
    /// instead of re-reading each file.
    pub fn all_manifests(&self) -> Result<Vec<PackageManifest>> {
        let mut out = Vec::new();
        let ids_dir = self.root.join("packages");
        for id in sorted_dirs(&ids_dir)? {
            for rev in sorted_dirs(&ids_dir.join(&id))? {
                if let Ok(m) = self.load_manifest(&id, &rev) {
                    out.push(m);
                }
            }
        }
        Ok(out)
    }

    /// All cross-package conflicts in a would-be union: same `Mods\` path,
    /// different bytes. `plan_mods_union` blocks on these; this collects
    /// every one so the UI can show details.
    pub fn find_conflicts(&self, manifests: &[&PackageManifest]) -> Vec<Conflict> {
        let mut by_lower: BTreeMap<String, (&str, &str)> = BTreeMap::new();
        let mut seen_targets: std::collections::BTreeSet<String> = Default::default();
        let mut out = Vec::new();
        for manifest in manifests {
            for file in &manifest.files {
                let Some(rel) = file.path.strip_prefix("mods/") else {
                    continue;
                };
                let key = rel.to_ascii_lowercase();
                match by_lower.get(&key) {
                    None => {
                        by_lower.insert(key, (manifest.id.as_str(), file.sha256.as_str()));
                    }
                    Some((owner, sha)) => {
                        if *sha != file.sha256 && seen_targets.insert(key.clone()) {
                            out.push(Conflict {
                                target: format!("Mods\\{rel}"),
                                first: ((*owner).to_string(), String::new()),
                                second: (manifest.id.clone(), String::new()),
                            });
                        }
                    }
                }
            }
        }
        out
    }

    /// Copy all `slot/**` files of a manifest into `dest`.
    pub fn materialize_slot(&self, manifest: &PackageManifest, dest: &Path) -> Result<()> {
        std::fs::create_dir_all(dest)?;
        for file in &manifest.files {
            let Some(rel) = file.path.strip_prefix("slot/") else {
                continue;
            };
            let target = dest.join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(self.blob_path(&file.sha256), &target)?;
        }
        Ok(())
    }

    /// Compute the union of `mods/**` across manifests, detecting conflicts:
    /// same target path (case-insensitive), different content (M5).
    pub fn plan_mods_union<'a>(
        &self,
        manifests: &[&'a PackageManifest],
    ) -> Result<Vec<ModsUnionEntry<'a>>> {
        // Index by lowercased path for Windows-semantics comparison; keep the
        // first-seen spelling for the on-disk layout.
        let mut by_lower: BTreeMap<String, ModsUnionEntry<'a>> = BTreeMap::new();

        for manifest in manifests {
            for file in &manifest.files {
                let Some(rel) = file.path.strip_prefix("mods/") else {
                    continue;
                };
                let key = rel.to_ascii_lowercase();
                match by_lower.get(&key) {
                    None => {
                        by_lower.insert(
                            key,
                            ModsUnionEntry {
                                rel_path: rel.to_string(),
                                file,
                                owner: manifest.id.as_str(),
                            },
                        );
                    }
                    Some(existing) => {
                        if existing.file.sha256 != file.sha256 {
                            return Err(crate::error::Error::User(crate::UserError {
                                message: format!(
                                    "dependency conflict on Mods\\{rel}: `{}` and `{}` ship different content",
                                    existing.owner, manifest.id
                                ),
                                path: None,
                            }));
                        }
                        // identical content: share silently
                    }
                }
            }
        }
        Ok(by_lower.into_values().collect())
    }

    /// Write the union's blobs into `mods_dir`, preserving relative paths.
    /// An existing entry of the OPPOSITE kind (a leftover packed `.SC2Mod`
    /// file where the package ships a directory, or vice versa) is replaced:
    /// the union is the source of truth for what is active.
    pub fn apply_mods_union(&self, union: &[ModsUnionEntry<'_>], mods_dir: &Path) -> Result<()> {
        for entry in union {
            let target = mods_dir.join(&entry.rel_path);
            if let Some(parent) = target.parent() {
                // A leftover packed .SC2Mod FILE where this package ships an
                // unpacked directory tree: remove the file so the dirs can be
                // created. The union is the source of truth for what is active.
                if parent
                    .symlink_metadata()
                    .map(|m| !m.is_dir())
                    .unwrap_or(false)
                {
                    std::fs::remove_file(parent)
                        .map_err(|e| pkg_err(parent.display().to_string(), e.to_string()))?;
                }
                std::fs::create_dir_all(parent)?;
            }
            let src = self.blob_path(&entry.file.sha256);
            copy_with_retry(&src, &target)?;
        }
        Ok(())
    }

    // --- ledger -------------------------------------------------------------

    /// Record activation of `(pkg_id, rev)` on `slot`, replacing any prior
    /// row for the slot.
    pub fn set_active_slot(&self, slot: SlotId, pkg_id: &str, rev: &str) -> Result<()> {
        let conn = self.conn.lock().expect("ledger poisoned");
        conn.execute(
            "INSERT INTO active_slots(slot, pkg_id, rev) VALUES(?1, ?2, ?3)
             ON CONFLICT(slot) DO UPDATE SET pkg_id = ?2, rev = ?3",
            rusqlite::params![slot.as_str(), pkg_id, rev],
        )
        .map_err(|e| pkg_err("ledger", e.to_string()))?;
        Ok(())
    }

    /// Clear a slot's active row.
    pub fn clear_active_slot(&self, slot: SlotId) -> Result<()> {
        let conn = self.conn.lock().expect("ledger poisoned");
        conn.execute(
            "DELETE FROM active_slots WHERE slot = ?1",
            rusqlite::params![slot.as_str()],
        )
        .map_err(|e| pkg_err("ledger", e.to_string()))?;
        Ok(())
    }

    /// Active slot rows as (slot, pkg_id, rev).
    pub fn active_slots(&self) -> Result<Vec<(String, String, String)>> {
        let conn = self.conn.lock().expect("ledger poisoned");
        let mut stmt = conn
            .prepare("SELECT slot, pkg_id, rev FROM active_slots WHERE rev IS NOT NULL")
            .map_err(|e| pkg_err("ledger", e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| pkg_err("ledger", e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| pkg_err("ledger", e.to_string()))
    }
}

/// Copy with short retries: antivirus and indexers briefly hold freshly
/// written files, which surfaces as os error 5/32 for no good reason.
fn copy_with_retry(src: &Path, dest: &Path) -> Result<()> {
    let mut last = None;
    for attempt in 0..3 {
        match std::fs::copy(src, dest) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(150 * (attempt + 1)));
            }
        }
    }
    Err(last
        .map(|e| pkg_err(dest.display().to_string(), e.to_string()))
        .unwrap_or_else(|| pkg_err(dest.display().to_string(), "copy failed")))
}

fn sorted_dirs(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(out), // missing dir = empty listing
    };
    for entry in entries {
        let entry = entry.map_err(|e| pkg_err(dir.display().to_string(), e.to_string()))?;
        if entry
            .file_type()
            .map_err(|e| pkg_err(dir.display().to_string(), e.to_string()))?
            .is_dir()
        {
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    out.sort();
    Ok(out)
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|e| pkg_err(path.display().to_string(), e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| pkg_err(path.display().to_string(), e.to_string()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

impl Store {
    /// Remove an installed package entirely: manifests, deploy trees, and
    /// any blobs no other package references. Refuses while the package is
    /// active on a slot — restore first.
    pub fn remove_package(&self, id: &str) -> Result<()> {
        let active = self.active_slots()?;
        if let Some((slot, _, _)) = active.iter().find(|(_, pkg, _)| pkg == id) {
            return Err(crate::error::Error::User(crate::UserError {
                message: format!(
                    "`{id}` is active on {slot} — restore that faction to plain before removing it"
                ),
                path: None,
            }));
        }

        // Collect this package's revisions (manifests + deploy trees).
        let pkg_dir = self.root.join("packages").join(id);
        let mut removed_revs = Vec::new();
        for rev in sorted_dirs(&pkg_dir)? {
            self.load_manifest(id, &rev)?;
            removed_revs.push(rev);
        }
        if removed_revs.is_empty() {
            return Err(pkg_err(id, "package is not installed"));
        }

        std::fs::remove_dir_all(&pkg_dir)
            .map_err(|e| pkg_err(pkg_dir.display().to_string(), e.to_string()))?;
        for rev in &removed_revs {
            // Deploy trees are named `<slot>-<rev>`; a rev could serve
            // several slots, so match on the rev suffix.
            let deploy_dir = self.root.join("deploy");
            if let Ok(entries) = std::fs::read_dir(&deploy_dir) {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .ends_with(&format!("-{rev}"))
                    {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }

        // Sweep GC: keep only blobs referenced by surviving manifests.
        let mut referenced = std::collections::BTreeSet::new();
        for manifest in self.all_manifests()? {
            for file in manifest.files {
                referenced.insert(file.sha256);
            }
        }
        let blobs_dir = self.root.join("blobs");
        for shard in sorted_dirs(&blobs_dir)? {
            let shard_dir = blobs_dir.join(&shard);
            for entry in std::fs::read_dir(&shard_dir)?.flatten() {
                let sha = entry.file_name().to_string_lossy().into_owned();
                if !referenced.contains(&sha) {
                    std::fs::remove_file(entry.path())?;
                }
            }
            if std::fs::read_dir(&shard_dir)?.next().is_none() {
                std::fs::remove_dir(&shard_dir)?;
            }
        }
        Ok(())
    }
}
