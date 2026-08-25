import { toCommandError } from "./errors";
import type {
  CommandError,
  ImportOperationState,
  ImportPreview,
  ImportProgressPhase,
} from "./types";

export type ImportUiState = "Idle" | ImportOperationState;

export interface ImportState {
  state: ImportUiState;
  opId: string | null;
  preview: ImportPreview | null;
  progress: number;
  phase: ImportProgressPhase | null;
  revision: string | null;
  error: CommandError | null;
}

export const initialImportState: ImportState = {
  state: "Idle",
  opId: null,
  preview: null,
  progress: 0,
  phase: null,
  revision: null,
  error: null,
};

export type ImportAction =
  | { type: "analyze"; opId: string }
  | { type: "ready"; preview: ImportPreview }
  | { type: "ingest" }
  | { type: "progress"; phase: ImportProgressPhase; value: number }
  | { type: "cancelled" }
  | { type: "failed"; error: unknown }
  | { type: "completed"; revision: string }
  | { type: "reset" };

export function importReducer(state: ImportState, action: ImportAction): ImportState {
  switch (action.type) {
    case "analyze":
      return { ...initialImportState, state: "Analyzing", opId: action.opId };
    case "ready":
      return { ...state, state: "Ready", preview: action.preview, progress: 100, error: null };
    case "ingest":
      return { ...state, state: "Ingesting", phase: "ingest", progress: 0, error: null };
    case "progress":
      return {
        ...state,
        phase: action.phase,
        progress: Math.max(0, Math.min(100, action.value)),
      };
    case "cancelled":
      return { ...state, state: "Cancelled", error: null };
    case "failed":
      return { ...state, state: "Failed", error: toCommandError(action.error) };
    case "completed":
      return { ...state, state: "Completed", revision: action.revision, progress: 100 };
    case "reset":
      return initialImportState;
  }
}

export function importIsTerminal(state: ImportUiState): boolean {
  return state === "Idle" || state === "Cancelled" || state === "Failed" || state === "Completed";
}
