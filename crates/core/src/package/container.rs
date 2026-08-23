//! Container-level operations.
//!
//! A container is a `.SC2Map` or `.SC2Mod` in either form: a directory tree
//! with loose internal files, or a single-file MPQ archive. Dependency
//! declarations live in `DocumentHeader` (binary) and `DocumentInfo` (XML);
//! both must be present and must agree before a package is accepted.

use std::path::Path;

use crate::error::{pkg_err, Result};
use crate::mpq::MpqArchive;

pub const DOCUMENT_HEADER: &str = "DocumentHeader";
pub const DOCUMENT_INFO: &str = "DocumentInfo";

/// The two dependency declarations for one container, after cross-checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerDependencies {
    pub header: Vec<String>,
    pub info: Vec<String>,
}

impl ContainerDependencies {
    /// The agreed dependency list (both sources matched).
    pub fn dependencies(&self) -> &Vec<String> {
        &self.header
    }
}

/// Read and cross-check the dependency declarations of any container form.
pub fn read_container_dependencies(path: &Path) -> Result<ContainerDependencies> {
    if path.is_dir() {
        read_directory_dependencies(path)
    } else {
        read_packed_dependencies(path)
    }
}

fn read_directory_dependencies(dir: &Path) -> Result<ContainerDependencies> {
    let context = dir.display().to_string();
    let header = std::fs::read(dir.join(DOCUMENT_HEADER)).map_err(|e| {
        pkg_err(
            context.clone(),
            format!("missing/unreadable {DOCUMENT_HEADER}: {e}"),
        )
    })?;
    let info = std::fs::read(dir.join(DOCUMENT_INFO)).map_err(|e| {
        pkg_err(
            context.clone(),
            format!("missing/unreadable {DOCUMENT_INFO}: {e}"),
        )
    })?;
    finish(context, header, info)
}

fn read_packed_dependencies(archive_path: &Path) -> Result<ContainerDependencies> {
    let context = archive_path.display().to_string();
    let mut archive = MpqArchive::open(archive_path)?;

    let header_name = archive
        .find_case_insensitive(DOCUMENT_HEADER)?
        .ok_or_else(|| pkg_err(context.clone(), format!("missing {DOCUMENT_HEADER}")))?;
    let info_name = archive
        .find_case_insensitive(DOCUMENT_INFO)?
        .ok_or_else(|| pkg_err(context.clone(), format!("missing {DOCUMENT_INFO}")))?;

    let header = archive.read(&header_name)?;
    let info = archive.read(&info_name)?;
    finish(context, header, info)
}

fn finish(
    context: String,
    header_bytes: Vec<u8>,
    info_bytes: Vec<u8>,
) -> Result<ContainerDependencies> {
    let header = super::header::read_dependencies(&header_bytes)
        .map_err(|e| format_error(&context, "DocumentHeader", e))?;
    let info = super::docinfo::read_dependencies_from_bytes(&info_bytes)
        .map_err(|e| pkg_err(context.clone(), format!("malformed DocumentInfo: {e}")))?;

    if header != info {
        return Err(pkg_err(
            context,
            format!(
                "DocumentHeader and DocumentInfo disagree:\n  header: {header:?}\n  info:   {info:?}"
            ),
        ));
    }

    Ok(ContainerDependencies { header, info })
}

fn format_error(context: &str, what: &str, e: crate::Error) -> crate::Error {
    match e {
        crate::Error::Package(mut package) => {
            package.context = Some(match package.context {
                Some(inner) => format!("{context}:{inner}"),
                None => context.to_string(),
            });
            crate::Error::Package(package)
        }
        other => pkg_err(context.to_string(), format!("{what}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn reads_packed_container_dependencies() {
        // RandomBuff.SC2Mod declares only a bnet fallback (zhCN locale).
        let deps = read_container_dependencies(&fixture("RandomBuff.SC2Mod")).unwrap();
        assert_eq!(deps.dependencies().len(), 1);
        assert!(deps.dependencies()[0].starts_with("bnet:"));
    }

    #[test]
    fn reads_directory_container_dependencies() {
        // Assemble a directory container from loose fixtures.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("RaynorRogue.SC2Mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            fixture("raynorrogue.DocumentHeader"),
            dir.join("DocumentHeader"),
        )
        .unwrap();
        std::fs::copy(
            fixture("raynorrogue.DocumentInfo"),
            dir.join("DocumentInfo"),
        )
        .unwrap();

        let deps = read_container_dependencies(&dir).unwrap();
        assert!(deps
            .dependencies()
            .iter()
            .any(|d| d.contains(r"SCORE\SCORE-Other.SC2Mod")));
    }

    #[test]
    fn mismatched_declarations_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Broken.SC2Mod");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(
            fixture("raynorrogue.DocumentHeader"),
            dir.join("DocumentHeader"),
        )
        .unwrap();
        std::fs::copy(fixture("tarcade.DocumentInfo"), dir.join("DocumentInfo")).unwrap();

        assert!(read_container_dependencies(&dir).is_err());
    }

    #[test]
    fn missing_metadata_is_a_package_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Empty.SC2Mod");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            read_container_dependencies(&dir),
            Err(crate::Error::Package(_))
        ));
    }
}
