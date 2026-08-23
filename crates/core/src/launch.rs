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
            let Ok(target) = std::fs::read_link(&slot_dir) else {
                drift.push(format!("{}: junction dangling", slot.as_str()));
                return;
            };
            if !target.exists() {
                drift.push(format!("{}: junction dangling", slot.as_str()));
                return;
            }
            // Junction resolves; fall through to the file-count check — the
            // target existing does not mean its content is intact (it can
            // be damaged through the link).
        }
    }

    if !slot_dir.is_dir() {
        drift.push(format!("{}: slot directory missing", slot.as_str()));
        return;
    }
    // WoL's slot is the shared Campaign root: sibling campaign dirs and
    // crash leftovers belong to other slots, so don't count them.
    let actual = count_files(&slot_dir, slot == crate::layout::SlotId::Wol);
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

/// Directory names under the shared Campaign root that belong to other
/// slots — never counted as WoL content.
const EXCLUDE_SIBLINGS: [&str; 4] = ["swarm", "void", "voidprologue", "nova"];

fn count_files(dir: &Path, shared_root: bool) -> usize {
    let mut count = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if shared_root && p.parent() == Some(dir) {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                // Sibling slots and crash leftovers are never WoL content.
                if EXCLUDE_SIBLINGS.contains(&name.as_str())
                    || name.contains(".backup-")
                    || name.contains(".staging-")
                {
                    continue;
                }
            }
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
/// SC2 spawned without the Battle.net agent running falls back to its
/// legacy login screen (password prompt). With the agent up, the game
/// authenticates silently. So every launch first ensures Battle.net is
/// running: start it if needed and wait for the agent to come up.
/// Best-effort Battle.net startup: spawn the Battle.net client if no
/// process is running, then wait (bounded) for it to register its agent.
/// Best-effort on purpose - if Battle.net can't start we still spawn the
/// game; it will show its own login as before rather than hard-failing.
fn ensure_battlenet_running() -> bool {
    // Preferred launch path: ask the running Battle.net app to press its own
    // Play button ("--exec=launch S2"). The game's SSO token is minted and
    // handed over inside Battle.net's process — never available to us — so
    // only the app itself can launch fully authenticated. Returns true when
    // the launch was delegated.
    if battlenet_running() {
        if let Some(exe) = find_battlenet_exe() {
            if Command::new(exe).args(["--exec=launch S2"]).spawn().is_ok() {
                return true;
            }
        }
    }
    false
}

fn find_battlenet_exe() -> Option<&'static std::path::Path> {
    // Local per-user install is a distinct candidate, so it cannot be 'static;
    // resolve among the static candidates only, and the per-user one via env.
    static CANDIDATES: [&str; 2] = [
        r"C:\Program Files (x86)\Battle.net\Battle.net.exe",
        r"C:\Program Files\Battle.net\Battle.net.exe",
    ];
    CANDIDATES
        .iter()
        .map(std::path::Path::new)
        .find(|p| p.is_file())
        .or_else(|| {
            let user = std::env::var("USERNAME").ok()?;
            let p = std::path::PathBuf::from(format!(
                r"C:\Users\{user}\AppData\Local\Battle.net\Battle.net.exe"
            ));
            p.is_file().then_some(Box::leak(p.into_boxed_path()))
        })
}

/// True when the Battle.net client (not the agent) is running.
#[cfg(windows)]
fn battlenet_running() -> bool {
    Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Battle.net.exe", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("Battle.net.exe"))
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn battlenet_running() -> bool {
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.chars().all(|c| c.is_ascii_digit())
                    && std::fs::read_to_string(e.path().join("comm"))
                        .map(|c| c.to_lowercase().contains("battle.net"))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// A just-closed game session lingers in the Agent's tracking for a few
/// seconds after the process exits; a relaunch inside that window crashes
/// SC2 at boot ("an error occurred starting StarCraft II").
const GAME_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(6);

#[tracing::instrument(skip_all, fields(exe = %layout.exe().display()))]
pub fn launch(layout: &WindowsLayout) -> Result<()> {
    let exe = layout.exe();
    if !exe.is_file() {
        return Err(pkg_err(exe.display().to_string(), "executable not found"));
    }
    // Race guard: wait out any running/closing session before delegating —
    // the Agent must see the game fully stopped or the new instance dies.
    // Bounded: a user keeping the game open gets an actionable error, not a
    // hung command thread.
    let mut observed_running = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while sc2_running() {
        observed_running = true;
        if std::time::Instant::now() > deadline {
            return Err(crate::error::Error::User(crate::UserError {
                message: "StarCraft II is still running — close it and retry".to_string(),
                path: None,
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // The grace only matters when a session was actually open moments ago.
    if observed_running {
        std::thread::sleep(GAME_SHUTDOWN_GRACE);
    }
    // Preferred: delegate to the running Battle.net app ("--exec=launch S2")
    // — it presses its own Play button, so the SSO token stays inside
    // Battle.net where it belongs. Fallback: spawn the game directly with
    // the switcher-style SSO arguments (works when the agent session is
    // warm; cold sessions show the game's own login page).
    if ensure_battlenet_running() {
        return Ok(());
    }
    // Battle.net absent: start it so at least the next launch is delegated,
    // then fall through to the direct spawn (best-effort as before).
    if let Some(exe) = find_battlenet_exe() {
        let _ = Command::new(exe).spawn();
        // Give the agent a moment, then retry the delegation once.
        for _ in 0..30 {
            if battlenet_running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if ensure_battlenet_running() {
            return Ok(());
        }
    }
    // Exactly what the Battle.net client passes (captured from a live
    // launch): silent SSO against the running agent plus product context.
    // Without these the game falls back to the legacy login page.
    let args = ["-sso=1", "-launch", "-uid", "s2"];
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new(&exe)
            .args(args)
            .creation_flags(0x0000_0008 | 0x0000_0400) // DETACHED_PROCESS | NEW_PROCESS_GROUP
            .spawn()
            .map_err(|e| pkg_err(exe.display().to_string(), e.to_string()))?;
    }
    #[cfg(not(windows))]
    {
        Command::new(&exe)
            .args(args)
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
