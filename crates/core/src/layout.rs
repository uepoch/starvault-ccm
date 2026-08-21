//! Ownership of the SC2 directory contract.
//!
//! Architectural rule 2: ALL knowledge of game-directory paths lives here.
//! No other module concatenates game path strings.

use std::path::{Path, PathBuf};

/// The four campaign slots the game reads. Order is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotId {
    Wol,
    HotS,
    LotV,
    Nco,
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

/// The game-directory layout contract.
///
/// Implementations produce every path the rest of the core needs; callers
/// never build game paths themselves.
pub trait GameLayout {
    /// Install root, e.g. `C:\Program Files (x86)\StarCraft II`.
    fn root(&self) -> &Path;

    /// Path to the game executable.
    fn exe(&self) -> PathBuf;

    /// Directory holding a slot's active content.
    fn slot_dir(&self, slot: SlotId) -> PathBuf;

    /// Directories that must exist for a slot to be usable, including
    /// sub-slots (`swarm\evolution`) and prologue directories.
    fn slot_dirs(&self, slot: SlotId) -> Vec<PathBuf>;

    /// The shared dependency namespace root.
    fn mods_dir(&self) -> PathBuf;
}

/// Windows install layout (the only v1 target).
#[derive(Debug, Clone)]
pub struct WindowsLayout {
    root: PathBuf,
}

impl WindowsLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
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
}

impl GameLayout for WindowsLayout {
    fn root(&self) -> &Path {
        &self.root
    }

    fn exe(&self) -> PathBuf {
        self.root.join("StarCraft II.exe")
    }

    fn slot_dir(&self, slot: SlotId) -> PathBuf {
        let base = self.root.join("Maps").join("Campaign");
        match slot {
            SlotId::Wol => base,
            SlotId::HotS => base.join("swarm"),
            SlotId::LotV => base.join("void"),
            SlotId::Nco => base.join("nova"),
        }
    }

    fn slot_dirs(&self, slot: SlotId) -> Vec<PathBuf> {
        let main = self.slot_dir(slot);
        match slot {
            SlotId::Wol | SlotId::LotV | SlotId::Nco => vec![main],
            SlotId::HotS => vec![main.clone(), main.join("evolution")],
        }
    }

    fn mods_dir(&self) -> PathBuf {
        self.root.join("Mods")
    }
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
    fn hots_has_an_evolution_subslot() {
        let l = WindowsLayout::new("C:\\SC2");
        let dirs = l.slot_dirs(SlotId::HotS);
        assert_eq!(dirs.len(), 2);
        assert!(dirs[1].ends_with("evolution"));
    }
}
