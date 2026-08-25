import { act, render, screen, waitFor } from "@testing-library/react";
import { listen } from "@tauri-apps/api/event";
import userEvent from "@testing-library/user-event";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { importApi } from "./ipc";
import ImportWizard from "./ImportWizard";
import type { ImportOperationSnapshot } from "./types";

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(vi.fn()),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn().mockResolvedValue("C:\\Downloads\\campaign.zip"),
}));

vi.mock("./ipc", () => ({
  activatePackage: vi.fn(),
  importApi: {
    analyze: vi.fn(),
    analyzeTranslator: vi.fn(),
    ingest: vi.fn(),
    cancel: vi.fn(),
  },
}));

describe("ImportWizard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(importApi.analyze).mockResolvedValue({
      op_id: "op-1",
      state: "Ready",
      preview: {
        suggested_id: "active-campaign",
        title: "Campaign",
        author: null,
        version: null,
        desc: null,
        slot_guess: "wol",
        matched_pattern: null,
        warnings: [],
        file_count: 8,
      },
    });
    vi.mocked(importApi.cancel).mockResolvedValue(undefined);
  });

  it("blocks active-package replacement and cancels cleanup when closed", async () => {
    const user = userEvent.setup();
    render(
      <MantineProvider defaultColorScheme="dark">
        <ImportWizard
          knownIds={new Set(["active-campaign"])}
          activePackageId="active-campaign"
          onImported={vi.fn()}
          pendingSource={null}
          onSourceConsumed={vi.fn()}
        />
      </MantineProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Import package…" }));
    await user.click(await screen.findByRole("button", { name: "Choose campaign zip…" }));

    await screen.findByText("Return to vanilla first");
    expect(
      (screen.getByRole("button", { name: /Ingest \(8 files\)/ }) as HTMLButtonElement).disabled,
    ).toBe(true);

    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(importApi.cancel).toHaveBeenCalledWith(expect.any(String)));
  });

  it("re-analyzes the retained archive with a fresh operation after ingest failure", async () => {
    const user = userEvent.setup();
    vi.mocked(importApi.ingest).mockRejectedValueOnce({
      kind: "environment",
      code: "write_failed",
      message: "The package could not be written.",
      retryable: true,
    });
    render(
      <MantineProvider defaultColorScheme="dark">
        <ImportWizard
          knownIds={new Set()}
          activePackageId={null}
          onImported={vi.fn()}
          pendingSource={null}
          onSourceConsumed={vi.fn()}
        />
      </MantineProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Import package…" }));
    await user.click(await screen.findByRole("button", { name: "Choose campaign zip…" }));
    await user.click(await screen.findByRole("button", { name: "Ingest (8 files)" }));
    await screen.findByText("The package could not be written.");
    expect(screen.queryByRole("button", { name: /Retry ingest/ })).toBeNull();

    const firstOperation = vi.mocked(importApi.analyze).mock.calls[0][0];
    const firstArchive = vi.mocked(importApi.analyze).mock.calls[0][1];
    await user.click(screen.getByRole("button", { name: "Analyze again" }));
    await waitFor(() => expect(importApi.analyze).toHaveBeenCalledTimes(2));
    expect(vi.mocked(importApi.analyze).mock.calls[1][0]).not.toBe(firstOperation);
    expect(vi.mocked(importApi.analyze).mock.calls[1][1]).toBe(firstArchive);
    await screen.findByRole("button", { name: "Ingest (8 files)" });
  });

  it("ignores an analysis result that arrives after the wizard closes", async () => {
    const user = userEvent.setup();
    let resolveAnalysis!: (value: ImportOperationSnapshot) => void;
    const analysis = new Promise<ImportOperationSnapshot>((resolve) => {
      resolveAnalysis = resolve;
    });
    vi.mocked(importApi.analyze).mockReturnValueOnce(analysis);
    render(
      <MantineProvider defaultColorScheme="dark">
        <ImportWizard
          knownIds={new Set()}
          activePackageId={null}
          onImported={vi.fn()}
          pendingSource={null}
          onSourceConsumed={vi.fn()}
        />
      </MantineProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Import package…" }));
    await user.click(await screen.findByRole("button", { name: "Choose campaign zip…" }));
    await screen.findByText("Analyzing package…");
    await user.click(screen.getByRole("button", { name: "Close import wizard" }));
    resolveAnalysis({
      op_id: "stale-operation",
      state: "Ready",
      preview: {
        suggested_id: "stale-campaign",
        title: "Stale campaign",
        author: null,
        version: null,
        desc: null,
        slot_guess: "wol",
        matched_pattern: null,
        warnings: [],
        file_count: 1,
      },
    });

    await user.click(screen.getByRole("button", { name: "Import package…" }));
    await screen.findByRole("button", { name: "Choose campaign zip…" });
    expect(screen.queryByText("Stale campaign")).toBeNull();
  });

  it("keeps an actionable wizard open when explicit close cleanup fails", async () => {
    const user = userEvent.setup();
    vi.mocked(importApi.cancel).mockRejectedValueOnce({
      kind: "environment",
      code: "cleanup_import_scratch",
      message: "The import scratch directory is locked.",
      retryable: true,
    });
    render(
      <MantineProvider defaultColorScheme="dark">
        <ImportWizard
          knownIds={new Set()}
          activePackageId={null}
          onImported={vi.fn()}
          pendingSource={null}
          onSourceConsumed={vi.fn()}
        />
      </MantineProvider>,
    );

    await user.click(screen.getByRole("button", { name: "Import package…" }));
    await user.click(await screen.findByRole("button", { name: "Choose campaign zip…" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));

    await screen.findByText("Import cleanup failed");
    expect(screen.getByText("The import scratch directory is locked.")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Retry cleanup and close" }));
    await waitFor(() => expect(importApi.cancel).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.queryByText("Import cleanup failed")).toBeNull());
  });
  it("downloads an approved translator source before showing the normal preview", async () => {
    let resolveAnalysis!: (value: ImportOperationSnapshot) => void;
    const analysis = new Promise<ImportOperationSnapshot>((resolve) => {
      resolveAnalysis = resolve;
    });
    vi.mocked(importApi.analyzeTranslator).mockReturnValueOnce(analysis);
    render(
      <MantineProvider defaultColorScheme="dark">
        <ImportWizard
          knownIds={new Set()}
          activePackageId={null}
          onImported={vi.fn()}
          pendingSource={{
            kind: "translator",
            instanceId: "upload-wpRtPJWdAa",
            expectedSize: 538_740_099,
          }}
          onSourceConsumed={vi.fn()}
        />
      </MantineProvider>,
    );

    await waitFor(() =>
      expect(importApi.analyzeTranslator).toHaveBeenCalledWith(
        expect.any(String),
        "upload-wpRtPJWdAa",
        538_740_099,
      ),
    );
    expect(importApi.analyze).not.toHaveBeenCalled();
    const listener = vi.mocked(listen).mock.calls[0][1];
    act(() => {
      listener({
        payload: {
          op_id: vi.mocked(importApi.analyzeTranslator).mock.calls[0][0],
          phase: "download",
          completed: 100,
          total: 538_740_099,
        },
      } as never);
    });
    expect(await screen.findByText("Downloading campaign…")).toBeTruthy();

    await act(async () => {
      resolveAnalysis({
        op_id: "translator-operation",
        state: "Ready",
        preview: {
          suggested_id: "translated-campaign",
          title: "Translated campaign",
          author: null,
          version: null,
          desc: null,
          slot_guess: "wol",
          matched_pattern: null,
          warnings: [],
          file_count: 8,
        },
      });
    });
    expect(await screen.findByDisplayValue("Translated campaign")).toBeTruthy();
  });
});
