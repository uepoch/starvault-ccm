import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { listLibrary, repairActive } from "./ipc";
import Library from "./Library";
import type { LibrarySnapshot } from "./types";

vi.mock("./ipc", () => ({
  activatePackage: vi.fn(),
  editPackageMetadata: vi.fn(),
  listLibrary: vi.fn(),
  playPackage: vi.fn(),
  removePackage: vi.fn(),
  repairActive: vi.fn(),
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
      <Library pendingZip={null} onZipConsumed={vi.fn()} />
    </MantineProvider>,
  );
}

describe("Library", () => {
  beforeEach(() => {
    vi.mocked(listLibrary).mockResolvedValue(readySnapshot);
    vi.mocked(repairActive).mockResolvedValue(undefined);
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

  it("offers Repair when the backend marks a health issue repairable", async () => {
    vi.mocked(listLibrary).mockResolvedValue({
      ...readySnapshot,
      health: {
        state: "drifted",
        issues: [
          {
            code: "mods_drift",
            message: "Managed Mods differ from the ledger.",
            repairable: true,
          },
        ],
      },
    });
    renderLibrary();

    const repair = await screen.findByRole("button", { name: "Repair active campaign" });
    fireEvent.click(repair);
    await waitFor(() => expect(repairActive).toHaveBeenCalledOnce());
  });

  it("does not offer Repair when the backend marks a health issue non-repairable", async () => {
    vi.mocked(listLibrary).mockResolvedValue({
      ...readySnapshot,
      health: {
        state: "drifted",
        issues: [
          {
            code: "package_missing",
            message: "The active package is missing.",
            repairable: false,
          },
        ],
      },
    });
    renderLibrary();

    await screen.findByText("The active package is missing.");
    expect(screen.queryByRole("button", { name: "Repair active campaign" })).toBeNull();
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
            repairable: false,
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
});
