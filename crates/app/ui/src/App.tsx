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

export default function App() {
  useEffect(() => {
    // Crash recovery before anything renders state.
    invoke("reconcile").catch(() => {});
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
