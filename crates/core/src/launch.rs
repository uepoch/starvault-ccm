//! Detached game launch after the application workflow's integrated preflight.
//!
//! This module never mutates slots, Mods, saves, or the activation ledger.

use std::process::Command;

use crate::error::{pkg_err, Result};
use crate::layout::WindowsLayout;

/// Running-instance detection. The shipping Windows path fails closed when
/// process inspection itself is unavailable.
pub fn sc2_running() -> bool {
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
        // Process inspection is a safety gate. If Windows cannot answer,
        // mutations and launch fail closed as though the game were open.
        process_running("SC2_x64.exe").unwrap_or(true)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Ask a running Battle.net client to launch SC2 with its private SSO token.
/// Direct process launch remains the fallback when delegation is unavailable.
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
    process_running("Battle.net.exe").unwrap_or(false)
}

#[cfg(windows)]
fn process_running(expected_name: &str) -> Option<bool> {
    use std::mem::size_of;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    struct Snapshot(windows_sys::Win32::Foundation::HANDLE);

    impl Drop for Snapshot {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    unsafe {
        let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let snapshot = Snapshot(handle);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot.0, &mut entry) == 0 {
            return None;
        }
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case(expected_name)
            {
                return Some(true);
            }
            if Process32NextW(snapshot.0, &mut entry) == 0 {
                return (windows_sys::Win32::Foundation::GetLastError() == ERROR_NO_MORE_FILES)
                    .then_some(false);
            }
        }
    }
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
            return Err(crate::error::EnvironmentError::GameRunning.into());
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

#[cfg(all(test, windows))]
mod tests {
    use super::process_running;

    #[test]
    fn native_process_snapshot_finds_the_current_test_process() {
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_string_lossy();

        assert_eq!(process_running(&name), Some(true));
        assert_eq!(
            process_running("svccm-process-that-does-not-exist.exe"),
            Some(false)
        );
    }
}
