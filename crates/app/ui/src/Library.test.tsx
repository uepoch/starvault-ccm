import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { listLibrary } from "./ipc";
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
      <Library pendingZip={null} onZipConsumed={vi.fn()} />
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
});
