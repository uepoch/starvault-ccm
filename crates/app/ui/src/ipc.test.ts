import { invoke } from "@tauri-apps/api/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { importApi, migrateCandidate } from "./ipc";
import type { ImportOperationSnapshot } from "./types";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

describe("migration IPC", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(invoke).mockResolvedValue("revision");
  });

  it("identifies a backend candidate without sending a source path", async () => {
    await migrateCandidate("candidate-17", "legacy-campaign", "hots");

    expect(invoke).toHaveBeenCalledWith("migrate_candidate", {
      candidateId: "candidate-17",
      id: "legacy-campaign",
      faction: "hots",
    });
    const args = vi.mocked(invoke).mock.calls[0]?.[1];
    expect(args).not.toHaveProperty("path");
  });
});

describe("import IPC", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("returns the backend-owned operation snapshot without fabricating state", async () => {
    const snapshot: ImportOperationSnapshot = {
      op_id: "operation-1",
      state: "Ready",
      preview: {
        suggested_id: "campaign",
        title: null,
        author: null,
        version: null,
        desc: null,
        slot_guess: "wol",
        matched_pattern: null,
        warnings: [],
        file_count: 1,
      },
    };
    vi.mocked(invoke).mockResolvedValueOnce(snapshot);

    const result = await importApi.analyze("operation-1", "C:\\campaign.zip");

    expect(result).toBe(snapshot);
    expect(invoke).toHaveBeenCalledWith("import_analyze", {
      opId: "operation-1",
      path: "C:\\campaign.zip",
    });
  });

  it("passes through the backend-owned terminal ingest snapshot", async () => {
    const snapshot: ImportOperationSnapshot = {
      op_id: "operation-2",
      state: "Completed",
      revision: "revision-2",
    };
    vi.mocked(invoke).mockResolvedValueOnce(snapshot);

    const result = await importApi.ingest({
      opId: "operation-2",
      id: "campaign",
      faction: "hots",
      title: "Campaign",
      author: null,
      version: null,
      desc: null,
    });

    expect(result).toBe(snapshot);
    expect(invoke).toHaveBeenCalledWith("import_ingest", {
      opId: "operation-2",
      id: "campaign",
      slot: "hots",
      meta: {
        title: "Campaign",
        author: null,
        version: null,
        desc: null,
      },
    });
  });
});
