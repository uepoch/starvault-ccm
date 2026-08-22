/// Tauri commands fail either with a plain string or a serialized
/// CommandError { message, conflict? }. Normalize both.

export interface ConflictInfo {
  target: string;
  conflict_count: number;
  other_id: string;
  other_slot: string;
}

interface CommandErrorPayload {
  message?: string;
  conflict?: ConflictInfo;
}

export function errMessage(e: unknown): string {
  if (typeof e === "string") return e;
  const o = e as CommandErrorPayload;
  return o?.message ?? String(e);
}

export function errConflict(e: unknown): ConflictInfo | null {
  if (typeof e === "object" && e !== null) {
    const o = e as CommandErrorPayload;
    if (o.conflict) return o.conflict;
  }
  return null;
}
