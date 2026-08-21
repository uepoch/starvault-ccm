//! Installed-package scanning and old-CCM migration detection.
//! Implementation lands in M1.

/// Detects a legacy SC2CCM configuration at
/// `%APPDATA%\SC2CCM\SC2CCM.txt` (decision P2: explicit migration flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyCcmInstall {
    /// First line of the old config: path to StarCraft II.exe.
    pub exe_hint: Option<String>,
}
