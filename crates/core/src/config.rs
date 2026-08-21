//! App configuration: `%APPDATA%\StarVault\CCM\config.toml`.
//!
//! TOML, versioned, human-editable. The original tool's "first line of a text
//! file" config is gone (see architectural review).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{pkg_err, Result};

/// User settings persisted across runs. Defaults are sensible; every field
/// is overridable in Settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Validated path to StarCraft II.exe.
    pub game_exe: Option<std::path::PathBuf>,
    /// Slot switching strategy override; None = auto (junction-first).
    pub strategy_override: Option<StrategyChoice>,
    /// Opt-in crash reporting only (decision S3). No analytics exist.
    pub crash_reports_opt_in: bool,
    /// Minimum operation-log level recorded: `info`, `warn`, or `error`.
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            game_exe: None,
            strategy_override: None,
            crash_reports_opt_in: false,
            log_level: "info".into(),
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
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(pkg_err(path.display().to_string(), e.to_string())),
        };
        toml::from_str(&text)
            .map_err(|e| pkg_err(path.display().to_string(), format!("parse: {e}")))
    }

    /// Persist the config, creating parent directories.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| pkg_err(path.display().to_string(), e.to_string()))?;
        std::fs::write(path, text)?;
        Ok(())
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
}
