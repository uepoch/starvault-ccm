import type { CommandError } from "./types";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function parseError(value: unknown): unknown {
  if (typeof value !== "string") return value;
  try {
    return JSON.parse(value) as unknown;
  } catch {
    return value;
  }
}

export function isCommandError(value: unknown): value is CommandError {
  const parsed = parseError(value);
  return (
    isRecord(parsed) &&
    ["user", "package", "environment", "internal"].includes(String(parsed.kind)) &&
    typeof parsed.code === "string" &&
    typeof parsed.message === "string" &&
    typeof parsed.retryable === "boolean"
  );
}

export function toCommandError(value: unknown): CommandError {
  const parsed = parseError(value);
  if (isCommandError(parsed)) return parsed;
  if (isRecord(parsed) && typeof parsed.message === "string") {
    return {
      kind: "internal",
      code: "unstructured_error",
      message: parsed.message,
      retryable: false,
    };
  }
  return {
    kind: "internal",
    code: "unstructured_error",
    message: typeof parsed === "string" ? parsed : String(parsed),
    retryable: false,
  };
}

export function errMessage(value: unknown): string {
  return toCommandError(value).message;
}
