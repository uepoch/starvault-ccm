//! Game launch: verify-only pre-flight, then detached spawn (decision X1).
//!
//! Launching never mutates slots or deployments. Drift found by pre-flight is
//! reported for the user to repair explicitly — never silently fixed.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::error::{pkg_err, Result};
use crate::layout::{GameLayout, WindowsLayout};
use crate::store::Store;

/// Result of the pre-launch verification pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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

/// Verify-only pass: exe present, no running instance, active slots match
/// their manifests (spot-check level), deployed Mods paths exist.
pub fn preflight(layout: &WindowsLayout, store: &Store) -> PreflightReport {
    let mut report = PreflightReport {
        exe_ok: layout.validate().is_ok(),
        no_running_instance: !sc2_running(),
        drift: Vec::new(),
    };

    for (slot, id, rev) in match store.active_slots() {
        Ok(rows) => rows,
        Err(e) => {
            report.drift.push(format!("ledger unreadable: {e}"));
            return report;
        }
    } {
        let Some(slot_id) = crate::layout::SlotId::ALL
            .into_iter()
            .find(|s| s.as_str() == slot)
        else {
            continue;
        };
        let Ok(manifest) = store.load_manifest(&id, &rev) else {
            report.drift.push(format!(
                "{slot}: manifest {id}@{} unreadable",
                &rev[..8.min(rev.len())]
            ));
            continue;
        };
        check_slot_tree(layout, slot_id, &manifest, &mut report.drift);
        check_mods_paths(layout, &manifest, &mut report.drift);
    }

    // Drop duplicate notes (e.g. one missing Mods dir reported per package).
    report.drift.sort();
    report.drift.dedup();
    report
}

/// Spot-check a slot's content against its manifest: junction targets must
/// resolve; copy trees must have the expected file count.
fn check_slot_tree(
    layout: &WindowsLayout,
    slot: crate::layout::SlotId,
    manifest: &crate::store::PackageManifest,
    drift: &mut Vec<String>,
) {
    let slot_dir = layout.slot_dir(slot);
    let expected = manifest
        .files
        .iter()
        .filter(|f| f.path.starts_with("slot/"))
        .count();

    if let Ok(meta) = std::fs::symlink_metadata(&slot_dir) {
        if meta.file_type().is_symlink() {
            match std::fs::read_link(&slot_dir) {
                Ok(target) if target.exists() => return, // junction intact
                _ => {
                    drift.push(format!("{}: junction dangling", slot.as_str()));
                    return;
                }
            }
        }
    }

    if !slot_dir.is_dir() {
        drift.push(format!("{}: slot directory missing", slot.as_str()));
        return;
    }
    let actual = count_files(&slot_dir);
    if actual != expected {
        drift.push(format!(
            "{}: {} files present, manifest expects {}",
            slot.as_str(),
            actual,
            expected
        ));
    }
}

/// Existence-level check of deployed `mods/**` paths.
fn check_mods_paths(
    layout: &WindowsLayout,
    manifest: &crate::store::PackageManifest,
    drift: &mut Vec<String>,
) {
    let mods_dir = layout.mods_dir();
    for file in &manifest.files {
        let Some(rel) = file.path.strip_prefix("mods/") else {
            continue;
        };
        if !mods_dir.join(rel).exists() {
            drift.push(format!("Mods\\{rel} missing"));
        }
    }
}

fn count_files(dir: &Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                count += 1;
            }
        }
    }
    count
}

/// Best-effort running-instance detection across platforms.
fn sc2_running() -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return false;
        };
        entries.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.chars().all(|c| c.is_ascii_digit())
                && std::fs::read_to_string(e.path().join("comm"))
                    .map(|c| {
                        let c = c.to_lowercase();
                        c.starts_with("sc2") || c.contains("starcraft")
                    })
                    .unwrap_or(false)
        })
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq SC2_x64.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("SC2_x64"))
            .unwrap_or(false)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Detached spawn: `<exe>` with no mutating arguments (X1).
pub fn launch(layout: &WindowsLayout) -> Result<()> {
    let exe = layout.exe();
    if !exe.is_file() {
        return Err(pkg_err(exe.display().to_string(), "executable not found"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new(&exe)
            .creation_flags(0x0000_0008 | 0x0000_0400) // DETACHED_PROCESS | NEW_PROCESS_GROUP
            .spawn()
            .map_err(|e| pkg_err(exe.display().to_string(), e.to_string()))?;
    }
    #[cfg(not(windows))]
    {
        Command::new(&exe)
            .spawn()
            .map_err(|e| pkg_err(exe.display().to_string(), e.to_string()))?;
    }
    Ok(())
}

/// Battle.net deep link when the local exe is unusable.
pub fn launch_battlenet() -> Result<()> {
    #[cfg(windows)]
    let status = Command::new("cmd")
        .args(["/C", "start", "", "battlenet://play"])
        .spawn();
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg("battlenet://play").spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let status = Command::new("xdg-open").arg("battlenet://play").spawn();
    status.map_err(|e| pkg_err("battlenet://play", e.to_string()))?;
    Ok(())
}
