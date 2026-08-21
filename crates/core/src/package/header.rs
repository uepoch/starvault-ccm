//! `DocumentHeader` binary dependency reader.
//!
//! Format (verified against real containers from `example.zip`):
//! - bytes 0..4: magic `"H2CS"`
//! - byte 44..48: dependency count, little-endian u32
//! - byte 48..: that many null-terminated UTF-8 strings
//!
//! The matching XML declarations live in `DocumentInfo`; ingest cross-checks
//! both and refuses mismatches (see package-model.md). This module reads the
//! binary side only.

use crate::error::{pkg_err, Result};

pub const MAGIC: [u8; 4] = [0x48, 0x32, 0x43, 0x53]; // "H2CS"
const COUNT_OFFSET: usize = 44;
const DEPS_OFFSET: usize = 48;
/// Sanity cap; real containers have single digits. Guards against corrupt data.
const MAX_SANE_DEPENDENCIES: u32 = 4096;

/// Read the dependency list from raw `DocumentHeader` bytes.
pub fn read_dependencies(bytes: &[u8]) -> Result<Vec<String>> {
    if bytes.len() < DEPS_OFFSET || bytes[0..4] != MAGIC {
        return Err(pkg_err("DocumentHeader", "missing H2CS header"));
    }

    let count = u32::from_le_bytes([
        bytes[COUNT_OFFSET],
        bytes[COUNT_OFFSET + 1],
        bytes[COUNT_OFFSET + 2],
        bytes[COUNT_OFFSET + 3],
    ]);
    if count > MAX_SANE_DEPENDENCIES {
        return Err(pkg_err(
            "DocumentHeader",
            format!("unreasonable dependency count {count}"),
        ));
    }

    let mut deps = Vec::with_capacity(count as usize);
    let mut offset = DEPS_OFFSET;
    for index in 0..count {
        let Some(end) = bytes[offset..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| offset + p)
        else {
            return Err(pkg_err(
                "DocumentHeader",
                format!("dependency {index} is not null-terminated"),
            ));
        };
        deps.push(String::from_utf8_lossy(&bytes[offset..end]).into_owned());
        offset = end + 1;
    }
    Ok(deps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic header the way the game's tooling does:
    /// 48-byte prefix, count at 44, then null-terminated strings.
    fn synth(deps: &[&str]) -> Vec<u8> {
        let mut bytes = vec![0u8; DEPS_OFFSET];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[COUNT_OFFSET..DEPS_OFFSET].copy_from_slice(&(deps.len() as u32).to_le_bytes());
        for dep in deps {
            bytes.extend_from_slice(dep.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    /// Real-world shape from `example.zip`'s tarcade.SC2Map.
    const TARCADE_DEPS: [&str; 2] = [
        r"file:Mods\kit_liberty_story.SC2Mod",
        r"file:Mods\RaynorRogue.SC2Mod",
    ];

    #[test]
    fn reads_real_world_dependency_list() {
        let deps = read_dependencies(&synth(&TARCADE_DEPS)).unwrap();
        assert_eq!(deps, TARCADE_DEPS);
    }

    #[test]
    fn nested_dependency_paths_survive_round_trip() {
        // The case old CCM broke: nested Mods path declared by RaynorRogue.
        let deps = read_dependencies(&synth(&[r"file:Mods\SCORE\SCORE-Other.SC2Mod"])).unwrap();
        assert_eq!(deps[0], r"file:Mods\SCORE\SCORE-Other.SC2Mod");
    }

    #[test]
    fn zero_dependencies_is_valid() {
        assert!(read_dependencies(&synth(&[])).unwrap().is_empty());
    }

    #[test]
    fn missing_magic_is_a_package_error() {
        let mut bytes = synth(&["x"]);
        bytes[0] = b'X';
        assert!(matches!(
            read_dependencies(&bytes),
            Err(crate::error::Error::Package(_))
        ));
    }

    #[test]
    fn unterminated_dependency_is_an_error() {
        let mut bytes = synth(&["fine"]);
        bytes.truncate(bytes.len() - 1); // drop the null terminator
        assert!(read_dependencies(&bytes).is_err());
    }
}
