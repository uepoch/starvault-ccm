import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { listLibrary, restoreVanilla } from "./ipc";
import Library from "./Library";
import type { LibrarySnapshot } from "./types";

vi.mock("./ipc", () => ({
  activatePackage: vi.fn(),
  editPackageMetadata: vi.fn(),
  listLibrary: vi.fn(),
  playPackage: vi.fn(),
  removePackage: vi.fn(),
  restoreVanilla: vi.fn(),
  revealPackage: vi.fn(),
}));

vi.mock("./ImportWizard", () => ({
  default: ({ disabled }: { disabled?: boolean }) => (
    <button type="button" disabled={disabled}>
      Import package…
    </button>
  ),
}));

vi.mock("./MigrationBanner", () => ({ default: () => null }));

const readySnapshot: LibrarySnapshot = {
  entries: [
    {
      id: "active-campaign",
      revision: "1234567890abcdef",
      faction: "wol",
      title: "Active Campaign",
      author: "Author B",
      version: "1.0",
      desc: null,
      imported_at: 1_700_000_000,
    },
    {
      id: "inactive-campaign",
      revision: "fedcba0987654321",
      faction: "hots",
      title: "Inactive Campaign",
      author: "Author A",
      version: null,
      desc: null,
      imported_at: 1_600_000_000,
    },
  ],
  active_campaign: {
    id: "active-campaign",
    revision: "1234567890abcdef",
    faction: "wol",
  },
  health: { state: "ready", issues: [] },
};

function renderLibrary() {
  return render(
    <MantineProvider defaultColorScheme="dark">
      <Library openRequest={null} onRequestConsumed={vi.fn()} />
    </MantineProvider>,
  );
}

describe("Library", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(listLibrary).mockResolvedValue(readySnapshot);
  });

  it("shows one global active campaign and keeps Activate and Play separate", async () => {
    renderLibrary();

    await screen.findAllByText("Active Campaign");
    expect(screen.getByRole("button", { name: "Return to vanilla" })).toBeTruthy();
    expect((screen.getByRole("button", { name: "Active" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect((screen.getByRole("button", { name: "Activate" }) as HTMLButtonElement).disabled).toBe(
      false,
    );
    expect(screen.getAllByRole("button", { name: "Play" })).toHaveLength(3);
  });

  it("uses a real sorting button and updates aria-sort", async () => {
    renderLibrary();
    await screen.findByText("Inactive Campaign");

    const titleHeader = screen.getByRole("columnheader", { name: "Title" });
    expect(titleHeader.getAttribute("aria-sort")).toBe("none");
    fireEvent.click(within(titleHeader).getByRole("button", { name: "Title" }));
    await waitFor(() => expect(titleHeader.getAttribute("aria-sort")).toBe("ascending"));
  });

  it("directs active deployment drift to Return to vanilla", async () => {
    vi.mocked(listLibrary).mockResolvedValue({
      ...readySnapshot,
      health: {
        state: "drifted",
        issues: [
          {
            code: "mods_drift",
            message: "Managed Mods differ from the ledger.",
          },
        ],
      },
    });
    renderLibrary();

    await screen.findByText("Return to vanilla to discard the active deployment before retrying.");
  });

  it("blocks package mutations, including import, while recovery is required", async () => {
    vi.mocked(listLibrary).mockResolvedValue({
      ...readySnapshot,
      health: {
        state: "recovery_required",
        issues: [
          {
            code: "interrupted_operation",
            message: "An interrupted operation must be recovered.",
          },
        ],
      },
    });
    renderLibrary();

    await screen.findByText("Recovery required");
    expect(
      (screen.getByRole("button", { name: "Import package…" }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect((screen.getByRole("button", { name: "Activate" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
  });

  it("shows the issue path and repairs a provably owned orphan after confirmation", async () => {
    const orphaned: LibrarySnapshot = {
      ...readySnapshot,
      active_campaign: null,
      health: {
        state: "drifted",
        issues: [
          {
            code: "orphaned_starvault_campaign",
            message: "StarVault can safely restore the preserved vanilla campaign state",
            path: "C:\\StarCraft II\\Maps\\Campaign",
          },
        ],
      },
    };
    vi.mocked(listLibrary)
      .mockResolvedValueOnce(orphaned)
      .mockResolvedValueOnce({ ...orphaned, health: { state: "ready", issues: [] } });
    vi.mocked(restoreVanilla).mockResolvedValue(undefined);
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    renderLibrary();

    expect(await screen.findByText("C:\\StarCraft II\\Maps\\Campaign")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Repair vanilla state…" }));

    await waitFor(() => expect(restoreVanilla).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(listLibrary).toHaveBeenCalledTimes(2));
    expect(confirm).toHaveBeenCalledWith(
      "StarVault will restore the preserved campaign directory and will not delete unknown Mods. Continue?",
    );
  });

  it("rescans every warning but offers Repair only for an owned orphan", async () => {
    const warning: LibrarySnapshot = {
      ...readySnapshot,
      active_campaign: null,
      health: {
        state: "drifted",
        issues: [
          {
            code: "ambiguous_campaign_state",
            message: "Manual recovery is required.",
            path: "C:\\StarCraft II\\Maps\\Campaign.starvault-plain",
          },
        ],
      },
    };
    vi.mocked(listLibrary).mockResolvedValue(warning);
    renderLibrary();

    await screen.findByText("Manual recovery is required.");
    expect(screen.queryByRole("button", { name: "Repair vanilla state…" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    await waitFor(() => expect(listLibrary).toHaveBeenCalledTimes(2));
  });

  it("shows the mapped repair error", async () => {
    vi.mocked(listLibrary).mockResolvedValue({
      ...readySnapshot,
      active_campaign: null,
      health: {
        state: "drifted",
        issues: [
          {
            code: "orphaned_starvault_campaign",
            message: "Repair is available.",
          },
        ],
      },
    });
    vi.mocked(restoreVanilla).mockRejectedValue({
      kind: "environment",
      code: "game_running",
      message: "Close StarCraft II before repairing.",
      retryable: true,
    });
    vi.spyOn(window, "confirm").mockReturnValue(true);
    renderLibrary();

    fireEvent.click(await screen.findByRole("button", { name: "Repair vanilla state…" }));
    expect(await screen.findByText("Close StarCraft II before repairing.")).toBeTruthy();
  });
});
