//! App configuration: `%APPDATA%\StarVault\CCM\config.toml`.
//!
//! TOML, versioned, human-editable. The original tool's "first line of a text
//! file" config is gone (see architectural review).

/// User settings persisted across runs. Defaults are sensible; every field
/// is overridable in Settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Validated path to StarCraft II.exe.
    pub game_exe: Option<std::path::PathBuf>,
    /// Slot switching strategy override; None = auto (junction-first).
    pub strategy_override: Option<StrategyChoice>,
    /// Opt-in crash reporting only (decision S3). No analytics exist.
    pub crash_reports_opt_in: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyChoice {
    Junction,
    Copy,
}
