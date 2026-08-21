//! Game launch: verify-only pre-flight, then detached spawn (decision X1).
//!
//! Launching never mutates slots or deployments. Implementation completes in
//! M3; the pre-flight contract is fixed here so the UI can build against it.

/// Result of the pre-launch verification pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    pub exe_ok: bool,
    pub no_running_instance: bool,
    /// Human-readable description of any drift found in slots/Mods.
    pub drift: Vec<String>,
}

impl PreflightReport {
    pub fn ok(&self) -> bool {
        self.exe_ok && self.no_running_instance && self.drift.is_empty()
    }
}
