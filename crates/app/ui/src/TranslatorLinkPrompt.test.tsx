import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MantineProvider } from "@mantine/core";
import { beforeEach, describe, expect, it, vi } from "vite-plus/test";
import { resolveTranslatorLink } from "./ipc";
import TranslatorLinkPrompt from "./TranslatorLinkPrompt";

vi.mock("./ipc", () => ({ resolveTranslatorLink: vi.fn() }));

const INSTANCE_ID = "upload-wpRtPJWdAa";

function renderPrompt(overrides: Partial<React.ComponentProps<typeof TranslatorLinkPrompt>> = {}) {
  const props = {
    instanceId: INSTANCE_ID,
    disabled: false,
    onDismiss: vi.fn(),
    onDownload: vi.fn(),
    onActivate: vi.fn(),
    ...overrides,
  };
  render(
    <MantineProvider defaultColorScheme="dark">
      <TranslatorLinkPrompt {...props} />
    </MantineProvider>,
  );
  return props;
}

describe("TranslatorLinkPrompt", () => {
  beforeEach(() => vi.clearAllMocks());

  it("confirms the server filename and decimal size before approving a download", async () => {
    vi.mocked(resolveTranslatorLink).mockResolvedValue({
      kind: "download",
      filename: "Project UED (WOL) 3.0.3.zip",
      size: 538_740_099,
    });
    const user = userEvent.setup();
    const declined = renderPrompt();

    expect(
      await screen.findByText(
        "You're going to download Project UED (WOL) 3.0.3.zip (538.7 MB). Do you want to continue?",
      ),
    ).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "No" }));
    expect(declined.onDismiss).toHaveBeenCalledOnce();
    expect(declined.onDownload).not.toHaveBeenCalled();

    const approved = renderPrompt();
    await screen.findAllByText(/You're going to download/);
    await user.click(screen.getAllByRole("button", { name: "Yes" }).at(-1)!);
    expect(approved.onDownload).toHaveBeenCalledWith({
      kind: "translator",
      instanceId: INSTANCE_ID,
      expectedSize: 538_740_099,
    });
  });

  it("activates an installed campaign without approving a download", async () => {
    vi.mocked(resolveTranslatorLink).mockResolvedValue({
      kind: "installed",
      package_id: "project-ued",
      title: "Project UED",
      active: false,
    });
    const user = userEvent.setup();
    const props = renderPrompt();

    await screen.findByText("Project UED is already installed. Do you want to activate it?");
    await user.click(screen.getByRole("button", { name: "Yes" }));
    expect(props.onActivate).toHaveBeenCalledWith({ id: "project-ued", title: "Project UED" });
    expect(props.onDownload).not.toHaveBeenCalled();
  });

  it("offers only OK when the installed campaign is already active", async () => {
    vi.mocked(resolveTranslatorLink).mockResolvedValue({
      kind: "installed",
      package_id: "project-ued",
      title: null,
      active: true,
    });
    renderPrompt();

    await screen.findByText("project-ued is already installed and active.");
    expect(screen.getByRole("button", { name: "OK" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Yes" })).toBeNull();
    expect(screen.queryByRole("button", { name: "No" })).toBeNull();
  });
});
