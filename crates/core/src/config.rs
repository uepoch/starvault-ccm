//! App configuration: `%APPDATA%\StarVault\CCM\config.toml`.
//!
//! TOML, versioned, human-editable. The original tool's "first line of a text
//! file" config is gone (see architectural review).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{pkg_err, Result};
use crate::filesystem::is_link_or_reparse;
use crate::identity::ProfileId;

/// User settings persisted across runs. Defaults are sensible; every field
/// is overridable in Settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Validated path to StarCraft II.exe.
    pub game_exe: Option<std::path::PathBuf>,
    /// Slot switching strategy override; None = auto (junction-first).
    pub strategy_override: Option<StrategyChoice>,
    /// Opt-in crash reporting only (decision S3).
    pub crash_reports_opt_in: bool,
    /// Opt-out anonymized usage analytics (Aptabase, EU).
    pub analytics_enabled: bool,
    /// The first-launch analytics disclaimer has been acknowledged.
    pub analytics_acknowledged: bool,
    /// Minimum operation-log level recorded: `info`, `warn`, or `error`.
    pub log_level: String,
    /// Experimental: isolate campaign saves per active package.
    pub save_isolation: bool,
    /// Opaque discovered profile token when isolation is on.
    pub saves_profile: Option<ProfileId>,
    /// Allow campaign activation to replace differing external Mods files.
    pub replace_external_mods: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            game_exe: None,
            strategy_override: None,
            crash_reports_opt_in: false,
            analytics_enabled: true,
            analytics_acknowledged: false,
            log_level: "info".into(),
            save_isolation: false,
            saves_profile: None,
            replace_external_mods: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrategyChoice {
    Junction,
    Copy,
}

impl Config {
    /// Load a config; a missing file yields the default (first run).
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(pkg_err(path.display().to_string(), error.to_string())),
        };
        if !metadata.is_file() || is_link_or_reparse(&metadata) {
            return Err(pkg_err(
                path.display().to_string(),
                "configuration path must be a regular file",
            ));
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => return Err(pkg_err(path.display().to_string(), error.to_string())),
        };
        toml::from_str(&text)
            .map_err(|e| pkg_err(path.display().to_string(), format!("parse: {e}")))
    }

    /// Persist the config, creating parent directories.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let text = toml::to_string_pretty(self)
            .map_err(|e| pkg_err(path.display().to_string(), e.to_string()))?;
        crate::atomic_file::write(path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");

        assert_eq!(Config::load(&path).unwrap(), Config::default());

        let cfg = Config {
            game_exe: Some(std::path::PathBuf::from("C:\\Games\\SC2\\StarCraft II.exe")),
            strategy_override: Some(StrategyChoice::Copy),
            crash_reports_opt_in: true,
            ..Default::default()
        };
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
    }

    #[test]
    fn tolerates_unknown_keys_for_forward_compat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "future_field = 42\n").unwrap();
        // Unknown top-level keys are ignored; missing ones fall back to defaults.
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }

    #[test]
    fn atomic_save_replaces_the_existing_file_without_temporary_debris() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "log_level = \"warn\"\n").unwrap();

        Config {
            log_level: "error".into(),
            ..Config::default()
        }
        .save(&path)
        .unwrap();

        assert_eq!(Config::load(&path).unwrap().log_level, "error");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn linked_config_is_rejected_without_changing_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let external = dir.path().join("external.toml");
        let linked = dir.path().join("config.toml");
        std::fs::write(&external, "log_level = \"warn\"\n").unwrap();
        symlink(&external, &linked).unwrap();

        assert!(Config::load(&linked).is_err());
        Config::default().save(&linked).unwrap();
        assert_eq!(
            std::fs::read_to_string(&external).unwrap(),
            "log_level = \"warn\"\n"
        );
        assert!(!linked.symlink_metadata().unwrap().file_type().is_symlink());
    }
}
