/// Wire shapes of the Rust command DTOs (serde emits snake_case).
/// Campaigns reads a subset of LibraryEntry; structural typing covers that.

export interface ConfigDto {
  game_exe: string | null;
  strategy_override: string | null;
  crash_reports_opt_in: boolean;
  log_level: string;
  save_isolation: boolean;
  saves_profile: string | null;
}

export interface SavesStatus {
  supported: boolean;
  reason: string | null;
  profiles: string[];
  selected: string | null;
  enabled: boolean;
}

export interface LibraryEntry {
  id: string;
  rev: string;
  slot: string;
  active_on: string[];
  title: string | null;
  author: string | null;
  version: string | null;
  desc: string | null;
  imported_at: number | null;
}
