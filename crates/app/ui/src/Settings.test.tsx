import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { clearAllData, getConfig, getSavesStatus, listLibrary, saveConfig } from "./ipc";
import Settings from "./Settings";

vi.mock("@tauri-apps/api/app", () => ({ getVersion: vi.fn().mockResolvedValue("0.2.0") }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));
vi.mock("./ipc", () => ({
  clearAllData: vi.fn(),
  discoverGameExe: vi.fn(),
  getConfig: vi.fn(),
  getSavesStatus: vi.fn(),
  listLibrary: vi.fn(),
  saveConfig: vi.fn(),
}));

describe("Settings", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(getConfig).mockResolvedValue({
      game_exe: "C:\\StarCraft II\\StarCraft II.exe",
      strategy_override: null,
      crash_reports_opt_in: false,
      analytics_enabled: true,
      analytics_acknowledged: true,
      log_level: "info",
      save_isolation: true,
      saves_profile: "profile-a",
      replace_external_mods: false,
    });
    vi.mocked(getSavesStatus).mockResolvedValue({
      supported: true,
      reason: null,
      profiles: [
        { id: "profile-a", label: "Commander One" },
        { id: "profile-b", label: "Commander Two" },
      ],
      selected: "profile-a",
      enabled: true,
    });
    vi.mocked(listLibrary).mockResolvedValue({
      entries: [],
      active_campaign: {
        id: "active-campaign",
        revision: "revision",
        faction: "lotv",
      },
      health: { state: "ready", issues: [] },
    });
    vi.mocked(saveConfig).mockResolvedValue(undefined);
    vi.mocked(clearAllData).mockResolvedValue(undefined);
  });

  it("locks deployment and save settings while a campaign is active", async () => {
    render(
      <MantineProvider defaultColorScheme="dark">
        <Settings />
      </MantineProvider>,
    );

    await screen.findByText("Campaign settings are locked");
    expect((screen.getByLabelText("StarCraft II.exe") as HTMLInputElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Browse…" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(
      (screen.getByRole("button", { name: "Auto-detect" }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("combobox", { name: "Switch strategy" }) as HTMLInputElement).disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("switch", { name: /Save isolation \(Beta\)/ }) as HTMLInputElement)
        .disabled,
    ).toBe(true);
    const profile = screen.getByRole("combobox", { name: "Saves profile" }) as HTMLInputElement;
    expect(profile.disabled).toBe(true);
    expect(profile.value).toBe("Commander One");
  });

  it("locks deployment and save settings while recovery is required", async () => {
    vi.mocked(listLibrary).mockResolvedValueOnce({
      entries: [],
      active_campaign: null,
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
    render(
      <MantineProvider defaultColorScheme="dark">
        <Settings />
      </MantineProvider>,
    );

    await screen.findByText("Recovery is required");
    expect((screen.getByLabelText("StarCraft II.exe") as HTMLInputElement).disabled).toBe(true);
    expect(
      (screen.getByRole("combobox", { name: "Switch strategy" }) as HTMLInputElement).disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("switch", { name: /Save isolation \(Beta\)/ }) as HTMLInputElement)
        .disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("combobox", { name: "Saves profile" }) as HTMLInputElement).disabled,
    ).toBe(true);
  });

  it("cancels a pending autosave before clearing all data", async () => {
    const user = userEvent.setup();
    render(
      <MantineProvider defaultColorScheme="dark">
        <Settings />
      </MantineProvider>,
    );

    await screen.findByText("Campaign settings are locked");
    await user.click(screen.getByRole("switch", { name: /^Crash reports/ }));
    await user.click(screen.getByRole("button", { name: "Clear all data…" }));
    await user.click(await screen.findByRole("button", { name: "Delete everything" }));
    await waitFor(() => expect(clearAllData).toHaveBeenCalledTimes(1));
    await new Promise((resolve) => window.setTimeout(resolve, 750));
    expect(saveConfig).not.toHaveBeenCalled();
  });

  it("waits for an in-flight autosave before clearing all data", async () => {
    const user = userEvent.setup();
    let resolveSave: () => void = () => {
      throw new Error("save resolver was not initialized");
    };
    vi.mocked(saveConfig).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveSave = resolve;
        }),
    );
    render(
      <MantineProvider defaultColorScheme="dark">
        <Settings />
      </MantineProvider>,
    );

    await screen.findByText("Campaign settings are locked");
    await user.click(screen.getByRole("switch", { name: /^Crash reports/ }));
    await waitFor(() => expect(saveConfig).toHaveBeenCalledTimes(1), { timeout: 1_200 });
    await user.click(screen.getByRole("button", { name: "Clear all data…" }));
    await user.click(await screen.findByRole("button", { name: "Delete everything" }));
    expect(clearAllData).not.toHaveBeenCalled();

    resolveSave();
    await waitFor(() => expect(clearAllData).toHaveBeenCalledTimes(1));
  });
});
