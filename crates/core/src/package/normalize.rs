//! Package normalization: arbitrary layouts → canonical form.
//!
//! Canonical form splits every package into two subtrees (decision M2):
//! - `slot/<name>.SC2Map/…` — what a campaign slot receives
//! - `mods/<relative path>/…` — what mirrors into the game's `Mods\`,
//!   nesting preserved (`Mods/SCORE/X.SC2Mod` stays nested; the case old
//!   CCM broke)
//!
//! Accepted input layouts (decision K4), all proven against real packages:
//! - game-mirror: `Maps/campaign/tarcade.SC2Map/…`, `Mods/…` at root
//! - CCM-flat:    `tarcade.SC2Map/…`, `Mods/…` at root
//! - wrapped:     any single wrapper folder around either of the above

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{pkg_err, Result};
use crate::package::container;
use crate::package::metadata::{LegacyMetadata, SlotGuess};

/// One planned output file: absolute source path → canonical relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub source: PathBuf,
    /// Canonical path inside the package, starting with `slot/` or `mods/`.
    pub target: String,
}

/// A dependency declared by some container, by logical path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalDependency {
    /// Path as maps reference it, e.g. `Mods\SCORE\SCORE-Other.SC2Mod`.
    pub reference: String,
}

#[derive(Debug, Clone)]
pub struct PackagePlan {
    pub metadata: Option<LegacyMetadata>,
    pub slot_guess: SlotGuess,
    pub warnings: Vec<String>,
    /// Sorted by target for deterministic manifests.
    pub files: Vec<PlannedFile>,
    pub dependencies: Vec<LogicalDependency>,
}

const CONTAINER_EXT: &[(&str, bool)] = &[("sc2map", true), ("sc2mod", false)]; // (ext, is_map)

fn container_kind(path: &Path) -> Option<bool> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    CONTAINER_EXT
        .iter()
        .find(|(ext, _)| name.ends_with(ext))
        .map(|(_, is_map)| *is_map)
}

/// Walk an extracted package tree and produce the normalization plan.
///
/// `root` is the extraction destination (which may itself be the wrapper).
pub fn plan_from_extracted(root: &Path) -> Result<PackagePlan> {
    let all_files = collect_files(root)?;

    // --- discover containers -------------------------------------------------
    // A packed container is a file with a container extension; a directory
    // container is found by walking up from any member file until a
    // container-named ancestor is reached.
    let mut collapsed: BTreeMap<PathBuf, bool> = BTreeMap::new();
    for file in &all_files {
        let mut ancestor: &Path = file;
        loop {
            match container_kind(ancestor) {
                Some(is_map) => {
                    collapsed.insert(ancestor.to_path_buf(), is_map);
                    break;
                }
                None => match ancestor.parent() {
                    Some(parent) => ancestor = parent,
                    None => break, // reached extraction root without a container
                },
            }
        }
    }

    if collapsed.is_empty() {
        return Err(pkg_err(
            root.display().to_string(),
            "no .SC2Map or .SC2Mod containers were found",
        ));
    }

    // --- wrapper prefix detection -------------------------------------------
    let metadata_path = all_files.iter().find(|f| {
        f.file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("metadata.txt"))
    });
    let wrapper: PathBuf = match &metadata_path {
        Some(meta) => meta.parent().expect("metadata has a parent").to_path_buf(),
        None => common_container_parent(&collapsed),
    };

    // --- map every file to its canonical target ------------------------------
    // Directory containers claim their members: a file inside container C
    // maps to canonical_target(C) + its path relative to C. Loose files map
    // by their own package-relative path.
    let mut warnings: Vec<String> = Vec::new();
    let mut files: Vec<PlannedFile> = Vec::new();
    let mut seen_targets: BTreeMap<String, PathBuf> = BTreeMap::new();

    // Only directory containers can contain member files.
    let dir_containers: Vec<(&Path, bool)> = collapsed
        .iter()
        .filter(|(p, _)| p.is_dir())
        .map(|(p, is_map)| (p.as_path(), *is_map))
        .collect();

    for file in &all_files {
        // Longest directory-container ancestor, if any.
        let owner = dir_containers
            .iter()
            .filter(|(c, _)| file.starts_with(c))
            .max_by_key(|(c, _)| c.as_os_str().len());

        let target = match owner {
            Some((container, is_map)) => {
                let rel_inside = file.strip_prefix(container).expect("owner contains file");
                if rel_inside.as_os_str().is_empty() {
                    continue;
                }
                format!(
                    "{}/{}",
                    canonical_container_target(container, *is_map, &wrapper),
                    rel_inside.to_string_lossy()
                )
            }
            None => {
                let rel = file.strip_prefix(&wrapper).unwrap_or(file);
                if rel.as_os_str().is_empty() {
                    continue; // the metadata file itself
                }
                map_loose_path(rel)
            }
        };

        if let Some(existing) = seen_targets.get(&target) {
            // Identical content colliding on one target is deduplicated;
            // differing content is a hard error naming both sources.
            match same_content(existing, file) {
                Some(true) => continue,
                Some(false) => {
                    return Err(pkg_err(
                        target,
                        format!(
                            "collision between {} and {}",
                            existing.display(),
                            file.display()
                        ),
                    ));
                }
                None => {
                    return Err(pkg_err(target, "collision candidates unreadable"));
                }
            }
        }
        seen_targets.insert(target.clone(), file.clone());
        files.push(PlannedFile {
            source: file.clone(),
            target,
        });
    }
    files.sort_by(|a, b| a.target.cmp(&b.target));

    // --- dependency validation ----------------------------------------------
    let mut dependencies = Vec::new();
    for container_path in collapsed.keys() {
        match container::read_container_dependencies(container_path) {
            Ok(deps) => {
                for reference in deps.dependencies() {
                    validate_reference(reference, &collapsed, &mut warnings);
                    dependencies.push(LogicalDependency {
                        reference: reference.clone(),
                    });
                }
            }
            Err(e) => warnings.push(format!(
                "skipped dependency check for {}: {e}",
                container_path.display()
            )),
        }
    }
    dependencies.sort_by(|a, b| a.reference.cmp(&b.reference));
    dependencies.dedup();
    warnings.sort();
    warnings.dedup();

    // --- metadata ------------------------------------------------------------
    let metadata = metadata_path.map(|p| {
        let text = std::fs::read_to_string(p).unwrap_or_default();
        LegacyMetadata::parse(&text)
    });
    let slot_guess = metadata
        .as_ref()
        .map(LegacyMetadata::slot_guess)
        .unwrap_or(SlotGuess {
            kind: crate::package::metadata::SlotGuessKind::Unknown,
            matched_pattern: None,
        });

    Ok(PackagePlan {
        metadata,
        slot_guess,
        warnings,
        files,
        dependencies,
    })
}

/// Canonical target directory for a container, case-preserving.
///
/// - under `Mods/` prefix → `mods/<relative path>` (nesting preserved, M2)
/// - map outside Mods    → `slot/<basename>`
/// - mod outside Mods    → `mods/<basename>` (legacy CCM contract: maps
///   reference these at the Mods root)
fn canonical_container_target(container: &Path, is_map: bool, wrapper: &Path) -> String {
    let rel = container.strip_prefix(wrapper).unwrap_or(container);
    let starts_with_mods = rel
        .components()
        .next()
        .is_some_and(|c| c.as_os_str().eq_ignore_ascii_case("mods"));
    if starts_with_mods {
        let rest = rel
            .components()
            .skip(1)
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        format!("mods/{rest}")
    } else if is_map {
        format!("slot/{}", basename_string(container))
    } else {
        format!("mods/{}", basename_string(container))
    }
}

/// Canonical target for a loose file (no owning container), case-preserving.
fn map_loose_path(rel: &Path) -> String {
    let first = rel
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase());
    if first.as_deref() == Some("mods") {
        let rest = rel
            .components()
            .skip(1)
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        return format!("mods/{rest}");
    }
    // Everything else travels with the slot (readmes, Name files, …).
    format!(
        "slot/{}",
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    )
}

fn basename_string(path: &Path) -> String {
    path.file_name()
        .expect("container has a name")
        .to_string_lossy()
        .into_owned()
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .map_err(|e| pkg_err(dir.display().to_string(), format!("unreadable: {e}")))?
        {
            let entry =
                entry.map_err(|e| pkg_err(dir.display().to_string(), format!("bad entry: {e}")))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn common_container_parent(containers: &BTreeMap<PathBuf, bool>) -> PathBuf {
    let mut parents: Vec<Vec<String>> = containers
        .keys()
        .map(|c| {
            c.parent()
                .map(|p| {
                    p.components()
                        .map(|s| s.as_os_str().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    parents.sort();

    let first = parents.first().cloned().unwrap_or_default();
    let mut common: Vec<String> = Vec::new();
    'outer: for (i, component) in first.iter().enumerate() {
        for parts in &parents {
            if parts.get(i) != Some(component) {
                break 'outer;
            }
        }
        common.push(component.clone());
    }
    // Trailing Maps/Mods segments are layout artifacts, not wrappers.
    while matches!(
        common.last().map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("maps" | "mods")
    ) {
        common.pop();
    }
    common.iter().collect()
}

fn validate_reference(
    reference: &str,
    containers: &BTreeMap<PathBuf, bool>,
    warnings: &mut Vec<String>,
) {
    let lower = reference.to_ascii_lowercase();
    if lower.starts_with("bnet:") {
        return; // Battle.net fallback, resolves in-game
    }
    let Some(rest) = lower.strip_prefix("file:") else {
        return; // unknown scheme: leave untouched, matching flat-waterfall
    };
    let basename = rest.rsplit(['\\', '/']).next().unwrap_or("");
    if INSTALLED_CAMPAIGN_MODS.contains(&basename) {
        return; // resolves from the game install
    }
    let bundled = containers.keys().any(|c| {
        c.file_name()
            .is_some_and(|n| n.to_string_lossy().to_ascii_lowercase() == basename)
    });
    if !bundled {
        warnings.push(format!(
            "unresolved dependency `{reference}`: not bundled in package"
        ));
    }
}

/// Blizzard campaign mods ship with the installation; references to them
/// resolve locally (same set as flat-waterfall).
const INSTALLED_CAMPAIGN_MODS: [&str; 6] = [
    "liberty.sc2mod",
    "swarm.sc2mod",
    "void.sc2mod",
    "voidprologue.sc2mod",
    "voidepilogue.sc2mod",
    "novac.sc2mod",
];

/// Byte-identical check: size first (cheap), then SHA-256.
fn same_content(a: &Path, b: &Path) -> Option<bool> {
    let len_a = std::fs::metadata(a).ok()?.len();
    let len_b = std::fs::metadata(b).ok()?.len();
    if len_a != len_b {
        return Some(false);
    }
    Some(hash_file(a).ok()? == hash_file(b).ok()?)
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}
