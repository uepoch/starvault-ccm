//! Ownership of the SC2 directory contract.
//!
//! Architectural rule 2: ALL knowledge of game-directory paths lives here.
//! No other module concatenates game path strings.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Campaigns-directory siblings owned by the game's own campaign slots —
/// never user content, never migration candidates, never counted as mod
/// files. Single source: slots (WoL reset), launch (drift check), library
/// (migration scan) all read this.
pub const SLOT_OWNED_SIBLINGS: [&str; 4] = ["swarm", "void", "voidprologue", "nova"];

/// The four campaign slots the game reads. Order is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotId {
    Wol,
    HotS,
    LotV,
    Nco,
}

impl std::str::FromStr for SlotId {
    type Err = crate::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|slot| slot.as_str() == value)
            .ok_or_else(|| {
                crate::error::user_err("invalid_faction", format!("unknown faction `{value}`"))
            })
    }
}

impl std::fmt::Display for SlotId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl SlotId {
    pub const ALL: [SlotId; 4] = [SlotId::Wol, SlotId::HotS, SlotId::LotV, SlotId::Nco];

    /// Canonical lowercase id used in manifests and config (`wol`, `hots`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            SlotId::Wol => "wol",
            SlotId::HotS => "hots",
            SlotId::LotV => "lotv",
            SlotId::Nco => "nco",
        }
    }

    /// Human-facing name ("Wings of Liberty", …).
    pub fn title(self) -> &'static str {
        match self {
            SlotId::Wol => "Wings of Liberty",
            SlotId::HotS => "Heart of the Swarm",
            SlotId::LotV => "Legacy of the Void",
            SlotId::Nco => "Nova Covert Ops",
        }
    }
}

/// Windows install layout (the only v1 target). Owns every game path
/// the core needs; callers never build game paths themselves.
#[derive(Debug, Clone)]
pub struct WindowsLayout {
    root: PathBuf,
}

impl WindowsLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Install root, e.g. `C:\Program Files (x86)\StarCraft II`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the game executable.
    pub fn exe(&self) -> PathBuf {
        self.root.join("StarCraft II.exe")
    }

    /// Directory holding a slot's active content.
    pub fn slot_dir(&self, slot: SlotId) -> PathBuf {
        let base = self.root.join("Maps").join("Campaign");
        match slot {
            SlotId::Wol => base,
            SlotId::HotS => base.join("swarm"),
            SlotId::LotV => base.join("void"),
            SlotId::Nco => base.join("nova"),
        }
    }

    /// The shared dependency namespace root.
    pub fn mods_dir(&self) -> PathBuf {
        self.root.join("Mods")
    }

    /// Validate that `root` looks like an SC2 install (exe present).
    pub fn validate(&self) -> Result<(), crate::error::Error> {
        if self.exe().is_file() {
            Ok(())
        } else {
            Err(crate::error::Error::Environment(
                crate::error::EnvironmentError::GameNotFound,
            ))
        }
    }

    /// Reject filesystem indirection on every shared path that a campaign
    /// mutation can write through.
    ///
    /// Call this immediately before each destructive workflow phase. Missing
    /// game-owned child directories are allowed because activation can create
    /// them. Existing components must be real directories, not symlinks,
    /// junctions, or other reparse points. Dedicated slot paths are
    /// intentionally not inspected here because StarVault may deploy those as
    /// junctions; their real parent, `Maps/Campaign`, is still checked.
    pub fn validate_mutation_roots(&self) -> Result<(), crate::error::Error> {
        require_real_directory(&self.root, "configured game root")?;
        let maps = self.root.join("Maps");
        optional_real_directory(&maps, "game Maps directory")?;
        optional_real_directory(&maps.join("Campaign"), "shared campaign directory")?;
        optional_real_directory(&self.mods_dir(), "game Mods directory")?;
        Ok(())
    }
}

fn require_real_directory(path: &Path, label: &str) -> Result<(), crate::error::Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_real_directory_metadata(path, &metadata, label),
        Err(error) => Err(crate::error::user_path_err(
            "inspect_game_layout",
            format!("could not inspect {label}: {error}"),
            path,
            error.kind() == std::io::ErrorKind::Interrupted,
        )),
    }
}

fn optional_real_directory(path: &Path, label: &str) -> Result<(), crate::error::Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_real_directory_metadata(path, &metadata, label),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::error::user_path_err(
            "inspect_game_layout",
            format!("could not inspect {label}: {error}"),
            path,
            error.kind() == std::io::ErrorKind::Interrupted,
        )),
    }
}

fn validate_real_directory_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    label: &str,
) -> Result<(), crate::error::Error> {
    if metadata.is_dir() && !is_link_or_reparse_point(metadata) {
        return Ok(());
    }
    Err(crate::error::user_path_err(
        "unsafe_game_layout",
        format!("{label} must be a real directory, not a link or reparse point"),
        path,
        false,
    ))
}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Best-effort install discovery (design §install discovery): registry
/// probe first (Windows), then well-known folders. Returns the exe path.
pub fn discover_install() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(found) = discover_from_registry() {
        return Some(found);
    }
    discover_from_common_locations()
}

/// `HKLM\Software\Classes\Blizzard.SC2Save\shell\open\command` holds
/// `"…\Support\SC2Switcher.exe" "%1"`; two path segments up from the
/// switcher is the install root.
#[cfg(windows)]
fn discover_from_registry() -> Option<PathBuf> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let key = winreg::RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"Software\Classes\Blizzard.SC2Save\shell\open\command")
        .ok()?;
    let command: String = key.get_value("").ok()?;
    let switcher = command.trim_start_matches('"').split('"').next()?;
    let root = Path::new(switcher).parent()?.parent()?;
    let candidate = root.join("StarCraft II.exe");
    candidate.is_file().then_some(candidate)
}

fn discover_from_common_locations() -> Option<PathBuf> {
    [r"C:\Program Files (x86)\StarCraft II"]
        .into_iter()
        .map(|d| PathBuf::from(d).join("StarCraft II.exe"))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_paths_match_the_game_contract() {
        fn components_after(path: &Path, root: &Path) -> Vec<String> {
            path.strip_prefix(root)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        }
        let l = WindowsLayout::new("C:\\Games\\StarCraft II");
        let root = l.root().to_path_buf();
        assert_eq!(
            components_after(&l.slot_dir(SlotId::Wol), &root),
            vec!["Maps", "Campaign"]
        );
        assert_eq!(
            components_after(&l.slot_dir(SlotId::HotS), &root),
            vec!["Maps", "Campaign", "swarm"]
        );
        assert_eq!(
            components_after(&l.slot_dir(SlotId::LotV), &root),
            vec!["Maps", "Campaign", "void"]
        );
        assert_eq!(components_after(&l.mods_dir(), &root), vec!["Mods"]);
    }

    #[test]
    fn mutation_roots_accept_real_or_missing_game_owned_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temporary.path().join("sc2"));
        std::fs::create_dir_all(layout.root()).unwrap();

        layout.validate_mutation_roots().unwrap();
        std::fs::create_dir_all(layout.slot_dir(SlotId::Wol)).unwrap();
        std::fs::create_dir_all(layout.mods_dir()).unwrap();
        layout.validate_mutation_roots().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mutation_roots_reject_linked_root_and_shared_children() {
        use std::os::unix::fs::symlink;

        fn assert_rejected(layout: &WindowsLayout, expected_path: &Path) {
            let error = layout.validate_mutation_roots().unwrap_err();
            assert_eq!(error.code(), "unsafe_game_layout");
            assert_eq!(error.path(), Some(expected_path));
        }

        let temporary = tempfile::tempdir().unwrap();
        let external_root = temporary.path().join("external-root");
        std::fs::create_dir_all(&external_root).unwrap();
        let linked_root = temporary.path().join("linked-root");
        symlink(&external_root, &linked_root).unwrap();
        assert_rejected(&WindowsLayout::new(&linked_root), &linked_root);

        for component in ["Maps", "Campaign", "Mods"] {
            let root = temporary.path().join(format!("sc2-{component}"));
            std::fs::create_dir_all(&root).unwrap();
            let external = temporary.path().join(format!("external-{component}"));
            std::fs::create_dir_all(&external).unwrap();
            let linked = match component {
                "Maps" => root.join("Maps"),
                "Campaign" => {
                    std::fs::create_dir(root.join("Maps")).unwrap();
                    root.join("Maps/Campaign")
                }
                "Mods" => root.join("Mods"),
                _ => unreachable!(),
            };
            symlink(&external, &linked).unwrap();
            assert_rejected(&WindowsLayout::new(&root), &linked);
        }
    }

    #[cfg(unix)]
    #[test]
    fn mutation_roots_preserve_valid_dedicated_slot_links() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let layout = WindowsLayout::new(temporary.path().join("sc2"));
        std::fs::create_dir_all(layout.slot_dir(SlotId::Wol)).unwrap();
        std::fs::create_dir_all(layout.mods_dir()).unwrap();
        let deployment = temporary.path().join("deployment");
        std::fs::create_dir_all(&deployment).unwrap();
        symlink(&deployment, layout.slot_dir(SlotId::LotV)).unwrap();

        layout.validate_mutation_roots().unwrap();
    }
}
