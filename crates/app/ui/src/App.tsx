import { useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { MantineProvider, createTheme, Tabs } from "@mantine/core";
import { Notifications } from "@mantine/notifications";
import "@mantine/core/styles.css";
import "@mantine/notifications/styles.css";
import Campaigns from "./Campaigns";
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

  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <Notifications position="top-right" />
      <Tabs defaultValue="library" keepMounted={false}>
        <Tabs.List px="lg" pt="sm">
          <Tabs.Tab value="library">Library</Tabs.Tab>
          <Tabs.Tab value="campaigns">Campaigns</Tabs.Tab>
          <Tabs.Tab value="log">Log</Tabs.Tab>
          <Tabs.Tab value="settings">Settings</Tabs.Tab>
        </Tabs.List>

        <Tabs.Panel value="library">
          <Library />
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
