import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { importApi } from "./ipc";
import ImportWizard from "./ImportWizard";

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
    ingest: vi.fn(),
    cancel: vi.fn(),
  },
  repairActive: vi.fn(),
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
          pendingZip={null}
          onZipConsumed={vi.fn()}
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
          pendingZip={null}
          onZipConsumed={vi.fn()}
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
    let resolveAnalysis: (value: Awaited<ReturnType<typeof importApi.analyze>>) => void = () => {
      throw new Error("analysis resolver was not initialized");
    };
    vi.mocked(importApi.analyze).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveAnalysis = resolve;
        }),
    );
    render(
      <MantineProvider defaultColorScheme="dark">
        <ImportWizard
          knownIds={new Set()}
          activePackageId={null}
          onImported={vi.fn()}
          pendingZip={null}
          onZipConsumed={vi.fn()}
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
          pendingZip={null}
          onZipConsumed={vi.fn()}
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
});
