/** Wire shapes owned by svccm-core. Serde emits snake_case fields. */

export interface ConfigDto {
  game_exe: string | null;
  strategy_override: string | null;
  crash_reports_opt_in: boolean;
  log_level: string;
  save_isolation: boolean;
  saves_profile: string | null;
  replace_external_mods: boolean;
}

export interface SavesProfile {
  id: string;
  label: string;
}

export interface SavesStatus {
  supported: boolean;
  reason: string | null;
  profiles: SavesProfile[];
  selected: string | null;
  enabled: boolean;
}

export interface ActiveCampaign {
  id: string;
  revision: string;
  faction: string;
}

export type HealthState = "ready" | "drifted" | "recovery_required";

export interface HealthIssue {
  code: string;
  message: string;
  path?: string;
  repairable: boolean;
}

export interface Health {
  state: HealthState;
  issues: HealthIssue[];
}

export interface LibraryEntry {
  id: string;
  revision: string;
  faction: string;
  title: string | null;
  author: string | null;
  version: string | null;
  desc: string | null;
  imported_at: number | null;
}

export interface LibrarySnapshot {
  entries: LibraryEntry[];
  active_campaign: ActiveCampaign | null;
  health: Health;
}

export interface StartupReport {
  library: LibrarySnapshot;
  recovery_performed: boolean;
  notes: string[];
}

export type CommandErrorKind = "user" | "package" | "environment" | "internal";

export interface CommandError {
  kind: CommandErrorKind;
  code: string;
  message: string;
  path?: string;
  retryable: boolean;
  report_id?: string;
}

export interface ImportPreview {
  suggested_id: string;
  title: string | null;
  author: string | null;
  version: string | null;
  desc: string | null;
  slot_guess: string;
  matched_pattern: string | null;
  warnings: string[];
  file_count: number;
}

export type ImportOperationState =
  | "Analyzing"
  | "Ready"
  | "Ingesting"
  | "Cancelled"
  | "Failed"
  | "Completed";

export interface ImportOperationSnapshot {
  op_id: string;
  state: ImportOperationState;
  preview?: ImportPreview;
  revision?: string;
  error_code?: string;
}

export interface MigrationCandidate {
  candidate_id: string;
  name: string;
}
