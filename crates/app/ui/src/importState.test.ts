import { describe, expect, it } from "vite-plus/test";
import { importReducer, initialImportState } from "./importState";
import type { ImportPreview } from "./types";

const preview: ImportPreview = {
  suggested_id: "example-campaign",
  title: "Example",
  author: null,
  version: null,
  desc: null,
  slot_guess: "wol",
  matched_pattern: null,
  warnings: [],
  file_count: 12,
};

describe("importReducer", () => {
  it("follows the backend operation states through completion", () => {
    const analyzing = importReducer(initialImportState, { type: "analyze", opId: "op-1" });
    expect(analyzing.state).toBe("Analyzing");

    const ready = importReducer(analyzing, { type: "ready", preview });
    expect(ready.state).toBe("Ready");
    expect(ready.preview).toBe(preview);

    const ingesting = importReducer(ready, { type: "ingest" });
    expect(ingesting.state).toBe("Ingesting");

    const completed = importReducer(ingesting, { type: "completed", revision: "abc123" });
    expect(completed.state).toBe("Completed");
    expect(completed.revision).toBe("abc123");
  });

  it("retains a preview for retry after failure and resets terminal state", () => {
    const ready = importReducer(
      importReducer(initialImportState, { type: "analyze", opId: "op-2" }),
      { type: "ready", preview },
    );
    const failed = importReducer(ready, {
      type: "failed",
      error: {
        kind: "package",
        code: "archive_invalid",
        message: "Invalid archive",
        retryable: true,
      },
    });
    expect(failed.state).toBe("Failed");
    expect(failed.preview).toBe(preview);
    expect(failed.error?.code).toBe("archive_invalid");
    expect(importReducer(failed, { type: "reset" })).toEqual(initialImportState);
  });
});
