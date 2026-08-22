import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { Box, MantineProvider, createTheme, Tabs } from "@mantine/core";
import { Notifications, notifications } from "@mantine/notifications";
import "@mantine/core/styles.css";
import "@mantine/notifications/styles.css";
import Campaigns from "./Campaigns";
import ChangelogButton from "./ChangelogButton";
import Library from "./Library";
import Log from "./Log";
import Settings from "./Settings";

const theme = createTheme({
  primaryColor: "blue",
  defaultRadius: "sm",
});

interface ConfigDto {
  game_exe: string | null;
  strategy_override: string | null;
  crash_reports_opt_in: boolean;
}

export default function App() {
  const [tab, setTab] = useState<string | null>(null);
  const [pendingZip, setPendingZip] = useState<string | null>(null);

  useEffect(() => {
    // Crash recovery, then first-run install detection.
    invoke("reconcile").catch(() => {});
    invoke<ConfigDto>("get_config")
      .then((cfg) => {
        if (!cfg.game_exe) {
          invoke<string | null>("discover_game_exe")
            .then((found) => {
              if (found) {
                return invoke("save_config", {
                  gameExe: found,
                  strategyOverride: cfg.strategy_override,
                  crashReportsOptIn: cfg.crash_reports_opt_in,
                });
              }
            })
            .catch(() => {});
        }
      })
      .catch(() => {});
  }, []);

  // A campaign zip dropped on any view opens the import wizard.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type !== "drop") return;
        const zip = event.payload.paths.find((p) => p.toLowerCase().endsWith(".zip"));
        if (!zip) return;
        // Visible feedback: proves the drop reached the app.
        notifications.show({ color: "blue", message: `Importing ${zip.split(/[\\/]/).pop()}…` });
        setTab("library");
        // Prop, not event: Library may not be mounted yet when a drop
        // lands from another tab.
        setPendingZip(zip);
      })
      .then((fn) => {
        unlisten = fn;
      });
    return () => unlisten?.();
  }, []);

  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <Notifications position="top-right" />
      <Tabs value={tab ?? "library"} onChange={setTab} keepMounted={false}>
        <Tabs.List px="lg" pt="sm">
          <Tabs.Tab value="library">Library</Tabs.Tab>
          <Tabs.Tab value="campaigns">Campaigns</Tabs.Tab>
          <Tabs.Tab value="log">Log</Tabs.Tab>
          <Tabs.Tab value="settings">Settings</Tabs.Tab>
          <Box ml="auto">
            <ChangelogButton />
          </Box>
        </Tabs.List>

        <Tabs.Panel value="library">
          <Library pendingZip={pendingZip} onZipConsumed={() => setPendingZip(null)} />
        </Tabs.Panel>
        <Tabs.Panel value="campaigns">
          <Campaigns />
        </Tabs.Panel>
        <Tabs.Panel value="log">
          <Log />
        </Tabs.Panel>
        <Tabs.Panel value="settings">
          <Settings />
        </Tabs.Panel>
      </Tabs>
    </MantineProvider>
  );
}
