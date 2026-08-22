---
name: windows-interop
description: Run Windows commands (PowerShell, reg.exe, process inspection) from WSL against the real machine. Use when developing/debugging the Windows side of this app from WSL - inspecting live process command lines, registry, file associations, or driving Windows executables for tests.
---

# Windows interop from WSL

This project ships a Windows app but is developed in WSL. Windows is
directly reachable from the WSL shell - use it instead of guessing.

## Powershell

```
/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -Command "<cmd>"
```

Notes:

- Always `-NoProfile` (profiles pollute output and slow startup).
- Quote args with backslash-escaped inner quotes for filters:
  `"Get-CimInstance Win32_Process -Filter \"Name='SC2_x64.exe'\""`
- `powershell.exe` runs in the Windows session, so `~` and `$env:` resolve
  to Windows paths/vars, not WSL ones.

## Windows filesystem

- `C:\` mounts at `/mnt/c` (user profile: `/mnt/c/Users/<user>/`).
- Real app data is readable: `Documents\StarCraft II\...`, `%APPDATA%`,
  ProgramData. Prefer reading real files over web lore - see
  docs/design/research-*.md, which were built this way.

## Live process inspection (the capture trick)

To learn how a Windows app is "really" launched by its parent, launch it
normally (via its launcher/Battle.net/etc.) and capture the command line:

```
/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe -NoProfile -Command \
  "Get-CimInstance Win32_Process -Filter \"Name='SC2_x64.exe'\" | Select-Object -ExpandProperty CommandLine"
```

This single technique solved the launch-auth bug: the game spawns with
`-sso=1 -launch -uid s2` when Battle.net starts it; missing args = legacy
login page. Whenever behavior differs between "launched by X" and "launched
directly", capture X's actual arguments this way before theorizing.

Polling variant (wait for a process to appear, e.g. while the user starts
the game): loop the command with `sleep` between attempts.

## Registry

```
/mnt/c/Windows/System32/reg.exe query "HKCU\Software\Blizzard Entertainment" /s
```

## Spawning Windows exes for tests

Windows executables run directly from WSL (`"/mnt/c/.../prog.exe" args`),
including GUI apps; they detach from the WSL shell. Caveat: elevated-Windows
rules still apply (UIPI blocks some interactions across integrity levels -
e.g. drag-and-drop into an elevated app).

## When to use a subagent

For exploratory sweeps (enumerate dirs, dump headers, capture several
process command lines), hand the probe list to a subagent so raw output
stays out of the main thread. The `researcher` agent pattern in this repo's
history (docs/design/research-*.md) is the template: local probes first,
web second, findings filed as a dated brief.
